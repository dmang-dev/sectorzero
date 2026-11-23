//! Additional containers: VHD differencing / undo disks, VDI, QCOW2, split raw sets, PC-98
//! NHD/HDI/FDI, and the read-only CD images (BIN/CUE, NRG, MDF/MDS).

use std::fs;
use std::path::{Path, PathBuf};

use sectorzero_core::{
    ContainerKind, CreateOptions, Error, FloppyFormat, IsoBuildOptions, Layout, NewImage,
    OpenOptions, Volume, build_iso, convert, create_differencing, probe,
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

fn inject_tree(img: &Path, src: &Path) {
    let v = Volume::open(img, &rw()).unwrap();
    for e in fs::read_dir(src).unwrap() {
        v.inject(&e.unwrap().path(), "/", false).unwrap();
    }
    v.close().unwrap();
}

fn extract_and_compare(img: &Path, src: &Path, out: &Path) {
    let v = Volume::open(img, &ro()).unwrap();
    v.extract("", out, false).unwrap();
    same_tree(src, out);
}

fn fill_and_check(img: &Path, tmp: &Path) {
    let src = sample_tree(tmp);
    inject_tree(img, &src);
    extract_and_compare(img, &src, &tmp.join("out"));
}

#[test]
fn new_containers_hold_a_floppy_and_a_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let kinds = [
        ("fixed.vdi", false, Some("fixed")),
        ("dyn.vdi", true, Some("dynamic")),
        ("a.qcow2", false, Some("v3")),
        ("a.nhd", false, None),
        ("a.hdi", false, None),
        ("a.fdi", false, None),
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
            assert_eq!(c.kind, ContainerKind::from_path(&path), "{name}");
            if let Some(v) = variant {
                assert_eq!(c.variant, v, "{name}");
            }
            assert!(matches!(layout, Layout::BareVolume), "{name}");
            fill_and_check(&path, &tmp.path().join(format!("w-{tag}-{name}")));
            let info = Volume::open(&path, &ro()).unwrap().info().unwrap();
            assert_eq!(info.label, "BOX", "{name}");
        }
    }
}

