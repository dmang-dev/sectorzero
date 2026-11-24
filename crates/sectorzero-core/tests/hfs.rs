//! HFS and HFS+ volumes: create, write, read back.
//!
//! Every change is followed by `Volume::hfs_check` (catalog order and threads, valences,
//! counts, block ownership against the allocation bitmap). The QEMU harness mounts the same
//! kind of images with Linux's hfs and hfsplus drivers (see RESEARCH.md).

use std::fs;
use std::path::{Path, PathBuf};

use sectorzero_core::{
    CreateOptions, EntryEdit, FloppyFormat, ForkMode, FsKind, Image, MediaFormat, NewImage,
    OpenOptions, PartitionScheme, Volume,
};

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}

fn opts(fs: FsKind) -> CreateOptions {
    CreateOptions {
        fs,
        label: Some("Mac Disk".into()),
        ..Default::default()
    }
}

fn clean(img: &Path) {
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let problems = v.hfs_check().unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
}

fn sample_tree(dir: &Path, files: usize) -> PathBuf {
    let src = dir.join("tree");
    fs::create_dir_all(src.join("Sub Folder/Deeper")).unwrap();
    fs::write(src.join("Read Me"), b"hello from SectorZero").unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 253) as u8).collect();
    fs::write(src.join("Sub Folder/big data.bin"), big).unwrap();
    fs::write(src.join("Sub Folder/Deeper/note.md"), b"# deep").unwrap();
    fs::write(src.join("empty"), b"").unwrap();
    fs::write(src.join("Café Größe"), b"accents").unwrap();
    for i in 0..files {
        fs::write(
            src.join(format!("Sub Folder/file {i:04}.txt")),
            format!("{i}"),
        )
        .unwrap();
    }
    src
}

fn same_tree(a: &Path, b: &Path) {
    for e in fs::read_dir(a).unwrap() {
        let e = e.unwrap();
        let other = b.join(e.file_name());
        if e.path().is_dir() {
            same_tree(&e.path(), &other);
        } else {
            assert_eq!(
                fs::read(e.path()).unwrap(),
                fs::read(&other).unwrap(),
                "{}",
                other.display()
            );
        }
    }
}

fn free(img: &Path) -> u64 {
    Volume::open(img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap()
        .free_bytes
}

fn round_trip(img: &Path, tmp: &Path, files: usize) {
    let src = sample_tree(tmp, files);
    let before = free(img);
    {
        let v = Volume::open(img, &rw()).unwrap();
        let s = v.inject(&src, "/", false).unwrap();
        assert_eq!(s.files as usize, 5 + files);
        v.close().unwrap();
    }
    clean(img);
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let names: Vec<String> = v
        .list("/tree")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["Sub Folder", "Café Größe", "empty", "Read Me"]);
    assert_eq!(
        v.stat("/TREE/read me").unwrap().unwrap().size,
        21,
        "case-blind lookup"
    );
    assert!(
        v.stat("/tree/CAFÉ GRÖSSE").unwrap().is_none()
            || v.stat("/tree/café größe").unwrap().is_some()
    );
    let out = tmp.join("out");
    v.extract("/", &out, false).unwrap();
    same_tree(&src, &out.join("tree"));
    drop(v);

    {
        let v = Volume::open(img, &rw()).unwrap();
        v.rename("/tree/Read Me", "/tree/Read Me Too").unwrap();
        v.rename("/tree/empty", "/tree/EMPTY").unwrap();
        v.rename("/tree/Sub Folder/Deeper", "/moved").unwrap();
        assert!(
            v.rename("/tree", "/tree/Sub Folder/x").is_err(),
            "into itself"
        );
        v.rename("/moved", "/tree/Sub Folder").unwrap();
        v.mkdir("/a/b/c").unwrap();
        fs::write(tmp.join("readme.txt"), b"second version").unwrap();
        v.inject(&tmp.join("readme.txt"), "/a/b", false).unwrap();
        assert!(v.inject(&tmp.join("readme.txt"), "/a/b", false).is_err());
        fs::write(tmp.join("readme.txt"), b"third").unwrap();
        v.inject(&tmp.join("readme.txt"), "/a/b", true).unwrap();
        v.remove("/tree/Sub Folder/file 0007.txt", false).unwrap();
        assert!(v.remove("/a", false).is_err(), "not empty");
        assert!(v.mkdir("/tree/Read Me Too/x").is_err(), "a file in the way");
        v.close().unwrap();
    }
    clean(img);
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let mut buf = Vec::new();
    v.copy_out("/a/b/readme.txt", &mut buf).unwrap();
    assert_eq!(buf, b"third");
    assert_eq!(
        v.stat("/tree/Sub Folder/moved/note.md")
            .unwrap()
            .unwrap()
            .size,
        6
    );
    assert!(v.stat("/moved").unwrap().is_none());
    assert!(v.stat("/tree/Read Me").unwrap().is_none());
    assert_eq!(v.stat("/tree/empty").unwrap().unwrap().name, "EMPTY");
    assert_eq!(v.list("/tree/Sub Folder").unwrap().len(), files + 1);
    drop(v);

    {
        let v = Volume::open(img, &rw()).unwrap();
        for e in v.list("/").unwrap() {
            v.remove(&e.path, true).unwrap();
        }
        assert!(v.list("/").unwrap().is_empty());
        v.close().unwrap();
    }
    clean(img);
    // The catalog keeps the room it grew by (as on a Mac); a second fill and emptying
    // must give exactly the same free space.
    let emptied = free(img);
    assert!(emptied <= before);
    {
        let v = Volume::open(img, &rw()).unwrap();
        v.inject(&src, "/", true).unwrap();
        v.remove("/tree", true).unwrap();
        v.close().unwrap();
    }
    clean(img);
    assert_eq!(free(img), emptied, "blocks leaked");
}

