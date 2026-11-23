//! Containers (IMZ, VHD, VHDX, VMDK), partition tables (MBR, GPT), conversion, label and
//! boot sector editing, defragmentation, change format, and ISO build/read.

use std::fs;
use std::path::{Path, PathBuf};

use sectorzero_core::{
    BootSectorEdit, ContainerKind, CreateOptions, FloppyFormat, Image, IsoBuildOptions, Layout,
    NewImage, OpenOptions, PartitionScheme, Volume, build_iso, convert, probe,
};

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}
fn ro() -> OpenOptions {
    OpenOptions::default()
}
fn f1440() -> NewImage {
    NewImage::Floppy(FloppyFormat::find("1440").unwrap())
}

fn sample_tree(dir: &Path) -> PathBuf {
    let src = dir.join("tree");
    fs::create_dir_all(src.join("Sub Folder/Deeper")).unwrap();
    fs::write(src.join("readme.txt"), b"hello from SectorZero").unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 253) as u8).collect();
    fs::write(src.join("Sub Folder/big data.bin"), big).unwrap();
    fs::write(src.join("Sub Folder/Deeper/note.md"), b"# deep").unwrap();
    src
}

/// Every file under `a` exists with the same bytes under `b`.
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

fn fill_and_check(img: &Path, tmp: &Path) {
    let src = sample_tree(tmp);
    {
        let v = Volume::open(img, &rw()).unwrap();
        for e in fs::read_dir(&src).unwrap() {
            v.inject(&e.unwrap().path(), "/", false).unwrap();
        }
        v.close().unwrap();
    }
    let out = tmp.join(format!(
        "out-{}",
        img.file_name().unwrap().to_string_lossy()
    ));
    let v = Volume::open(img, &ro()).unwrap();
    v.extract("", &out, false).unwrap();
    same_tree(&src, &out);
}

