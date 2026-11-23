use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use sectorzero_core::{
    CreateOptions, Error, FLOPPY_FORMATS, FloppyFormat, Layout, NewImage, OpenOptions, Volume,
    probe,
};

const RW: &OpenOptions = &OpenOptions {
    writable: true,
    partition: None,
    member: None,
};
const RO: &OpenOptions = &OpenOptions {
    writable: false,
    partition: None,
    member: None,
};

fn labelled(label: &str) -> CreateOptions {
    CreateOptions {
        label: Some(label.into()),
        ..Default::default()
    }
}

fn floppy(dir: &Path, id: &str) -> std::path::PathBuf {
    let path = dir.join(format!("{id}.ima"));
    Volume::create(
        &path,
        NewImage::Floppy(FloppyFormat::find(id).unwrap()),
        &labelled("TEST"),
    )
    .unwrap();
    path
}

#[test]
fn every_floppy_format_creates_and_reopens() {
    let tmp = tempfile::tempdir().unwrap();
    for f in FLOPPY_FORMATS {
        let path = tmp.path().join(format!("{}.ima", f.id));
        Volume::create(&path, NewImage::Floppy(f), &labelled("FMT"))
            .unwrap_or_else(|e| panic!("{}: {e}", f.id));
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            f.size_bytes(),
            "{}",
            f.id
        );
        let vol = Volume::open(&path, RO).unwrap();
        let info = vol.info().unwrap();
        assert_eq!(info.fs_type, "FAT12", "{}", f.id);
        assert_eq!(info.cluster_size, f.bytes_per_cluster, "{}", f.id);
        assert_eq!(info.floppy_format, Some(f.name), "{}", f.id);
        assert_eq!(info.label, "FMT", "{}", f.id);
        assert!(vol.list("/").unwrap().is_empty(), "{}", f.id);
    }
}

#[test]
fn inject_list_extract_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let img = floppy(tmp.path(), "1440");

    // Host tree: a file with a long name, and a folder with a nested file.
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("Docs/Deep")).unwrap();
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(src.join("A long file name.txt"), b"hello floppy").unwrap();
    fs::write(src.join("Docs/Deep/data.bin"), &big).unwrap();

    {
        let vol = Volume::open(&img, RW).unwrap();
        let s = vol
            .inject(&src.join("A long file name.txt"), "/", false)
            .unwrap();
        assert_eq!((s.files, s.bytes), (1, 12));
        let s = vol.inject(&src.join("Docs"), "/", false).unwrap();
        assert_eq!((s.files, s.dirs, s.bytes), (1, 2, big.len() as u64));
        // Same file again without overwrite is refused; with overwrite it succeeds.
        assert!(matches!(
            vol.inject(&src.join("A long file name.txt"), "/", false),
            Err(Error::Exists(_))
        ));
        vol.inject(&src.join("A long file name.txt"), "/", true)
            .unwrap();
        vol.close().unwrap();
    }

    let vol = Volume::open(&img, RO).unwrap();
    let root = vol.list("/").unwrap();
    let names: Vec<_> = root.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Docs", "A long file name.txt"]); // folders first
    assert_eq!(root[1].short_name, "ALONGF~1.TXT");
    let all = vol.walk("/").unwrap();
    assert!(
        all.iter()
            .any(|e| e.path == "Docs/Deep/data.bin" && e.size == big.len() as u64)
    );
    // Case-insensitive lookup, either slash style.
    assert!(vol.stat("docs\\DEEP\\DATA.BIN").unwrap().is_some());

    let out = tmp.path().join("out");
    let s = vol.extract("", &out, false).unwrap();
    assert_eq!(s.files, 2);
    assert_eq!(
        fs::read(out.join("A long file name.txt")).unwrap(),
        b"hello floppy"
    );
    assert_eq!(fs::read(out.join("Docs/Deep/data.bin")).unwrap(), big);

    // Writes are refused on a read-only open, before touching the image.
    assert!(vol.mkdir("/X").is_err());
}

#[test]
fn mkdir_rename_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let img = floppy(tmp.path(), "720");
    let f = tmp.path().join("note.txt");
    fs::write(&f, b"x").unwrap();

    let vol = Volume::open(&img, RW).unwrap();
    vol.mkdir("/a/b/c").unwrap();
    vol.mkdir("/a/b").unwrap(); // already exists: fine
    vol.inject(&f, "/a/b/c", false).unwrap();
    vol.rename("/a/b/c/note.txt", "/a").unwrap(); // move into a folder
    assert!(vol.stat("/a/note.txt").unwrap().is_some());
    vol.rename("/a/note.txt", "/a/renamed.txt").unwrap();
    assert!(vol.stat("/a/renamed.txt").unwrap().is_some());

    assert!(matches!(vol.remove("/a", false), Err(Error::Invalid(_)))); // not empty
    let s = vol.remove("/a", true).unwrap();
    assert_eq!((s.files, s.dirs), (1, 3));
    assert!(vol.list("/").unwrap().is_empty());
    assert!(matches!(
        vol.remove("/missing", false),
        Err(Error::NotFound(_))
    ));
    vol.close().unwrap();
}

