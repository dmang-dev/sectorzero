//! ext2 / ext3 / ext4 (create, read, write) and NTFS (read) volumes.
//!
//! Linux's own tools are the real oracle for ext: the QEMU harness runs `e2fsck -fn`, mounts
//! s0's images in the kernel and checks images made by mke2fs (see RESEARCH.md). These tests
//! cover the same paths without a VM, with `Volume::ext_check` (checksums, bitmaps, counts,
//! links) after every change. `mke2fs-ext4.img.gz` was made by mkfs.ext4 1.47 with its
//! defaults (64bit, flex_bg, metadata_csum, orphan_file) and filled by the Linux kernel.
//! The NTFS image is the `ntfs` crate's test file system (made with mkntfs; MIT OR
//! Apache-2.0, see THIRD_PARTY_NOTICES.md).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sectorzero_core::{
    CreateOptions, EntryEdit, FloppyFormat, FsKind, Geometry, Image, NewImage, OpenOptions,
    PartitionScheme, Volume,
};

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}

fn ext(fs: FsKind) -> CreateOptions {
    CreateOptions {
        fs,
        label: Some("LINUXVOL".into()),
        ..Default::default()
    }
}

fn sample_tree(dir: &Path) -> PathBuf {
    let src = dir.join("tree");
    fs::create_dir_all(src.join("Sub Folder/Deeper")).unwrap();
    fs::write(src.join("readme.txt"), b"hello from SectorZero").unwrap();
    // 1 MiB: past the double-indirect boundary on 1 KiB blocks.
    let big: Vec<u8> = (0..1_048_576u32).map(|i| (i * 7 % 253) as u8).collect();
    fs::write(src.join("Sub Folder/big data.bin"), big).unwrap();
    fs::write(src.join("Sub Folder/Deeper/note.md"), b"# deep").unwrap();
    fs::write(src.join("empty"), b"").unwrap();
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

fn clean(img: &Path, part: Option<usize>) {
    let v = Volume::open(
        img,
        &OpenOptions {
            partition: part,
            ..Default::default()
        },
    )
    .unwrap();
    let problems = v.ext_check().unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
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
        assert_eq!(s.files, 124);
        v.close().unwrap();
    }
    clean(img, None);
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let names: Vec<String> = v
        .list("/tree")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["Sub Folder", "empty", "readme.txt"]);
    assert_eq!(
        v.stat("/TREE/README.TXT").unwrap().unwrap().size,
        21,
        "case-insensitive lookup"
    );
    let out = tmp.join("out");
    v.extract("/", &out, false).unwrap();
    same_tree(&src, &out.join("tree"));
    drop(v);

    // Edit: rename, move between folders, overwrite, delete; then everything back out.
    {
        let v = Volume::open(img, &rw()).unwrap();
        v.rename("/tree/readme.txt", "/tree/README-renamed.txt")
            .unwrap();
        v.rename("/tree/Sub Folder/Deeper", "/moved").unwrap();
        v.mkdir("/a/b/c").unwrap();
        fs::write(tmp.join("readme.txt"), b"second version").unwrap();
        v.inject(&tmp.join("readme.txt"), "/a/b", false).unwrap();
        assert!(v.inject(&tmp.join("readme.txt"), "/a/b", false).is_err());
        fs::write(tmp.join("readme.txt"), b"third").unwrap();
        v.inject(&tmp.join("readme.txt"), "/a/b", true).unwrap();
        v.remove("/tree/Sub Folder/file number 007.txt", false)
            .unwrap();
        assert!(v.remove("/a", false).is_err(), "not empty");
        v.close().unwrap();
    }
    clean(img, None);
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let mut buf = Vec::new();
    v.copy_out("/a/b/readme.txt", &mut buf).unwrap();
    assert_eq!(buf, b"third");
    assert_eq!(v.stat("/moved/note.md").unwrap().unwrap().size, 6);
    assert!(v.stat("/tree/Sub Folder/Deeper").unwrap().is_none());
    assert!(v.stat("/tree/readme.txt").unwrap().is_none());
    assert!(v.stat("/tree/README-renamed.txt").unwrap().is_some());
    assert_eq!(v.list("/tree/Sub Folder").unwrap().len(), 120);
    drop(v);

    // Deleting everything gives every block and inode back.
    {
        let v = Volume::open(img, &rw()).unwrap();
        for e in v.list("/").unwrap() {
            if e.name != "lost+found" {
                v.remove(&e.path, true).unwrap();
            }
        }
        v.close().unwrap();
    }
    clean(img, None);
    let after = Volume::open(img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    assert_eq!(after.free_bytes, before.free_bytes, "blocks leaked");
}