#[test]
fn every_container_holds_a_floppy_and_a_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let kinds = [
        ("a.ima", false, "raw"),
        ("a.imz", false, "zip"),
        ("fixed.vhd", false, "fixed"),
        ("dyn.vhd", true, "dynamic"),
        ("fixed.vhdx", false, "fixed"),
        ("dyn.vhdx", true, "dynamic"),
        ("flat.vmdk", false, "monolithicFlat"),
        ("sparse.vmdk", true, "monolithicSparse"),
    ];
    for (name, dynamic, variant) in kinds {
        for (spec, tag) in [
            (f1440(), "fd"),
            (
                NewImage::Volume {
                    size_bytes: 40 << 20,
                },
                "hd",
            ),
        ] {
            let path = tmp.path().join(format!("{tag}-{name}"));
            Volume::create(
                &path,
                spec,
                &CreateOptions {
                    label: Some("BOX".into()),
                    dynamic,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            let (c, layout) = probe(&path).unwrap();
            assert_eq!(c.variant, variant, "{name}");
            assert!(matches!(layout, Layout::BareVolume), "{name}");
            fill_and_check(&path, &tmp.path().join(format!("w-{tag}-{name}")));
            let info = Volume::open(&path, &ro()).unwrap().info().unwrap();
            assert_eq!(info.label, "BOX", "{name}");
            assert_eq!(
                info.container.kind,
                ContainerKind::from_path(&path),
                "{name}"
            );
        }
    }
}

#[test]
fn dynamic_containers_stay_small() {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["d.vhd", "d.vhdx", "d.vmdk"] {
        let path = tmp.path().join(name);
        Volume::create(
            &path,
            NewImage::Volume {
                size_bytes: 256 << 20,
            },
            &CreateOptions {
                dynamic: true,
                ..Default::default()
            },
        )
        .unwrap();
        let len = fs::metadata(&path).unwrap().len();
        assert!(len < 24 << 20, "{name} is {len} bytes");
    }
}

#[test]
fn mbr_and_gpt_disks() {
    let tmp = tempfile::tempdir().unwrap();
    for (scheme, name) in [
        (PartitionScheme::Mbr, "mbr.vhd"),
        (PartitionScheme::Gpt, "gpt.vhdx"),
    ] {
        let path = tmp.path().join(name);
        let opts = CreateOptions {
            label: Some("DATA".into()),
            scheme,
            dynamic: true,
            ..Default::default()
        };
        Volume::create(
            &path,
            NewImage::Volume {
                size_bytes: 600 << 20,
            },
            &opts,
        )
        .unwrap();
        let (_, layout) = probe(&path).unwrap();
        let parts = layout.partitions().to_vec();
        assert_eq!(parts.len(), 1, "{name}");
        assert_eq!(parts[0].start_lba, 2048);
        assert_eq!(parts[0].fs, Some("FAT"));
        match (scheme, &layout) {
            (PartitionScheme::Mbr, Layout::Mbr(p)) => assert_eq!(p[0].type_id, "0x0C"),
            (PartitionScheme::Gpt, Layout::Gpt(p)) => {
                assert_eq!(p[0].type_name, "Basic data");
                assert_eq!(p[0].name, "Basic data partition");
            }
            other => panic!("{other:?}"),
        }
        fill_and_check(&path, &tmp.path().join(format!("w-{name}")));
        let v = Volume::open(&path, &ro()).unwrap();
        let info = v.info().unwrap();
        assert_eq!(info.fs_type, "FAT32");
        assert_eq!(info.volume_offset, 2048 * 512);
        let boot = v.image().boot_sector_info().unwrap();
        assert_eq!(boot.hidden_sectors, 2048);
    }
}

#[test]
fn convert_between_all_containers_keeps_the_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.ima");
    Volume::create(&src, f1440(), &CreateOptions::default()).unwrap();
    fill_and_check(&src, &tmp.path().join("w"));
    let original = fs::read(&src).unwrap();
    let mut prev = src.clone();
    for (name, dynamic) in [
        ("a.vhd", true),
        ("b.vhdx", true),
        ("c.vmdk", true),
        ("d.imz", false),
        ("e.vhd", false),
        ("f.vmdk", false),
        ("g.vhdx", false),
        ("h.ima", false),
    ] {
        let dst = tmp.path().join(name);
        convert(&prev, None, &dst, None, dynamic, false).unwrap_or_else(|e| panic!("{name}: {e}"));
        prev = dst;
    }
    assert_eq!(fs::read(&prev).unwrap(), original);
}

#[test]
fn label_and_boot_sector_editing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("l.imz");
    Volume::create(&path, f1440(), &CreateOptions::default()).unwrap();
    let img = Image::open(&path, &rw()).unwrap();
    img.set_label(Some("New Label")).unwrap();
    img.edit_boot_sector(&BootSectorEdit {
        oem_name: Some("MSDOS5.0".into()),
        volume_id: Some("1234-ABCD".into()),
        ..Default::default()
    })
    .unwrap();
    let mut code = img.boot_sector().unwrap();
    code[62..70].copy_from_slice(b"BOOTCODE");
    code[11] = 0; // a different BPB in the file must not leak in
    img.import_boot_sector(&code).unwrap();
    drop(img);

    let v = Volume::open(&path, &ro()).unwrap();
    let info = v.info().unwrap();
    assert_eq!(
        (
            info.label.as_str(),
            info.volume_id.as_str(),
            info.oem_name.as_str()
        ),
        ("NEW LABEL", "1234-ABCD", "MSDOS5.0")
    );
    let b = v.image().boot_sector_info().unwrap();
    assert_eq!(b.label.as_deref(), Some("NEW LABEL"));
    assert_eq!(b.bytes_per_sector, 512);
    assert_eq!(&v.image().boot_sector().unwrap()[62..70], b"BOOTCODE");
    drop(v);

    let img = Image::open(&path, &rw()).unwrap();
    img.set_label(None).unwrap();
    drop(img);
    assert_eq!(Volume::open(&path, &ro()).unwrap().label().unwrap(), "");
}