#[test]
fn hfs_floppy() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("mac.img");
    Volume::create(
        &img,
        NewImage::Floppy(FloppyFormat::find("1440").unwrap()),
        &opts(FsKind::Hfs),
    )
    .unwrap();
    let info = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    assert_eq!(info.fs_type, "HFS");
    assert!(info.fs_writable, "{:?}", info.read_only_reason);
    clean(&img);
    round_trip(&img, tmp.path(), 40);
}

#[test]
fn hfs_plus_grows_its_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("plus.img");
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 40 << 20,
        },
        &opts(FsKind::HfsPlus),
    )
    .unwrap();
    clean(&img);
    // 1500 files: far more than the catalog's first clump, several index levels.
    round_trip(&img, tmp.path(), 1500);
}

#[test]
fn hfs_on_an_apple_partition_map() {
    let tmp = tempfile::tempdir().unwrap();
    for fs_kind in [FsKind::Hfs, FsKind::HfsPlus] {
        let img = tmp.path().join(format!("zip-{fs_kind:?}.img"));
        Volume::create(
            &img,
            NewImage::Media(MediaFormat::find("zip100").unwrap()),
            &opts(fs_kind),
        )
        .unwrap();
        clean(&img);
        let work = tmp.path().join(format!("w-{fs_kind:?}"));
        fs::create_dir_all(&work).unwrap();
        round_trip(&img, &work, 200);
    }
}

#[test]
fn fragmented_files_use_overflow_extents() {
    let tmp = tempfile::tempdir().unwrap();
    for fs_kind in [FsKind::Hfs, FsKind::HfsPlus] {
        let img = tmp.path().join(format!("frag-{fs_kind:?}.img"));
        let spec = match fs_kind {
            FsKind::Hfs => NewImage::Floppy(FloppyFormat::find("1440").unwrap()),
            _ => NewImage::Volume {
                size_bytes: 8 << 20,
            },
        };
        Volume::create(&img, spec, &opts(fs_kind)).unwrap();
        let src = tmp.path().join(format!("small-{fs_kind:?}"));
        fs::create_dir_all(&src).unwrap();
        for i in 0..60 {
            fs::write(src.join(format!("f{i:02}")), vec![i as u8; 5000]).unwrap();
        }
        let v = Volume::open(&img, &rw()).unwrap();
        v.inject(&src, "/", false).unwrap();
        // Take the rest of the volume (minus a little for the catalog), then punch holes.
        let filler = tmp.path().join("filler");
        fs::write(&filler, vec![0xEE; (free(&img) - 16 * 1024) as usize]).unwrap();
        v.inject(&filler, "/", false).unwrap();
        for i in (0..60).step_by(2) {
            v.remove(&format!("/small-{fs_kind:?}/f{i:02}"), false)
                .unwrap();
        }
        // Fills the holes: one fork in 20+ pieces, most of them in overflow records.
        let fill = 20 * 5120;
        let data: Vec<u8> = (0..fill as u32).map(|i| (i % 251) as u8).collect();
        let big = tmp.path().join("pieces");
        fs::write(&big, &data).unwrap();
        v.inject(&big, "/", false).unwrap();
        drop(v);
        clean(&img);
        let v = Volume::open(&img, &OpenOptions::default()).unwrap();
        let mut back = Vec::new();
        v.copy_out("/pieces", &mut back).unwrap();
        assert!(back == data, "{fs_kind:?}: fragmented file differs");
        drop(v);
        let v = Volume::open(&img, &rw()).unwrap();
        v.remove("/pieces", false).unwrap();
        drop(v);
        clean(&img);
    }
}