#[test]
fn sparse_new_containers_stay_small() {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["d.vdi", "d.qcow2"] {
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
fn convert_through_new_containers_keeps_the_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.ima");
    Volume::create(&src, f1440(), &CreateOptions::default()).unwrap();
    fill_and_check(&src, &tmp.path().join("w"));
    let original = fs::read(&src).unwrap();
    let mut prev = src.clone();
    for (name, dynamic) in [
        ("a.vdi", true),
        ("b.qcow2", false),
        ("c.nhd", false),
        ("d.hdi", false),
        ("e.fdi", false),
        ("f.vdi", false),
        ("g.vhd", true),
        ("h.ima", false),
    ] {
        let dst = tmp.path().join(name);
        convert(&prev, None, &dst, None, dynamic, false).unwrap_or_else(|e| panic!("{name}: {e}"));
        prev = dst;
    }
    assert_eq!(fs::read(&prev).unwrap(), original);
}

#[test]
fn differencing_vhd_and_undo_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base.vhd");
    Volume::create(
        &base,
        NewImage::Volume {
            size_bytes: 64 << 20,
        },
        &CreateOptions {
            label: Some("BASE".into()),
            dynamic: true,
            ..Default::default()
        },
    )
    .unwrap();
    let first = sample_tree(&tmp.path().join("one"));
    inject_tree(&base, &first);
    let base_bytes = fs::read(&base).unwrap();

    for child_name in ["child.vhd", "Undo.vud"] {
        let child = tmp.path().join("undo").join(child_name);
        fs::create_dir_all(child.parent().unwrap()).unwrap();
        let info = create_differencing(&child, &base, false).unwrap();
        assert_eq!(info.kind, ContainerKind::Vhd);
        assert_eq!(info.variant, "differencing");
        assert!(matches!(
            create_differencing(&child, &base, false),
            Err(Error::Exists(_))
        ));
        let (c, layout) = probe(&child).unwrap();
        assert_eq!(c.variant, "differencing");
        assert!(c.parent.as_deref().unwrap().ends_with("base.vhd"));
        assert!(matches!(layout, Layout::BareVolume));
        // The child sees the parent's files; new files go to the child only.
        extract_and_compare(&child, &first, &tmp.path().join(format!("o1-{child_name}")));
        let extra = tmp.path().join(format!("extra-{child_name}"));
        fs::create_dir_all(&extra).unwrap();
        fs::write(extra.join("new.txt"), b"only in the child").unwrap();
        {
            let v = Volume::open(&child, &rw()).unwrap();
            v.inject(&extra.join("new.txt"), "/", false).unwrap();
            v.remove("/readme.txt", false).unwrap();
            v.close().unwrap();
        }
        assert_eq!(fs::read(&base).unwrap(), base_bytes, "parent untouched");
        let v = Volume::open(&child, &ro()).unwrap();
        assert!(v.stat("/new.txt").unwrap().is_some());
        assert!(v.stat("/readme.txt").unwrap().is_none());
        assert!(v.stat("/Sub Folder/big data.bin").unwrap().is_some());
        drop(v);
        let v = Volume::open(&base, &ro()).unwrap();
        assert!(v.stat("/new.txt").unwrap().is_none());
        assert!(v.stat("/readme.txt").unwrap().is_some());
        drop(v);
        // Flattening to a standalone image keeps the merged view.
        let flat = tmp.path().join(format!("flat-{child_name}.vhdx"));
        convert(&child, None, &flat, None, true, false).unwrap();
        let v = Volume::open(&flat, &ro()).unwrap();
        assert!(v.stat("/new.txt").unwrap().is_some());
        assert!(v.stat("/readme.txt").unwrap().is_none());
    }
    // A differencing disk cannot be rebuilt in place.
    let child = tmp.path().join("undo").join("child.vhd");
    assert!(matches!(
        Volume::change_format(&child, &ro(), f1440()),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn split_raw_set() {
    let tmp = tempfile::tempdir().unwrap();
    let whole = tmp.path().join("whole.ima");
    Volume::create(&whole, f1440(), &CreateOptions::default()).unwrap();
    let data = fs::read(&whole).unwrap();
    let first = tmp.path().join("disk.001");
    let cuts = [0, 500_000, 1_000_000, data.len()];
    for i in 0..3 {
        fs::write(
            tmp.path().join(format!("disk.{:03}", i + 1)),
            &data[cuts[i]..cuts[i + 1]],
        )
        .unwrap();
    }
    let (c, layout) = probe(&first).unwrap();
    assert_eq!(c.kind, ContainerKind::Split);
    assert_eq!(c.virtual_size, data.len() as u64);
    assert!(matches!(layout, Layout::BareVolume));
    fill_and_check(&first, &tmp.path().join("w"));
    for i in 0..3 {
        let len = fs::metadata(tmp.path().join(format!("disk.{:03}", i + 1)))
            .unwrap()
            .len();
        assert_eq!(len as usize, cuts[i + 1] - cuts[i], "piece {}", i + 1);
    }
    // Joined into one file, it is the same volume.
    let joined = tmp.path().join("joined.ima");
    convert(&first, None, &joined, None, false, false).unwrap();
    extract_and_compare(
        &joined,
        &tmp.path().join("w").join("tree"),
        &tmp.path().join("o2"),
    );
    assert!(matches!(
        Volume::create(
            &tmp.path().join("new.001"),
            f1440(),
            &CreateOptions::default()
        ),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn hdi_and_fdi_need_a_real_header() {
    let tmp = tempfile::tempdir().unwrap();
    // Plain raw images that only carry the extension stay raw.
    for name in ["plain.hdi", "plain.fdi"] {
        let path = tmp.path().join(name);
        Volume::create(
            &path,
            f1440(),
            &CreateOptions {
                container: Some(ContainerKind::Raw),
                ..Default::default()
            },
        )
        .unwrap();
        let (c, layout) = probe(&path).unwrap();
        assert_eq!(c.kind, ContainerKind::Raw, "{name}");
        assert!(matches!(layout, Layout::BareVolume));
    }
    // A real FDI renamed to .img is raw too (header bytes and all), but an NHD is found by
    // its signature whatever the name.
    let fdi = tmp.path().join("real.fdi");
    Volume::create(&fdi, f1440(), &CreateOptions::default()).unwrap();
    let renamed = tmp.path().join("real.img");
    fs::copy(&fdi, &renamed).unwrap();
    assert_eq!(probe(&renamed).unwrap().0.kind, ContainerKind::Raw);
    let nhd = tmp.path().join("disk.nhd");
    Volume::create(&nhd, f1440(), &CreateOptions::default()).unwrap();
    let renamed = tmp.path().join("disk.bin");
    fs::copy(&nhd, &renamed).unwrap();
    assert_eq!(probe(&renamed).unwrap().0.kind, ContainerKind::Nhd);
}

// --- CD images ----------------------------------------------------------------------------

const SYNC: [u8; 12] = [
    0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0,
];

/// Cooked 2048-byte sectors as raw 2352-byte Mode 1 or Mode 2 form 1 sectors.
fn to_raw(iso: &[u8], mode: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(iso.len() / 2048 * 2352);
    for (lba, s) in iso.chunks(2048).enumerate() {
        let mut sec = vec![0u8; 2352];
        sec[..12].copy_from_slice(&SYNC);
        let abs = lba as u64 + 150;
        let bcd = |v: u64| ((v / 10) << 4 | (v % 10)) as u8;
        sec[12] = bcd(abs / 75 / 60);
        sec[13] = bcd(abs / 75 % 60);
        sec[14] = bcd(abs % 75);
        sec[15] = mode;
        let user = if mode == 1 { 16 } else { 24 };
        sec[user..user + s.len()].copy_from_slice(s);
        out.extend_from_slice(&sec);
    }
    out
}

#[test]
fn cd_images_expose_the_iso_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let src = sample_tree(tmp.path());
    let iso = tmp.path().join("disc.iso");
    build_iso(
        &src,
        &iso,
        &IsoBuildOptions {
            volume_id: "CDTEST".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let cooked = fs::read(&iso).unwrap();

    let mut images: Vec<(PathBuf, ContainerKind, &str)> = Vec::new();
    for (mode, data, tag) in [
        ("MODE1/2352", to_raw(&cooked, 1), "m1"),
        ("MODE2/2352", to_raw(&cooked, 2), "m2"),
        ("MODE1/2048", cooked.clone(), "cooked"),
    ] {
        fs::write(tmp.path().join(format!("{tag}.bin")), &data).unwrap();
        let cue = tmp.path().join(format!("{tag}.cue"));
        fs::write(
            &cue,
            format!("FILE \"{tag}.bin\" BINARY\n  TRACK 01 {mode}\n    INDEX 01 00:00:00\n"),
        )
        .unwrap();
        images.push((cue, ContainerKind::Cue, mode));
    }
    // A raw BIN opened without its cue sheet.
    images.push((tmp.path().join("m2.bin"), ContainerKind::Cue, "MODE2/2352"));
    // Nero, track-at-once, version 2 footer.
    let mut nrg = cooked.clone();
    let chunks_at = nrg.len() as u64;
    let mut etn = vec![0u8; 32];
    etn[8..16].copy_from_slice(&(cooked.len() as u64).to_be_bytes());
    nrg.extend_from_slice(b"ETN2");
    nrg.extend_from_slice(&32u32.to_be_bytes());
    nrg.extend_from_slice(&etn);
    nrg.extend_from_slice(b"END!\0\0\0\0");
    nrg.extend_from_slice(b"NER5");
    nrg.extend_from_slice(&chunks_at.to_be_bytes());
    let nrg_path = tmp.path().join("disc.nrg");
    fs::write(&nrg_path, &nrg).unwrap();
    images.push((nrg_path, ContainerKind::Nrg, "MODE1/2048"));
    // An MDF without its descriptor (raw sectors with subchannel data).
    let mut mdf = Vec::new();
    for s in to_raw(&cooked, 1).chunks(2352) {
        mdf.extend_from_slice(s);
        mdf.extend_from_slice(&[0u8; 96]);
    }
    let mdf_path = tmp.path().join("disc.mdf");
    fs::write(&mdf_path, &mdf).unwrap();
    images.push((mdf_path, ContainerKind::Mds, "MODE1/2448"));

    for (i, (path, kind, variant)) in images.iter().enumerate() {
        let (c, layout) = probe(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(c.kind, *kind, "{}", path.display());
        assert_eq!(c.variant, *variant, "{}", path.display());
        assert_eq!(c.virtual_size, cooked.len() as u64);
        assert!(matches!(layout, Layout::Iso), "{}", path.display());
        let v = Volume::open(path, &ro()).unwrap();
        assert_eq!(v.info().unwrap().label, "CDTEST");
        let out = tmp.path().join(format!("out{i}"));
        v.extract("", &out, false).unwrap();
        same_tree(&src, &out);
        drop(v);
        assert!(
            matches!(Volume::open(path, &rw()), Err(Error::Unsupported(_))),
            "{} must refuse writing",
            path.display()
        );
        // Converting a CD image gives a plain ISO.
        let iso_out = tmp.path().join(format!("conv{i}.iso"));
        convert(path, None, &iso_out, None, false, false).unwrap();
        assert_eq!(fs::read(&iso_out).unwrap(), cooked);
    }
    for name in ["new.cue", "new.nrg", "new.mds"] {
        assert!(matches!(
            convert(&iso, None, &tmp.path().join(name), None, false, false),
            Err(Error::Unsupported(_))
        ));
    }
}
