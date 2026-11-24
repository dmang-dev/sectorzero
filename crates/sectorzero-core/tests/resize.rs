//! Resizing partitioned (MBR and GPT) disk images: the container, the partition table and
//! the last partition's FAT volume.

use std::fs;
use std::path::Path;

use sectorzero_core::{
    CreateOptions, EntryEdit, FsKind, Image, Layout, NewImage, OpenOptions, PartitionScheme, Volume,
};

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}

fn disk(path: &Path, mb: u64, scheme: PartitionScheme, fs: FsKind) {
    Volume::create(
        path,
        NewImage::Volume {
            size_bytes: mb << 20,
        },
        &CreateOptions {
            scheme,
            fs,
            label: Some("RESIZEME".into()),
            ..Default::default()
        },
    )
    .unwrap();
}

fn fill(img: &Path, tmp: &Path) -> std::path::PathBuf {
    let src = tmp.join("tree");
    fs::create_dir_all(src.join("sub/deeper")).unwrap();
    fs::write(src.join("a long file name.txt"), b"hello").unwrap();
    let big: Vec<u8> = (0..900_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(src.join("sub/big.bin"), big).unwrap();
    for i in 0..50 {
        fs::write(src.join(format!("sub/deeper/f{i}.txt")), format!("{i}")).unwrap();
    }
    let v = Volume::open(img, &rw()).unwrap();
    v.inject(&src, "/", false).unwrap();
    v.close().unwrap();
    let image = Image::open(img, &rw()).unwrap();
    image
        .edit_entry(
            "/tree/a long file name.txt",
            &EntryEdit {
                read_only: Some(true),
                hidden: Some(true),
                modified: Some("2001-02-03 04:05:06".into()),
                ..Default::default()
            },
            false,
        )
        .unwrap();
    src
}

fn same_tree(a: &Path, b: &Path) {
    for e in fs::read_dir(a).unwrap() {
        let e = e.unwrap();
        let other = b.join(e.file_name());
        if e.path().is_dir() {
            same_tree(&e.path(), &other);
        } else {
            assert_eq!(fs::read(e.path()).unwrap(), fs::read(&other).unwrap());
        }
    }
}

fn check_files(img: &Path, src: &Path, tmp: &Path, serial: &str) {
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let info = v.info().unwrap();
    assert_eq!(info.label, "RESIZEME");
    assert_eq!(info.volume_id, serial);
    let e = v.stat("/tree/a long file name.txt").unwrap().unwrap();
    assert!(e.attributes.read_only && e.attributes.hidden);
    assert_eq!(e.modified, "2001-02-03 04:05:06");
    let out = tmp.join(format!("out-{}", info.total_bytes));
    v.extract("/", &out, true).unwrap();
    same_tree(src, &out.join("tree"));
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Both GPT headers are valid, point at each other, and their entry arrays match.
fn check_gpt(raw: &[u8]) {
    let total = (raw.len() / 512) as u64;
    let header = |lba: u64| {
        let h = &raw[(lba * 512) as usize..(lba * 512 + 512) as usize];
        assert_eq!(&h[0..8], b"EFI PART", "header at {lba}");
        let size = le32(h, 12) as usize;
        let mut c = h[..size].to_vec();
        c[16..20].fill(0);
        assert_eq!(crc32fast::hash(&c), le32(h, 16), "header CRC at {lba}");
        let entries = le64(h, 72) as usize * 512;
        let len = le32(h, 80) as usize * le32(h, 84) as usize;
        assert_eq!(crc32fast::hash(&raw[entries..entries + len]), le32(h, 88));
        h.to_vec()
    };
    let p = header(1);
    let b = header(total - 1);
    assert_eq!(le64(&p, 32), total - 1);
    assert_eq!(le64(&b, 24), total - 1);
    assert_eq!(le64(&b, 32), 1);
    assert_eq!(le64(&p, 48), le64(&b, 48));
    assert_eq!(le64(&p, 48), total - 34, "last usable LBA");
    // Protective MBR covers the disk.
    assert_eq!(raw[446 + 4], 0xEE);
    assert_eq!(u64::from(le32(raw, 446 + 12)), (total - 1).min(0xFFFF_FFFF));
}

fn part1(img: &Path) -> sectorzero_core::PartitionInfo {
    let image = Image::open(img, &OpenOptions::default()).unwrap();
    image.layout().partitions()[0].clone()
}

#[test]
fn gpt_fat_grows_and_shrinks() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("g.img");
    disk(&img, 64, PartitionScheme::Gpt, FsKind::Fat);
    let src = fill(&img, tmp.path());
    let serial = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap()
        .volume_id;

    let r = Volume::resize_disk(&img, &OpenOptions::default(), 600 << 20, true).unwrap();
    assert_eq!(r.new_size, 600 << 20);
    assert_eq!(r.stats.as_ref().unwrap().files, 52);
    let raw = fs::read(&img).unwrap();
    assert_eq!(raw.len() as u64, 600 << 20);
    check_gpt(&raw);
    let p = part1(&img);
    assert_eq!(p.start_lba + p.sectors - 1, (600 << 11) - 34);
    assert_eq!(
        Volume::open(&img, &OpenOptions::default())
            .unwrap()
            .info()
            .unwrap()
            .fs_type,
        "FAT32"
    );
    check_files(&img, &src, tmp.path(), &serial);

    Volume::resize_disk(&img, &OpenOptions::default(), 40 << 20, true).unwrap();
    let raw = fs::read(&img).unwrap();
    assert_eq!(raw.len() as u64, 40 << 20);
    check_gpt(&raw);
    check_files(&img, &src, tmp.path(), &serial);

    // Too small for the files.
    let extra = tmp.path().join("extra.bin");
    fs::write(&extra, vec![7u8; 1_500_000]).unwrap();
    {
        let v = Volume::open(&img, &rw()).unwrap();
        v.inject(&extra, "/", false).unwrap();
        v.close().unwrap();
    }
    assert!(Volume::resize_disk(&img, &OpenOptions::default(), 2 << 20, true).is_err());
    check_files(&img, &src, tmp.path(), &serial);
}

#[test]
fn mbr_fat_grows_into_fat32_and_vhd_keeps_its_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("m.vhd");
    disk(&img, 48, PartitionScheme::Mbr, FsKind::Fat);
    let src = fill(&img, tmp.path());
    let before = Image::open(&img, &OpenOptions::default())
        .unwrap()
        .mbr()
        .unwrap();
    assert_eq!(before[446 + 4], 0x0E, "FAT16 LBA");
    let serial = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap()
        .volume_id;
    Volume::resize_disk(&img, &OpenOptions::default(), 700 << 20, true).unwrap();
    let image = Image::open(&img, &OpenOptions::default()).unwrap();
    let mbr = image.mbr().unwrap();
    assert_eq!(mbr[446 + 4], 0x0C, "FAT32 LBA after growing");
    assert_eq!(mbr[440..444], before[440..444], "disk signature kept");
    assert_eq!(mbr[446], 0x80, "still the boot partition");
    assert_eq!(le32(&mbr, 446 + 8), 2048);
    assert_eq!(u64::from(le32(&mbr, 446 + 12)), (700 << 11) - 2048);
    let c = image.container();
    assert_eq!(c.virtual_size, 700 << 20);
    assert_eq!(c.kind, sectorzero_core::ContainerKind::Vhd);
    drop(image);
    check_files(&img, &src, tmp.path(), &serial);
}