#[test]
fn defrag_makes_files_contiguous_and_keeps_contents() {
    let tmp = tempfile::tempdir().unwrap();
    let cases = [
        ("f.ima", f1440(), "FAT12"),
        (
            "v.vhd",
            NewImage::Volume {
                size_bytes: 300 << 20,
            },
            "FAT16",
        ),
        (
            "w.vhdx",
            NewImage::Volume {
                size_bytes: 600 << 20,
            },
            "FAT32",
        ),
    ];
    for (name, spec, fat) in cases {
        let path = tmp.path().join(name);
        Volume::create(
            &path,
            spec,
            &CreateOptions {
                dynamic: true,
                ..Default::default()
            },
        )
        .unwrap();
        let work = tmp.path().join(format!("w-{name}"));
        fs::create_dir_all(&work).unwrap();
        // Fragment: many small files, delete every other one, then add big files.
        {
            let v = Volume::open(&path, &rw()).unwrap();
            v.mkdir("/dir").unwrap();
            for i in 0..40 {
                let f = work.join(format!("small{i}.txt"));
                fs::write(&f, vec![i as u8; 3000]).unwrap();
                v.inject(&f, if i % 3 == 0 { "/dir" } else { "/" }, false)
                    .unwrap();
            }
            for i in (0..40).step_by(2) {
                v.remove(
                    &format!("{}/small{i}.txt", if i % 3 == 0 { "dir" } else { "" }),
                    false,
                )
                .unwrap();
            }
            v.close().unwrap();
            // Remount so allocation restarts at the first free cluster (fills the holes).
            let v = Volume::open(&path, &rw()).unwrap();
            assert_eq!(v.info().unwrap().fs_type, fat);
            for i in 0..3 {
                let f = work.join(format!("big{i}.bin"));
                fs::write(
                    &f,
                    (0..150_000u32)
                        .map(|x| (x as u8).wrapping_mul(i + 3))
                        .collect::<Vec<_>>(),
                )
                .unwrap();
                v.inject(&f, "/dir", false).unwrap();
            }
            v.close().unwrap();
        }
        let before = tmp.path().join(format!("before-{name}"));
        Volume::open(&path, &ro())
            .unwrap()
            .extract("", &before, false)
            .unwrap();
        let img = Image::open(&path, &rw()).unwrap();
        let a = img.analyze_fragmentation().unwrap();
        if fat != "FAT32" {
            // FAT32 keeps a next-free hint in FSInfo, so fatfs appends instead of filling holes.
            assert!(a.fragmented > 0, "{name}: nothing fragmented: {a:?}");
        }
        let r = img.defragment(true).unwrap();
        assert!(r.moved_clusters > 0);
        let after = img.analyze_fragmentation().unwrap();
        assert_eq!(after.fragmented, 0, "{name}: {after:?}");
        assert_eq!(after.used_clusters, a.used_clusters);
        drop(img);
        let out = tmp.path().join(format!("after-{name}"));
        let v = Volume::open(&path, &ro()).unwrap();
        v.extract("", &out, false).unwrap();
        same_tree(&before, &out);
        same_tree(&out, &before);
        // The file system is still writable and consistent.
        drop(v);
        let v = Volume::open(&path, &rw()).unwrap();
        v.inject(&work.join("small1.txt"), "/dir", false).unwrap();
        v.close().unwrap();
    }
}

