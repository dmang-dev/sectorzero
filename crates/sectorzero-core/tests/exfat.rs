//! exFAT volumes: create, read and write.
//!
//! Every change is followed by `Volume::exfat_check` (boot region and entry set checksums,
//! name hashes, cluster ownership against the allocation bitmap). The QEMU harness checks
//! the same images with Linux's exFAT driver and fsck.exfat (see RESEARCH.md).

use std::fs;
use std::path::{Path, PathBuf};

use sectorzero_core::{
    CreateOptions, EntryEdit, FloppyFormat, FsKind, Image, NewImage, OpenOptions, PartitionScheme,
    Volume,
};

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}

fn exfat() -> CreateOptions {
    CreateOptions {
        fs: FsKind::Exfat,
        label: Some("EXFATVOL".into()),
        ..Default::default()
    }
}

fn clean(img: &Path) {
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let problems = v.exfat_check().unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
}

fn sample_tree(dir: &Path) -> PathBuf {
    let src = dir.join("tree");
    fs::create_dir_all(src.join("Sub Folder/Deeper")).unwrap();
    fs::write(src.join("readme.txt"), b"hello from SectorZero").unwrap();
    let big: Vec<u8> = (0..700_000u32).map(|i| (i * 7 % 253) as u8).collect();
    fs::write(src.join("Sub Folder/big data.bin"), big).unwrap();
    fs::write(src.join("Sub Folder/Deeper/note.md"), b"# deep").unwrap();
    fs::write(src.join("empty"), b"").unwrap();
    fs::write(
        src.join("A file name that is long enough to need several name entries.txt"),
        b"long",
    )
    .unwrap();
    fs::write(src.join("Ünïcödé ωορδ 日本.txt"), b"unicode").unwrap();
    // 120 files of 4 entries each: the folder outgrows one 4 KiB cluster several times.
    for i in 0..120 {
        fs::write(
            src.join(format!("Sub Folder/file number {i:03}.txt")),
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

fn round_trip(img: &Path, tmp: &Path) {
    let src = sample_tree(tmp);
    let before = Volume::open(img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    {
        let v = Volume::open(img, &rw()).unwrap();
        let s = v.inject(&src, "/", false).unwrap();
        assert_eq!(s.files, 126);
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
    assert_eq!(
        names,
        [
            "Sub Folder",
            "A file name that is long enough to need several name entries.txt",
            "empty",
            "readme.txt",
            "Ünïcödé ωορδ 日本.txt",
        ]
    );
    assert_eq!(
        v.stat("/TREE/README.TXT").unwrap().unwrap().size,
        21,
        "case-insensitive lookup"
    );
    assert!(
        v.stat("/tree/ÜNÏCÖDÉ ΩΟΡΔ 日本.TXT").unwrap().is_some(),
        "up-case table beyond ASCII"
    );
    let out = tmp.join("out");
    v.extract("/", &out, false).unwrap();
    same_tree(&src, &out.join("tree"));
    drop(v);

    {
        let v = Volume::open(img, &rw()).unwrap();
        v.rename("/tree/readme.txt", "/tree/README-renamed.txt")
            .unwrap();
        v.rename("/tree/empty", "/tree/EMPTY").unwrap();
        v.rename("/tree/Sub Folder/Deeper", "/moved").unwrap();
        assert!(v.rename("/tree", "/tree/Sub Folder/x").is_err());
        assert!(v.rename("/moved", "/tree/Sub Folder").is_ok());
        v.mkdir("/a/b/c").unwrap();
        fs::write(tmp.join("readme.txt"), b"second version").unwrap();
        v.inject(&tmp.join("readme.txt"), "/a/b", false).unwrap();
        assert!(v.inject(&tmp.join("readme.txt"), "/a/b", false).is_err());
        fs::write(tmp.join("readme.txt"), b"third").unwrap();
        v.inject(&tmp.join("readme.txt"), "/a/b", true).unwrap();
        v.remove("/tree/Sub Folder/file number 007.txt", false)
            .unwrap();
        assert!(v.remove("/a", false).is_err(), "not empty");
        assert!(v.mkdir("/bad:name").is_err());
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
    assert!(v.stat("/tree/readme.txt").unwrap().is_none());
    assert!(v.stat("/tree/README-renamed.txt").unwrap().is_some());
    assert_eq!(v.stat("/tree/empty").unwrap().unwrap().name, "EMPTY");
    assert_eq!(v.list("/tree/Sub Folder").unwrap().len(), 121);
    drop(v);

    {
        let v = Volume::open(img, &rw()).unwrap();
        for e in v.list("/").unwrap() {
            v.remove(&e.path, true).unwrap();
        }
        v.close().unwrap();
    }
    clean(img);
    let after = Volume::open(img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    assert_eq!(after.free_bytes, before.free_bytes, "clusters leaked");
}

#[test]
fn exfat_floppy() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("ex.img");
    let f = NewImage::Floppy(FloppyFormat::find("2880").unwrap());
    Volume::create(&img, f, &exfat()).unwrap();
    let info = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    assert_eq!(info.fs_type, "exFAT");
    assert_eq!(info.label, "EXFATVOL");
    assert_eq!(info.cluster_size, 4096);
    assert!(info.fs_writable, "{:?}", info.read_only_reason);
    clean(&img);
    round_trip(&img, tmp.path());
}

#[test]
fn exfat_gpt_vhdx() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("ex.vhdx");
    let opts = CreateOptions {
        scheme: PartitionScheme::Gpt,
        dynamic: true,
        ..exfat()
    };
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 300 << 20,
        },
        &opts,
    )
    .unwrap();
    let info = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    assert_eq!(info.cluster_size, 32 * 1024);
    assert_eq!(info.partition.as_ref().unwrap().fs, Some("exFAT"));
    clean(&img);
    round_trip(&img, tmp.path());
}

#[test]
fn exfat_mbr_partition_type() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("ex.vhd");
    let opts = CreateOptions {
        scheme: PartitionScheme::Mbr,
        ..exfat()
    };
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 64 << 20,
        },
        &opts,
    )
    .unwrap();
    let image = Image::open(&img, &OpenOptions::default()).unwrap();
    assert_eq!(image.mbr().unwrap()[0x1C2], 0x07);
    drop(image);
    clean(&img);
}