#[test]
fn keep_partitions_moves_only_the_table() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("e.img");
    disk(&img, 32, PartitionScheme::Gpt, FsKind::Ext4);
    let src = tmp.path().join("x.txt");
    fs::write(&src, b"ext payload").unwrap();
    {
        let v = Volume::open(&img, &rw()).unwrap();
        v.inject(&src, "/", false).unwrap();
        v.close().unwrap();
    }
    let before = part1(&img);
    let err = Volume::resize_disk(&img, &OpenOptions::default(), 64 << 20, true).unwrap_err();
    assert!(err.to_string().contains("FAT only"), "{err}");
    let r = Volume::resize_disk(&img, &OpenOptions::default(), 64 << 20, false).unwrap();
    assert!(r.partition.is_none());
    check_gpt(&fs::read(&img).unwrap());
    let after = part1(&img);
    assert_eq!(
        (after.start_lba, after.sectors),
        (before.start_lba, before.sectors)
    );
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    assert!(v.ext_check().unwrap().is_empty());
    let mut buf = Vec::new();
    v.copy_out("/x.txt", &mut buf).unwrap();
    assert_eq!(buf, b"ext payload");
    drop(v);
    // Shrinking below the partition is refused and leaves the file alone.
    let err = Volume::resize_disk(&img, &OpenOptions::default(), 16 << 20, false).unwrap_err();
    assert!(err.to_string().contains("ends at sector"), "{err}");
    assert_eq!(fs::metadata(&img).unwrap().len(), 64 << 20);
}

#[test]
fn bare_volumes_are_not_partitioned_disks() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("b.img");
    disk(&img, 8, PartitionScheme::None, FsKind::Fat);
    assert!(matches!(
        Image::open(&img, &OpenOptions::default()).unwrap().layout(),
        Layout::BareVolume
    ));
    assert!(Volume::resize_disk(&img, &OpenOptions::default(), 16 << 20, true).is_err());
}