#[test]
fn ext2_floppy() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("linux.img");
    let f = NewImage::Floppy(FloppyFormat::find("2880").unwrap());
    Volume::create(&img, f, &ext(FsKind::Ext2)).unwrap();
    let info = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    assert_eq!(info.fs_type, "ext2");
    assert_eq!(info.label, "LINUXVOL");
    assert!(info.fs_writable && info.read_only_reason.is_none());
    assert_eq!(info.cluster_size, 1024);
    clean(&img, None);
    round_trip(&img, tmp.path());
}

#[test]
fn ext4_floppy() {
    // 2880 blocks: too small for a journal, so (like mke2fs) none.
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("ext4.img");
    let f = NewImage::Floppy(FloppyFormat::find("2880").unwrap());
    Volume::create(&img, f, &ext(FsKind::Ext4)).unwrap();
    let info = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    assert_eq!(info.fs_type, "ext4");
    assert!(info.fs_writable, "{:?}", info.read_only_reason);
    clean(&img, None);
    round_trip(&img, tmp.path());
}

#[test]
fn ext4_journaled_gpt() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("ext4.vhdx");
    let opts = CreateOptions {
        scheme: PartitionScheme::Gpt,
        dynamic: true,
        ..ext(FsKind::Ext4)
    };
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 48 << 20,
        },
        &opts,
    )
    .unwrap();
    clean(&img, None);
    round_trip(&img, tmp.path());
}

/// A file spread over 150 free gaps needs an extent tree below the inode.
#[test]
fn ext4_fragmented_file() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("frag.img");
    let f = NewImage::Floppy(FloppyFormat::find("2880").unwrap());
    Volume::create(&img, f, &ext(FsKind::Ext4)).unwrap();
    let one = tmp.path().join("one");
    fs::create_dir_all(&one).unwrap();
    {
        let v = Volume::open(&img, &rw()).unwrap();
        for i in 0..300 {
            let p = one.join(format!("k{i:04}"));
            fs::write(&p, [i as u8; 1000]).unwrap();
            v.inject(&p, "/fill", false).unwrap();
            fs::remove_file(&p).unwrap();
        }
        for i in (0..300).step_by(2) {
            v.remove(&format!("/fill/k{i:04}"), false).unwrap();
        }
        v.close().unwrap();
    }
    clean(&img, None);
    let data: Vec<u8> = (0..140_000u32).map(|i| (i % 251) as u8).collect();
    let big = tmp.path().join("big.bin");
    fs::write(&big, &data).unwrap();
    {
        let v = Volume::open(&img, &rw()).unwrap();
        v.inject(&big, "/", false).unwrap();
        v.close().unwrap();
    }
    clean(&img, None);
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
    clean(&img, None);
}

