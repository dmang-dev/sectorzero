//! Custom geometry, file attributes and dates, checksums, listing export, truncation and
//! in-place ISO file replacement.

use std::fs;

use sectorzero_core::{
    ContainerKind, CreateOptions, EntryEdit, FloppyFormat, Geometry, Image, IsoBuildOptions,
    ListingFormat, NewImage, OpenOptions, Volume, build_iso, checksum_file, convert,
    render_listing, truncate_image,
};

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}

fn floppy_with_files(dir: &std::path::Path) -> std::path::PathBuf {
    let img = dir.join("disk.ima");
    Volume::create(
        &img,
        NewImage::Floppy(FloppyFormat::find("1440").unwrap()),
        &CreateOptions::default(),
    )
    .unwrap();
    let src = dir.join("src");
    fs::create_dir_all(src.join("Folder")).unwrap();
    fs::write(src.join("hello.txt"), b"hello").unwrap();
    fs::write(src.join("Folder/inner.bin"), vec![7u8; 5000]).unwrap();
    let v = Volume::open(&img, &rw()).unwrap();
    v.inject(&src.join("hello.txt"), "", false).unwrap();
    v.inject(&src.join("Folder"), "", false).unwrap();
    v.close().unwrap();
    img
}

#[test]
fn custom_geometry_is_written_as_asked_and_can_be_copied() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("custom.ima");
    let g = Geometry {
        total_sectors: 1600,
        sectors_per_track: 10,
        heads: 2,
        bytes_per_cluster: 1024,
        root_entries: 64,
        fats: 1,
        media: 0xF9,
        fat_bits: 12,
        hidden_sectors: 5,
        drive_number: 0x00,
    };
    Volume::create(&img, NewImage::Custom(g), &CreateOptions::default()).unwrap();
    assert_eq!(fs::metadata(&img).unwrap().len(), 1600 * 512);
    let boot = Image::open(&img, &OpenOptions::default())
        .unwrap()
        .boot_sector()
        .unwrap();
    let back = Geometry::from_boot_sector(&boot).unwrap();
    assert_eq!(back, g);
    let info = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .info()
        .unwrap();
    assert_eq!(info.fs_type, "FAT12");
    assert_eq!(info.cluster_size, 1024);

    // Copy the geometry into a VHD and resize a standard floppy into it.
    let vhd = tmp.path().join("copy.vhd");
    Volume::create(&vhd, NewImage::Custom(back), &CreateOptions::default()).unwrap();
    let floppy = floppy_with_files(tmp.path());
    Volume::change_format(&floppy, &rw(), NewImage::Custom(g)).unwrap();
    let v = Volume::open(&floppy, &OpenOptions::default()).unwrap();
    assert_eq!(v.walk("").unwrap().len(), 3);

    // Nonsense is refused before anything is written.
    let bad = Geometry { fats: 3, ..g };
    let err = Volume::create(
        &tmp.path().join("bad.ima"),
        NewImage::Custom(bad),
        &CreateOptions::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("FATs"), "{err}");
}

#[test]
fn attributes_and_dates_can_be_edited() {
    let tmp = tempfile::tempdir().unwrap();
    let img = floppy_with_files(tmp.path());
    let image = Image::open(&img, &rw()).unwrap();
    let edit = EntryEdit {
        read_only: Some(true),
        hidden: Some(true),
        archive: Some(false),
        modified: Some("1995-08-24 09:30:15".into()),
        created: Some("1994-01-02 03:04:05".into()),
        accessed: Some("1996-07-08".into()),
        ..Default::default()
    };
    // Short (8.3) names work as well as long ones.
    assert_eq!(image.edit_entry("HELLO.TXT", &edit, false).unwrap(), 1);
    drop(image);
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    let e = v.stat("hello.txt").unwrap().unwrap();
    assert!(e.attributes.read_only && e.attributes.hidden && !e.attributes.archive);
    // FAT keeps modified times in 2-second steps; creation times keep the odd second.
    assert_eq!(e.modified, "1995-08-24 09:30:14");
    assert_eq!(e.created, "1994-01-02 03:04:05");
    assert_eq!(e.accessed, "1996-07-08");
    drop(v);

    // Recursive from the root touches every entry.
    let image = Image::open(&img, &rw()).unwrap();
    let all = EntryEdit {
        system: Some(true),
        ..Default::default()
    };
    assert_eq!(image.edit_entry("/", &all, true).unwrap(), 3);
    assert!(
        image
            .edit_entry(
                "hello.txt",
                &EntryEdit {
                    modified: Some("1979-12-31".into()),
                    ..Default::default()
                },
                false
            )
            .is_err()
    );
    assert!(image.edit_entry("nope.txt", &all, false).is_err());
    drop(image);
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    assert!(v.walk("").unwrap().iter().all(|e| e.attributes.system));
    // The file contents are untouched.
    let mut out = Vec::new();
    v.copy_out("Folder/inner.bin", &mut out).unwrap();
    assert_eq!(out, vec![7u8; 5000]);
}

