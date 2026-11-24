//! Removable media presets (Zip, Jaz, SuperDisk, magneto-optical): exact capacities, the
//! vendors' layouts, and FAT on 2048-byte sectors.

use std::fs;

use sectorzero_core::{
    ContainerKind, CreateOptions, FsKind, Image, Layout, MEDIA_FORMATS, MediaFormat, MediaLayout,
    NewImage, OpenOptions, PartitionScheme, Volume,
};

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn opts(fs: FsKind) -> CreateOptions {
    CreateOptions {
        label: Some("CART".into()),
        fs,
        container: Some(ContainerKind::Vhd),
        dynamic: true,
        ..Default::default()
    }
}

#[test]
fn every_preset_creates_its_vendor_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = tmp.path().join("note.txt");
    fs::write(&payload, b"on a cartridge").unwrap();
    for m in MEDIA_FORMATS {
        let img = tmp.path().join(format!("{}.vhd", m.id));
        Volume::create(&img, NewImage::Media(m), &opts(FsKind::Fat)).unwrap();
        assert_eq!(MediaFormat::find(m.id), Some(m));

        let image = Image::open(&img, &OpenOptions::default()).unwrap();
        assert_eq!(image.container().virtual_size, m.size_bytes(), "{}", m.id);
        let ss = m.sector_size as usize;
        let (start, boot) = match m.layout {
            MediaLayout::Mbr { slot, start } => {
                let Layout::Mbr(parts) = image.layout() else {
                    panic!("{}: not MBR", m.id)
                };
                assert_eq!(parts.len(), 1, "{}", m.id);
                let p = &parts[0];
                assert_eq!(p.index, usize::from(slot), "{}", m.id);
                assert_eq!(p.start_lba, u64::from(start));
                assert_eq!(p.start_lba + p.sectors, m.total_sectors);
                let mbr = image.mbr().unwrap();
                let e = 446 + (usize::from(slot) - 1) * 16;
                assert_eq!(mbr[e], 0, "{}: not active", m.id);
                // Partition type: FAT16 CHS (0x06) while the disk fits in 1024 cylinders.
                let cyl = m.total_sectors / (u64::from(m.heads) * u64::from(m.sectors_per_track));
                assert_eq!(
                    mbr[e + 4],
                    if cyl <= 1024 { 0x06 } else { 0x0E },
                    "{}",
                    m.id
                );
                // CHS of the first sector in the drive's geometry: sector 32 = head 1, sector 1.
                assert_eq!(&mbr[e + 1..e + 4], &[1, 1, 0], "{}", m.id);
                (u64::from(start), image.boot_sector().unwrap())
            }
            MediaLayout::Superfloppy => {
                assert!(matches!(image.layout(), Layout::BareVolume), "{}", m.id);
                (0, image.boot_sector().unwrap())
            }
        };
        drop(image);
        assert_eq!(
            usize::from(le16(&boot, 11)),
            ss,
            "{}: bytes per sector",
            m.id
        );
        assert_eq!(boot[21], m.media, "{}: media byte", m.id);
        assert_eq!(le16(&boot, 24), m.sectors_per_track, "{}", m.id);
        assert_eq!(le16(&boot, 26), m.heads, "{}", m.id);
        assert_eq!(
            u64::from(le32(&boot, 28)),
            start,
            "{}: hidden sectors",
            m.id
        );

        let v = Volume::open(&img, &rw()).unwrap();
        let info = v.info().unwrap();
        assert_eq!(info.media_format, Some(m.name), "{}", m.id);
        assert_eq!(usize::from(info.bytes_per_sector), ss, "{}", m.id);
        assert_eq!(info.label, "CART");
        if m.fat_bits != 0 {
            assert_eq!(info.fs_type, format!("FAT{}", m.fat_bits), "{}", m.id);
        }
        if m.bytes_per_cluster != 0 {
            assert_eq!(info.cluster_size, m.bytes_per_cluster, "{}", m.id);
        }
        v.inject(&payload, "/", false).unwrap();
        v.close().unwrap();
        let v = Volume::open(&img, &OpenOptions::default()).unwrap();
        let mut out = Vec::new();
        v.copy_out("/note.txt", &mut out).unwrap();
        assert_eq!(out, b"on a cartridge", "{}", m.id);
    }
}

#[test]
fn preset_lookup_and_refusals() {
    assert_eq!(MediaFormat::find("Zip-100").unwrap().id, "zip100");
    assert_eq!(MediaFormat::find("LS 120").unwrap().id, "ls120");
    assert!(MediaFormat::find("1440").is_none());
    assert!(sectorzero_core::FloppyFormat::find("zip100").is_none());
    let zip = MediaFormat::find("zip100").unwrap();
    assert_eq!(MediaFormat::matching(zip.size_bytes()), Some(zip));

    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("z.vhd");
    let mut o = opts(FsKind::Fat);
    o.scheme = PartitionScheme::Gpt;
    assert!(Volume::create(&img, NewImage::Media(zip), &o).is_err());
    assert!(!img.exists());
    let mo = MediaFormat::find("mo640").unwrap();
    let err = Volume::create(&img, NewImage::Media(mo), &opts(FsKind::Ext2)).unwrap_err();
    assert!(err.to_string().contains("2048-byte sectors"), "{err}");

    // Other file systems keep the vendor's partition slot.
    Volume::create(&img, NewImage::Media(zip), &opts(FsKind::Exfat)).unwrap();
    let image = Image::open(&img, &OpenOptions::default()).unwrap();
    assert_eq!(image.mbr().unwrap()[446 + 3 * 16 + 4], 0x07);
    drop(image);
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    assert_eq!(v.info().unwrap().fs_type, "exFAT");
    assert!(v.exfat_check().unwrap().is_empty());
    drop(v);
    assert!(Volume::change_format(&img, &OpenOptions::default(), NewImage::Media(zip)).is_err());
}

#[test]
fn a_bare_volume_of_a_media_size_is_recognised() {
    // An image read from a real cartridge has the medium's exact size whatever its contents.
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("ls.vhd");
    let ls = MediaFormat::find("ls120").unwrap();
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: ls.size_bytes(),
        },
        &opts(FsKind::Fat),
    )
    .unwrap();
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    assert_eq!(v.info().unwrap().media_format, Some(ls.name));
}