#[test]
fn ext_properties() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("props.img");
    let f = NewImage::Floppy(FloppyFormat::find("2880").unwrap());
    Volume::create(&img, f, &ext(FsKind::Ext4)).unwrap();
    let src = sample_tree(tmp.path());
    {
        let v = Volume::open(&img, &rw()).unwrap();
        v.inject(&src, "/", false).unwrap();
        v.close().unwrap();
    }
    let image = Image::open(&img, &rw()).unwrap();
    let edit = EntryEdit {
        read_only: Some(true),
        modified: Some("2001-02-03 04:05:06".into()),
        created: Some("1999-12-31 23:59:58".into()),
        accessed: Some("2040-06-07 08:09".into()),
        ..Default::default()
    };
    assert_eq!(
        image.edit_entry("/tree/readme.txt", &edit, false).unwrap(),
        1
    );
    // A folder and everything in it: 1 + 120 files + "big data.bin" + Deeper + note.md.
    let n = image
        .edit_entry(
            "/tree/Sub Folder",
            &EntryEdit {
                read_only: Some(true),
                ..Default::default()
            },
            true,
        )
        .unwrap();
    assert_eq!(n, 124);
    for bad in [
        EntryEdit {
            hidden: Some(true),
            ..Default::default()
        },
        EntryEdit {
            modified: Some("1850-01-01".into()),
            ..Default::default()
        },
        EntryEdit {
            modified: Some("yesterday".into()),
            ..Default::default()
        },
    ] {
        assert!(image.edit_entry("/tree/empty", &bad, false).is_err());
    }
    drop(image);
    clean(&img, None);
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    let e = v.stat("/tree/readme.txt").unwrap().unwrap();
    assert!(e.attributes.read_only);
    assert_eq!(e.modified, "2001-02-03 04:05:06");
    assert_eq!(e.created, "1999-12-31 23:59:58");
    assert_eq!(e.accessed, "2040-06-07 08:09:00");
    assert!(
        v.stat("/tree/Sub Folder/Deeper/note.md")
            .unwrap()
            .unwrap()
            .attributes
            .read_only
    );
    assert!(!v.stat("/tree/empty").unwrap().unwrap().attributes.read_only);
    drop(v);
    // Clearing read-only gives the owner write permission back.
    let image = Image::open(&img, &rw()).unwrap();
    image
        .edit_entry(
            "/tree/readme.txt",
            &EntryEdit {
                read_only: Some(false),
                ..Default::default()
            },
            false,
        )
        .unwrap();
    drop(image);
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    assert!(
        !v.stat("/tree/readme.txt")
            .unwrap()
            .unwrap()
            .attributes
            .read_only
    );
    drop(v);
    // NTFS stays read-only.
    let ntfs = ntfs_image(tmp.path());
    let image = Image::open(&ntfs, &rw()).unwrap();
    assert!(image.edit_entry("/empty-file", &edit, false).is_err());
}

fn gunzip(name: &str, tmp: &Path) -> PathBuf {
    let gz = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    let mut data = Vec::new();
    flate2::read::GzDecoder::new(fs::File::open(gz).unwrap())
        .read_to_end(&mut data)
        .unwrap();
    let img = tmp.join(name.trim_end_matches(".gz"));
    fs::write(&img, data).unwrap();
    img
}

/// mkfs.ext4's own layout: 64-bit descriptors, flex_bg, uninitialized groups, checksums
/// everywhere, a hashed folder, an orphan file.
#[test]
fn ext4_from_mke2fs() {
    let tmp = tempfile::tempdir().unwrap();
    let img = gunzip("mke2fs-ext4.img.gz", tmp.path());
    clean(&img, None);
    let info = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    assert_eq!(
        (info.fs_type.as_str(), info.label.as_str()),
        ("ext4", "MKE2FS-EXT4")
    );
    assert!(info.fs_writable, "{:?}", info.read_only_reason);
    {
        let v = Volume::open(&img, &rw()).unwrap();
        assert_eq!(v.list("/hashed").unwrap().len(), 300);
        fs::write(tmp.path().join("new.txt"), b"added by SectorZero").unwrap();
        // Adding to and removing from the hashed folder turns it into a linear one.
        v.inject(&tmp.path().join("new.txt"), "/hashed", false)
            .unwrap();
        v.remove("/hashed/entry-number-7.txt", false).unwrap();
        v.rename("/dir", "/hashed/dir").unwrap();
        v.close().unwrap();
    }
    clean(&img, None);
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    assert_eq!(v.list("/hashed").unwrap().len(), 301);
    let mut b = Vec::new();
    v.copy_out("/hashed/dir/sub/deep.txt", &mut b).unwrap();
    assert_eq!(b, b"deep\n");
    drop(v);
    // Clear out what Linux wrote (hard links, symlinks, the de-indexed folder), then the
    // usual round trip on mke2fs's layout.
    {
        let v = Volume::open(&img, &rw()).unwrap();
        for e in v.list("/").unwrap() {
            if e.name != "lost+found" {
                v.remove(&e.path, true).unwrap();
            }
        }
        v.close().unwrap();
    }
    clean(&img, None);
    round_trip(&img, tmp.path());
}