/// With no contiguous run left, a file is chained through the FAT.
#[test]
fn exfat_fragmented_file() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("frag.img");
    let f = NewImage::Floppy(FloppyFormat::find("1440").unwrap());
    Volume::create(&img, f, &exfat()).unwrap();
    let one = tmp.path().join("one");
    fs::create_dir_all(&one).unwrap();
    {
        let v = Volume::open(&img, &rw()).unwrap();
        let mut i = 0;
        loop {
            let p = one.join(format!("k{i:04}"));
            fs::write(&p, [i as u8; 4096]).unwrap();
            let r = v.inject(&p, "/fill", false);
            fs::remove_file(&p).unwrap();
            if r.is_err() {
                break;
            }
            i += 1;
        }
        assert!(i > 300, "{i}");
        for k in (0..i).step_by(2) {
            v.remove(&format!("/fill/k{k:04}"), false).unwrap();
        }
        v.close().unwrap();
    }
    clean(&img);
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let big = tmp.path().join("big.bin");
    fs::write(&big, &data).unwrap();
    {
        let v = Volume::open(&img, &rw()).unwrap();
        v.inject(&big, "/", false).unwrap();
        v.close().unwrap();
    }
    clean(&img);
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    let mut back = Vec::new();
    v.copy_out("/big.bin", &mut back).unwrap();
    assert!(back == data, "fragmented file reads back");
    drop(v);
    {
        let v = Volume::open(&img, &rw()).unwrap();
        v.remove("/big.bin", false).unwrap();
        v.remove("/fill", true).unwrap();
        v.close().unwrap();
    }
    clean(&img);
}