#[test]
fn change_format_keeps_files_metadata_and_container() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cf.imz");
    Volume::create(
        &path,
        f1440(),
        &CreateOptions {
            label: Some("KEEPME".into()),
            ..Default::default()
        },
    )
    .unwrap();
    fill_and_check(&path, &tmp.path().join("w"));
    let before = Volume::open(&path, &ro()).unwrap();
    let (old_info, old_entries) = (before.info().unwrap(), before.walk("/").unwrap());
    drop(before);

    Volume::change_format(
        &path,
        &rw(),
        NewImage::Floppy(FloppyFormat::find("2880").unwrap()),
    )
    .unwrap();
    let v = Volume::open(&path, &ro()).unwrap();
    let info = v.info().unwrap();
    assert_eq!(info.floppy_format, Some("2.88 MB 3.5\" DS/ED"));
    assert_eq!(info.container.kind, ContainerKind::Imz);
    assert_eq!(
        (info.label.as_str(), info.volume_id.as_str()),
        ("KEEPME", old_info.volume_id.as_str())
    );
    let entries = v.walk("/").unwrap();
    assert_eq!(entries.len(), old_entries.len());
    for (a, b) in old_entries.iter().zip(&entries) {
        assert_eq!(
            (&a.path, a.size, &a.modified, a.is_dir),
            (&b.path, b.size, &b.modified, b.is_dir)
        );
    }
    let out = tmp.path().join("cf-out");
    v.extract("", &out, false).unwrap();
    same_tree(&tmp.path().join("w/tree"), &out);
    drop(v);

    // Too small: refused, and the original is untouched.
    let len = fs::metadata(&path).unwrap().len();
    let err = Volume::change_format(
        &path,
        &rw(),
        NewImage::Floppy(FloppyFormat::find("160").unwrap()),
    )
    .unwrap_err();
    assert!(err.to_string().contains("do not fit"), "{err}");
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
    assert_eq!(
        fs::read_dir(tmp.path())
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".s0-"))
            .count(),
        0
    );
}

#[test]
fn iso_build_and_read_back_with_el_torito() {
    let tmp = tempfile::tempdir().unwrap();
    let src = sample_tree(tmp.path());
    fs::write(
        src.join("A very long file name, with spaces.txt"),
        b"joliet",
    )
    .unwrap();
    let boot = tmp.path().join("boot.ima");
    Volume::create(
        &boot,
        f1440(),
        &CreateOptions {
            label: Some("BOOTFLOP".into()),
            ..Default::default()
        },
    )
    .unwrap();

    let iso = tmp.path().join("cd.iso");
    let r = build_iso(
        &src,
        &iso,
        &IsoBuildOptions {
            volume_id: "My Disc".into(),
            boot_image: Some(boot.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(r.bootable);
    assert_eq!(r.files, 4);

    let (_, layout) = probe(&iso).unwrap();
    assert!(matches!(layout, Layout::Iso));
    let v = Volume::open(&iso, &ro()).unwrap();
    let info = v.info().unwrap();
    assert_eq!(info.fs_type, "ISO 9660 + Joliet");
    assert_eq!(info.label, "My Disc");
    assert!(!info.fs_writable);
    let iso_info = info.iso.unwrap();
    assert_eq!(iso_info.boot_entries.len(), 1);
    assert_eq!(iso_info.boot_entries[0].media, "1.44 MB floppy");
    assert_eq!(v.boot_image(1).unwrap(), fs::read(&boot).unwrap());
    let names: Vec<String> = v.list("/").unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        [
            "Sub Folder",
            "A very long file name, with spaces.txt",
            "readme.txt"
        ]
    );
    assert!(v.stat("sub folder/deeper/NOTE.md").unwrap().is_some());
    let out = tmp.path().join("iso-out");
    v.extract("", &out, false).unwrap();
    same_tree(&src, &out);
    assert!(v.mkdir("/x").is_err());

    // The El Torito floppy is a normal image.
    let floppy = tmp.path().join("extracted.ima");
    fs::write(&floppy, v.boot_image(1).unwrap()).unwrap();
    assert_eq!(
        Volume::open(&floppy, &ro()).unwrap().label().unwrap(),
        "BOOTFLOP"
    );
}