#[test]
fn ext3_on_partitioned_vhd() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("linux.vhd");
    let opts = CreateOptions {
        scheme: PartitionScheme::Mbr,
        dynamic: true,
        ..ext(FsKind::Ext3)
    };
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 40 << 20,
        },
        &opts,
    )
    .unwrap();
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    let info = v.info().unwrap();
    assert_eq!(info.fs_type, "ext3");
    let p = info.partition.unwrap();
    assert_eq!((p.type_id.as_str(), p.fs), ("0x83", Some("ext3")));
    assert!(v.list("/").unwrap().iter().any(|e| e.name == "lost+found"));
    drop(v);
    round_trip(&img, tmp.path());
}

#[test]
fn ext2_gpt_and_labels() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("gpt.img");
    let opts = CreateOptions {
        scheme: PartitionScheme::Gpt,
        ..ext(FsKind::Ext2)
    };
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 8 << 20,
        },
        &opts,
    )
    .unwrap();
    let image = Image::open(&img, &rw()).unwrap();
    assert_eq!(image.partition().unwrap().type_name, "Linux filesystem");
    image.set_label(Some("renamed")).unwrap();
    drop(image);
    assert_eq!(
        Volume::open(&img, &OpenOptions::default())
            .unwrap()
            .label()
            .unwrap(),
        "renamed"
    );
    Image::open(&img, &rw()).unwrap().set_label(None).unwrap();
    assert_eq!(
        Volume::open(&img, &OpenOptions::default())
            .unwrap()
            .label()
            .unwrap(),
        ""
    );
    assert!(
        Image::open(&img, &rw())
            .unwrap()
            .set_label(Some("seventeen chars!!"))
            .is_err()
    );
}

#[test]
fn ext_refusals() {
    let tmp = tempfile::tempdir().unwrap();
    let f = NewImage::Floppy(FloppyFormat::find("1440").unwrap());
    let err = Volume::create(&tmp.path().join("j.img"), f, &ext(FsKind::Ext3)).unwrap_err();
    assert!(err.to_string().contains("journal"), "{err}");
    let g = Geometry::from_floppy(FloppyFormat::find("1440").unwrap());
    assert!(
        Volume::create(
            &tmp.path().join("c.img"),
            NewImage::Custom(g),
            &ext(FsKind::Ext2)
        )
        .is_err()
    );
    // FAT-only tools say so.
    let img = tmp.path().join("e.img");
    Volume::create(&img, f, &ext(FsKind::Ext2)).unwrap();
    let image = Image::open(&img, &rw()).unwrap();
    let err = image.defragment(false).unwrap_err().to_string();
    assert!(err.contains("ext2"), "{err}");
    // Read-only open refuses writes.
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    assert!(v.mkdir("/x").is_err());
}

fn ntfs_image(tmp: &Path) -> PathBuf {
    gunzip("ntfs-testfs1.img.gz", tmp)
}

#[test]
fn ntfs_read() {
    let tmp = tempfile::tempdir().unwrap();
    let img = ntfs_image(tmp.path());
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    let info = v.info().unwrap();
    assert_eq!(info.fs_type, "NTFS");
    assert_eq!(info.label, "mylabel");
    assert!(!info.fs_writable && info.read_only_reason.is_some());
    assert!(info.free_bytes > 0 && info.free_bytes < info.total_bytes);
    let names: Vec<String> = v.list("/").unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        [
            "many_subdirs",
            "1000-bytes-file",
            "empty-file",
            "file-with-12345",
            "sparse-file"
        ]
    );
    let read = |p: &str| {
        let mut b = Vec::new();
        v.copy_out(p, &mut b).unwrap();
        b
    };
    assert_eq!(read("/file-with-12345"), b"12345");
    assert_eq!(read("/1000-bytes-file"), b"12345".repeat(200));
    let sparse = read("/SPARSE-FILE");
    assert_eq!(sparse.len(), 500_005);
    assert_eq!(&sparse[..5], b"12345");
    assert_eq!(&sparse[500_000..], b"11111");
    assert!(sparse[5..500_000].iter().all(|&b| b == 0));
    assert_eq!(
        v.stat("/empty-file").unwrap().unwrap().modified[..10],
        *"2021-01-01"
    );
    assert_eq!(v.list("/many_subdirs").unwrap().len(), 512);
    assert!(v.list("/many_subdirs/12").unwrap().is_empty());
    // Writes are refused.
    drop(v);
    let v = Volume::open(&img, &rw()).unwrap();
    assert!(v.mkdir("/x").is_err());
    assert!(v.inject(&img, "/", false).is_err());
}