#[test]
fn forks_go_in_and_come_out() {
    let tmp = tempfile::tempdir().unwrap();
    for fs_kind in [FsKind::Hfs, FsKind::HfsPlus] {
        let img = tmp.path().join(format!("forks-{fs_kind:?}.img"));
        Volume::create(
            &img,
            NewImage::Floppy(FloppyFormat::find("1440").unwrap()),
            &opts(fs_kind),
        )
        .unwrap();
        // An AppleDouble pair, as macOS leaves on FAT disks.
        let src = tmp.path().join(format!("src-{fs_kind:?}"));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Letter"), b"Dear reader").unwrap();
        let rsrc: Vec<u8> = (0..3000u32).map(|i| (i % 7) as u8).collect();
        let mut ad = vec![0u8; 50];
        ad[..4].copy_from_slice(&0x0005_1607u32.to_be_bytes());
        ad[4..8].copy_from_slice(&0x0002_0000u32.to_be_bytes());
        ad[24..26].copy_from_slice(&2u16.to_be_bytes());
        for (i, (id, off, len)) in [(9u32, 50u32, 32u32), (2, 82, rsrc.len() as u32)]
            .into_iter()
            .enumerate()
        {
            let at = 26 + 12 * i;
            ad[at..at + 4].copy_from_slice(&id.to_be_bytes());
            ad[at + 4..at + 8].copy_from_slice(&off.to_be_bytes());
            ad[at + 8..at + 12].copy_from_slice(&len.to_be_bytes());
        }
        let mut finder = [0u8; 32];
        finder[..8].copy_from_slice(b"TEXTttxt");
        ad.extend(finder);
        ad.extend(&rsrc);
        fs::write(src.join("._Letter"), &ad).unwrap();
        let v = Volume::open(&img, &rw()).unwrap();
        v.inject_with(&src, "/", false, ForkMode::AppleDouble)
            .unwrap();
        let e = v
            .stat(&format!("/src-{fs_kind:?}/Letter"))
            .unwrap()
            .unwrap();
        let mac = e.mac.unwrap();
        assert_eq!(
            (
                mac.file_type.as_str(),
                mac.creator.as_str(),
                mac.resource_size
            ),
            ("TEXT", "ttxt", 3000)
        );
        assert!(
            v.stat(&format!("/src-{fs_kind:?}/._Letter"))
                .unwrap()
                .is_none()
        );
        // Out as MacBinary, back in as MacBinary under another folder.
        let mb = tmp.path().join(format!("mb-{fs_kind:?}"));
        v.extract_with(
            &format!("/src-{fs_kind:?}"),
            &mb,
            false,
            ForkMode::MacBinary,
        )
        .unwrap();
        v.inject_with(
            &mb.join(format!("src-{fs_kind:?}")),
            "/again",
            false,
            ForkMode::MacBinary,
        )
        .unwrap();
        drop(v);
        clean(&img);
        let v = Volume::open(&img, &OpenOptions::default()).unwrap();
        let again = v
            .stat(&format!("/again/src-{fs_kind:?}/Letter"))
            .unwrap()
            .unwrap();
        assert_eq!(again.mac.unwrap().resource_size, 3000);
        assert_eq!(again.size, 11);
        let ad_out = tmp.path().join(format!("ad-{fs_kind:?}"));
        v.extract_with(
            &format!("/again/src-{fs_kind:?}"),
            &ad_out,
            false,
            ForkMode::AppleDouble,
        )
        .unwrap();
        let dir = ad_out.join(format!("src-{fs_kind:?}"));
        assert_eq!(fs::read(dir.join("Letter")).unwrap(), b"Dear reader");
        let back = fs::read(dir.join("._Letter")).unwrap();
        assert_eq!(&back[back.len() - 3000..], &rsrc[..]);
    }
}