#[test]
fn disk_checksums_do_not_depend_on_the_container() {
    let tmp = tempfile::tempdir().unwrap();
    let img = floppy_with_files(tmp.path());
    let raw = Image::open(&img, &OpenOptions::default())
        .unwrap()
        .checksum_disk()
        .unwrap();
    assert_eq!(raw.bytes, 1_474_560);
    assert_eq!(raw, checksum_file(&img).unwrap());
    for ext in ["imz", "vhd", "vhdx", "vmdk"] {
        let other = tmp.path().join(format!("copy.{ext}"));
        convert(&img, None, &other, None, true, false).unwrap();
        let c = Image::open(&other, &OpenOptions::default())
            .unwrap()
            .checksum_disk()
            .unwrap();
        assert_eq!(c, raw, "{ext}");
        assert_ne!(checksum_file(&other).unwrap().sha256, raw.sha256, "{ext}");
    }
    let f = Volume::open(&img, &OpenOptions::default())
        .unwrap()
        .checksum("hello.txt")
        .unwrap();
    assert_eq!(
        f.sha256,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn truncation_keeps_every_file_and_the_checksum() {
    let tmp = tempfile::tempdir().unwrap();
    let img = floppy_with_files(tmp.path());
    let before = Image::open(&img, &OpenOptions::default())
        .unwrap()
        .checksum_disk()
        .unwrap();
    let r = truncate_image(&img).unwrap();
    assert_eq!(r.old_size, 1_474_560);
    assert!(r.new_size < 40_000, "{}", r.new_size);
    assert_eq!(fs::metadata(&img).unwrap().len(), r.new_size);
    let after = Image::open(&img, &OpenOptions::default())
        .unwrap()
        .checksum_disk()
        .unwrap();
    assert_eq!(after, before);
    // Still writable: a new file goes past the cut and the image grows again.
    let v = Volume::open(&img, &rw()).unwrap();
    let big = tmp.path().join("big.bin");
    fs::write(&big, vec![1u8; 300_000]).unwrap();
    v.inject(&big, "", false).unwrap();
    v.close().unwrap();
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    assert_eq!(v.stat("big.bin").unwrap().unwrap().size, 300_000);
    assert_eq!(v.stat("hello.txt").unwrap().unwrap().size, 5);
    drop(v);
    // Only raw bare images can be truncated.
    let vhd = tmp.path().join("d.vhd");
    convert(&img, None, &vhd, Some(ContainerKind::Vhd), true, false).unwrap();
    assert!(truncate_image(&vhd).is_err());
}

#[test]
fn listing_export_covers_every_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let img = floppy_with_files(tmp.path());
    let v = Volume::open(&img, &OpenOptions::default()).unwrap();
    let entries = v.walk("").unwrap();
    let csv = render_listing("disk.ima", &entries, ListingFormat::Csv);
    assert_eq!(csv.lines().count(), 4);
    assert!(csv.contains("Folder/inner.bin,file,5000,"));
    let html = render_listing("disk.ima", &entries, ListingFormat::Html);
    assert!(html.starts_with("<!doctype html>") && html.contains("Folder/inner.bin"));
}

#[test]
fn iso_files_can_be_replaced_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("cd");
    fs::create_dir_all(src.join("DOCS")).unwrap();
    fs::write(src.join("DOCS/Long Name Readme.txt"), vec![b'a'; 3000]).unwrap();
    fs::write(src.join("other.txt"), b"keep me").unwrap();
    let iso = tmp.path().join("cd.iso");
    build_iso(&src, &iso, &IsoBuildOptions::default()).unwrap();

    let image = Image::open(&iso, &rw()).unwrap();
    // 3000 bytes occupy two 2048-byte sectors, so up to 4096 bytes fit.
    image
        .iso_replace_file("DOCS/Long Name Readme.txt", &[b'b'; 4096])
        .unwrap();
    assert!(
        image
            .iso_replace_file("DOCS/Long Name Readme.txt", &[0u8; 4097])
            .is_err()
    );
    assert!(image.iso_replace_file("DOCS", b"x").is_err());
    drop(image);

    let v = Volume::open(&iso, &OpenOptions::default()).unwrap();
    let mut out = Vec::new();
    v.copy_out("DOCS/Long Name Readme.txt", &mut out).unwrap();
    assert_eq!(out, vec![b'b'; 4096]);
    let mut other = Vec::new();
    v.copy_out("other.txt", &mut other).unwrap();
    assert_eq!(other, b"keep me");
    // Read-only images refuse.
    let ro = Image::open(&iso, &OpenOptions::default()).unwrap();
    assert!(ro.iso_replace_file("other.txt", b"x").is_err());
}