#[test]
fn full_image_reports_no_space_and_leaves_no_partial_file() {
    let tmp = tempfile::tempdir().unwrap();
    let img = floppy(tmp.path(), "360");
    let f = tmp.path().join("big.bin");
    fs::write(&f, vec![7u8; 400 * 1024]).unwrap();

    let vol = Volume::open(&img, RW).unwrap();
    let err = vol.inject(&f, "/", false).unwrap_err();
    assert!(err.to_string().contains("not enough free space"), "{err}");
    assert!(vol.stat("/big.bin").unwrap().is_none());
    assert_eq!(
        vol.info().unwrap().free_clusters,
        vol.info().unwrap().total_clusters
    );
}

#[test]
fn opens_fat_partition_inside_mbr_disk() {
    let tmp = tempfile::tempdir().unwrap();
    // Build a 34 MiB disk: MBR, then a FAT16 partition at LBA 2048.
    let part = tmp.path().join("part.img");
    Volume::create(
        &part,
        NewImage::Volume {
            size_bytes: 32 << 20,
        },
        &labelled("PART1"),
    )
    .unwrap();
    let start_lba = 2048u32;
    let sectors = (32u32 << 20) / 512;

    let disk = tmp.path().join("disk.img");
    let mut d = fs::File::create(&disk).unwrap();
    let mut mbr = [0u8; 512];
    let e = &mut mbr[446..462];
    e[0] = 0x80;
    e[4] = 0x06; // FAT16
    e[8..12].copy_from_slice(&start_lba.to_le_bytes());
    e[12..16].copy_from_slice(&sectors.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    d.write_all(&mbr).unwrap();
    d.seek(SeekFrom::Start(u64::from(start_lba) * 512)).unwrap();
    d.write_all(&fs::read(&part).unwrap()).unwrap();
    d.set_len(u64::from(start_lba + sectors) * 512 + (2 << 20))
        .unwrap();
    drop(d);

    match probe(&disk).unwrap().1 {
        Layout::Mbr(p) => assert_eq!(
            (p.len(), p[0].type_id.as_str(), p[0].bootable),
            (1, "0x06", true)
        ),
        other => panic!("{other:?}"),
    }

    let f = tmp.path().join("hi.txt");
    fs::write(&f, b"in a partition").unwrap();
    let vol = Volume::open(&disk, RW).unwrap();
    let info = vol.info().unwrap();
    assert_eq!(
        (
            info.fs_type.as_str(),
            info.label.as_str(),
            info.volume_offset
        ),
        ("FAT16", "PART1", 2048 * 512)
    );
    vol.inject(&f, "/", false).unwrap();
    vol.close().unwrap();

    // The MBR itself was not touched, and the file is readable after reopening.
    assert_eq!(&fs::read(&disk).unwrap()[..512], &mbr);
    let vol = Volume::open(
        &disk,
        &OpenOptions {
            partition: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    let out = tmp.path().join("out");
    vol.extract("/hi.txt", &out, false).unwrap();
    assert_eq!(fs::read(out.join("hi.txt")).unwrap(), b"in a partition");
    assert!(
        Volume::open(
            &disk,
            &OpenOptions {
                partition: Some(2),
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn rejects_non_images() {
    let tmp = tempfile::tempdir().unwrap();
    let junk = tmp.path().join("junk.ima");
    fs::write(&junk, vec![0u8; 4096]).unwrap();
    assert!(matches!(Volume::open(&junk, RO), Err(Error::NotFat(_))));
    assert!(matches!(probe(&junk).unwrap().1, Layout::Unknown));
}

/// WinImage (and some DOS tools) reject a folder whose first two slots are not plain `.`/`..`
/// entries; upstream fatfs 0.3.6 wrote long-name entries before them (patched in third_party/fatfs).
#[test]
fn new_folders_start_with_plain_dot_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let img = floppy(tmp.path(), "1440");
    {
        let vol = Volume::open(&img, RW).unwrap();
        vol.mkdir("A Long Folder Name").unwrap();
        vol.close().unwrap();
    }
    let raw = fs::read(&img).unwrap();
    let u16_at = |o: usize| u16::from_le_bytes([raw[o], raw[o + 1]]) as usize;
    let (bps, spc, reserved, fats, root_entries, spf) = (
        u16_at(11),
        raw[13] as usize,
        u16_at(14),
        raw[16] as usize,
        u16_at(17),
        u16_at(22),
    );
    let root = (reserved + fats * spf) * bps;
    let data = root + root_entries * 32;
    let dir = (0..root_entries)
        .map(|i| &raw[root + i * 32..root + i * 32 + 32])
        .find(|e| e[11] == 0x10)
        .expect("folder entry in the root directory");
    let cluster = u16::from_le_bytes([dir[26], dir[27]]) as usize;
    let first = data + (cluster - 2) * spc * bps;
    assert_eq!(&raw[first..first + 11], b".          ");
    assert_eq!(
        raw[first + 11] & 0x10,
        0x10,
        "`.` is a directory entry, not an LFN slot"
    );
    assert_eq!(&raw[first + 32..first + 43], b"..         ");
    assert_eq!(raw[first + 43] & 0x10, 0x10);
}