#[test]
fn labels_and_properties() {
    let tmp = tempfile::tempdir().unwrap();
    for fs_kind in [FsKind::Hfs, FsKind::HfsPlus] {
        let img = tmp.path().join(format!("p-{fs_kind:?}.img"));
        Volume::create(
            &img,
            NewImage::Floppy(FloppyFormat::find("1440").unwrap()),
            &opts(fs_kind),
        )
        .unwrap();
        let im = Image::open(&img, &rw()).unwrap();
        im.set_label(Some("Renamed Disk")).unwrap();
        assert!(
            im.set_label(None).is_err(),
            "HFS volumes always have a name"
        );
        drop(im);
        fs::write(tmp.path().join("doc"), b"x").unwrap();
        let v = Volume::open(&img, &rw()).unwrap();
        assert_eq!(v.info().unwrap().label, "Renamed Disk");
        v.inject(&tmp.path().join("doc"), "/f", false).unwrap();
        drop(v);
        let im = Image::open(&img, &rw()).unwrap();
        let edit = EntryEdit {
            read_only: Some(true),
            hidden: Some(true),
            modified: Some("2001-02-03 04:05:06".into()),
            ..Default::default()
        };
        assert_eq!(im.edit_entry("/f", &edit, true).unwrap(), 2);
        assert!(
            im.edit_entry(
                "/f",
                &EntryEdit {
                    system: Some(true),
                    ..Default::default()
                },
                false
            )
            .is_err()
        );
        drop(im);
        clean(&img);
        let v = Volume::open(&img, &OpenOptions::default()).unwrap();
        let e = v.stat("/f/doc").unwrap().unwrap();
        assert!(e.attributes.read_only && e.attributes.hidden);
        assert_eq!(e.modified, "2001-02-03 04:05:06");
        assert!(v.stat("/f").unwrap().unwrap().attributes.hidden);
    }
}

#[test]
fn hfs_names_are_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("n.img");
    Volume::create(
        &img,
        NewImage::Floppy(FloppyFormat::find("1440").unwrap()),
        &opts(FsKind::Hfs),
    )
    .unwrap();
    let v = Volume::open(&img, &rw()).unwrap();
    assert!(v.mkdir("/日本").is_err(), "not Mac Roman");
    assert!(
        v.mkdir(&format!("/{}", "x".repeat(32))).is_err(),
        "too long"
    );
    v.mkdir(&format!("/{}", "x".repeat(31))).unwrap();
    v.mkdir("/a:b").unwrap();
    assert_eq!(v.list("/").unwrap().len(), 2);
    drop(v);
    clean(&img);
    let _ = PartitionScheme::Apm;
}

/// `SZ_HFS_CHECK="a.img;b.img"`: the consistency check on images written elsewhere (the
/// QEMU harness runs it after the Linux drivers wrote to s0's volumes).
#[test]
#[ignore]
fn hfs_check_external() {
    let list = std::env::var("SZ_HFS_CHECK").expect("SZ_HFS_CHECK");
    let mut bad = Vec::new();
    for p in list.split(';').filter(|p| !p.is_empty()) {
        let v = Volume::open(Path::new(p), &OpenOptions::default()).unwrap();
        let problems = v.hfs_check().unwrap_or_else(|e| vec![e.to_string()]);
        println!("{p}: {} problems {problems:#?}", problems.len());
        if !problems.is_empty() {
            bad.push(p.to_string());
        }
    }
    assert!(bad.is_empty(), "{bad:?}");
}