#[test]
fn exfat_properties_and_label() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("props.img");
    let f = NewImage::Floppy(FloppyFormat::find("2880").unwrap());
    Volume::create(&img, f, &exfat()).unwrap();
    let src = sample_tree(tmp.path());
    {
        let v = Volume::open(&img, &rw()).unwrap();
        v.inject(&src, "/", false).unwrap();
        v.close().unwrap();
    }
    let image = Image::open(&img, &rw()).unwrap();
    let edit = EntryEdit {
        read_only: Some(true),
        hidden: Some(true),
        archive: Some(false),
        modified: Some("2001-02-03 04:05:06".into()),
        created: Some("1999-12-31 23:59:58".into()),
        accessed: Some("2040-06-07 08:09".into()),
        ..Default::default()
    };
    assert_eq!(
        image.edit_entry("/tree/readme.txt", &edit, false).unwrap(),
        1
    );
    let n = image
        .edit_entry(
            "/tree/Sub Folder",
            &EntryEdit {
                system: Some(true),
                ..Default::default()
            },
            true,
        )
        .unwrap();
    assert_eq!(n, 124);
    assert!(
        image
            .edit_entry(
                "/tree/empty",
                &EntryEdit {
                    modified: Some("1970-01-01".into()),
                    ..Default::default()
                },
                false
            )
            .is_err(),
        "before 1980"
    );
    image.set_label(Some("Ünï Label")).unwrap();
    assert!(image.set_label(Some("twelve chars")).is_err());
    drop(image);
    clean(&img);
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    let e = v.stat("/tree/readme.txt").unwrap().unwrap();
    assert!(e.attributes.read_only && e.attributes.hidden && !e.attributes.archive);
    assert_eq!(e.modified, "2001-02-03 04:05:06");
    assert_eq!(e.created, "1999-12-31 23:59:58");
    assert_eq!(e.accessed, "2040-06-07 08:09:00");
    let e = v.stat("/tree/Sub Folder/Deeper/note.md").unwrap().unwrap();
    assert!(e.attributes.system && e.attributes.archive);
    assert_eq!(v.info().unwrap().label, "Ünï Label");
    drop(v);
    let image = Image::open(&img, &rw()).unwrap();
    image.set_label(None).unwrap();
    drop(image);
    assert_eq!(
        Volume::open(&img, &OpenOptions::default())
            .unwrap()
            .info()
            .unwrap()
            .label,
        ""
    );
    clean(&img);
}

/// A volume that was not cleanly unmounted opens read-only, and writes leave the dirty flag
/// clear again.
#[test]
fn exfat_dirty_volume_is_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("dirty.img");
    let f = NewImage::Floppy(FloppyFormat::find("1440").unwrap());
    Volume::create(&img, f, &exfat()).unwrap();
    fs::write(tmp.path().join("x.txt"), b"x").unwrap();
    {
        let v = Volume::open(&img, &rw()).unwrap();
        v.inject(&tmp.path().join("x.txt"), "/", false).unwrap();
        v.close().unwrap();
    }
    let mut raw = fs::read(&img).unwrap();
    assert_eq!(raw[106] & 2, 0, "dirty flag cleared after writing");
    raw[106] |= 2;
    fs::write(&img, &raw).unwrap();
    let v = Volume::open(&img, &rw()).unwrap();
    let info = v.info().unwrap();
    assert!(!info.fs_writable);
    assert!(info.read_only_reason.unwrap().contains("cleanly"));
    assert!(v.mkdir("/new").is_err());
    let mut buf = Vec::new();
    v.copy_out("/x.txt", &mut buf).unwrap();
    assert_eq!(buf, b"x");
}

/// Checks an exFAT image made or changed by other software (the QEMU harness passes the
/// Linux kernel's results): `SZ_EXFAT_CHECK="img[|partition];…" cargo test -- --ignored`.
#[test]
#[ignore]
fn exfat_check_external() {
    let list = std::env::var("SZ_EXFAT_CHECK").expect("set SZ_EXFAT_CHECK");
    for item in list.split(';').filter(|s| !s.is_empty()) {
        let (path, part) = match item.rsplit_once('|') {
            Some((p, n)) => (p, Some(n.parse::<usize>().unwrap())),
            None => (item, None),
        };
        let v = Volume::open(
            Path::new(path),
            &OpenOptions {
                partition: part,
                ..Default::default()
            },
        )
        .unwrap();
        let problems = v.exfat_check().unwrap();
        println!("{item}: {} problem(s)", problems.len());
        assert!(problems.is_empty(), "{item}: {problems:#?}");
    }
}
