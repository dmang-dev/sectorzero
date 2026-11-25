//! Differencing disks and snapshot chains: VHDX (AVHDX), VMDK deltas, VDI differencing.

use std::fs;
use std::path::Path;

use sectorzero_core::{
    ContainerKind, CreateOptions, Error, FloppyFormat, NewImage, OpenOptions, Volume,
    checksum_virtual_disk, convert, create_differencing, probe,
};

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}

fn put(img: &Path, name: &str, data: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let host = dir.path().join(name);
    fs::write(&host, data).unwrap();
    Volume::open(img, &rw())
        .unwrap()
        .inject(&host, "/", false)
        .unwrap();
}

fn has(img: &Path, name: &str) -> bool {
    Volume::open(img, &OpenOptions::default())
        .unwrap()
        .stat(name)
        .unwrap()
        .is_some()
}

fn disk(path: &Path, size: u64) {
    Volume::create(
        path,
        NewImage::Volume { size_bytes: size },
        &CreateOptions {
            dynamic: true,
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn differencing_vhdx_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base.vhdx");
    disk(&base, 40 << 20);
    put(&base, "BASE.TXT", b"in the base");
    let base_sum = checksum_virtual_disk(&base, None).unwrap().sha256;
    let base_bytes = fs::read(&base).unwrap();

    // A checkpoint in a subfolder, then a second checkpoint on top of it.
    let snap = tmp.path().join("Snapshots");
    fs::create_dir_all(&snap).unwrap();
    let c1 = snap.join("vm_1.avhdx");
    let info = create_differencing(&c1, &base, false).unwrap();
    assert_eq!(info.kind, ContainerKind::Vhdx);
    assert_eq!(info.variant, "differencing");
    assert_eq!(checksum_virtual_disk(&c1, None).unwrap().sha256, base_sum);
    put(&c1, "ONE.TXT", &vec![b'1'; 300_000]);
    let c2 = snap.join("vm_2.avhdx");
    create_differencing(&c2, &c1, false).unwrap();
    put(&c2, "TWO.TXT", b"second checkpoint");
    Volume::open(&c2, &rw())
        .unwrap()
        .remove("/BASE.TXT", false)
        .unwrap();

    assert_eq!(fs::read(&base).unwrap(), base_bytes, "base untouched");
    assert!(has(&base, "/BASE.TXT") && !has(&base, "/ONE.TXT"));
    assert!(has(&c1, "/BASE.TXT") && has(&c1, "/ONE.TXT") && !has(&c1, "/TWO.TXT"));
    assert!(!has(&c2, "/BASE.TXT") && has(&c2, "/ONE.TXT") && has(&c2, "/TWO.TXT"));
    let (c, _) = probe(&c2).unwrap();
    assert!(c.parent.unwrap().ends_with("vm_1.avhdx"));

    // Flattening keeps the merged view.
    let flat = tmp.path().join("flat.img");
    convert(&c2, None, &flat, None, false, false).unwrap();
    assert!(has(&flat, "/TWO.TXT") && has(&flat, "/ONE.TXT") && !has(&flat, "/BASE.TXT"));
    assert_eq!(
        checksum_virtual_disk(&flat, None).unwrap().sha256,
        checksum_virtual_disk(&c2, None).unwrap().sha256
    );

    // The chain still opens after the whole folder moves (relative locators).
    let moved = tmp.path().join("moved");
    fs::create_dir_all(moved.join("Snapshots")).unwrap();
    fs::rename(&base, moved.join("base.vhdx")).unwrap();
    for n in ["vm_1.avhdx", "vm_2.avhdx"] {
        fs::rename(snap.join(n), moved.join("Snapshots").join(n)).unwrap();
    }
    assert!(has(&moved.join("Snapshots/vm_2.avhdx"), "/TWO.TXT"));

    // A VHDX child needs a VHDX parent, and a differencing disk is not rebuilt in place.
    let vhd = tmp.path().join("p.vhd");
    disk(&vhd, 8 << 20);
    assert!(create_differencing(&tmp.path().join("x.vhdx"), &vhd, false).is_err());
    assert!(matches!(
        Volume::change_format(
            &moved.join("Snapshots/vm_2.avhdx"),
            &OpenOptions::default(),
            NewImage::Floppy(FloppyFormat::find("1440").unwrap())
        ),
        Err(Error::Unsupported(_))
    ));
    // Writing to the parent afterwards breaks the link, and the child says so.
    put(&moved.join("base.vhdx"), "LATER.TXT", b"x");
    let err = Volume::open(&moved.join("Snapshots/vm_1.avhdx"), &OpenOptions::default())
        .err()
        .unwrap();
    assert!(err.to_string().contains("DataWriteGuid"), "{err}");
}

#[test]
fn vmdk_snapshot_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base.vmdk");
    disk(&base, 40 << 20);
    put(&base, "BASE.TXT", b"in the base");
    let base_bytes = fs::read(&base).unwrap();
    let snap = tmp.path().join("snaps").join("base-000001.vmdk");
    fs::create_dir_all(snap.parent().unwrap()).unwrap();
    let info = create_differencing(&snap, &base, false).unwrap();
    assert_eq!(info.kind, ContainerKind::Vmdk);
    assert!(info.parent.is_some());
    put(&snap, "SNAP.TXT", &vec![7u8; 200_000]);
    assert!(has(&snap, "/BASE.TXT") && has(&snap, "/SNAP.TXT"));
    assert!(!has(&base, "/SNAP.TXT"));
    assert_eq!(fs::read(&base).unwrap(), base_bytes, "base untouched");
    let flat = tmp.path().join("flat.vhd");
    convert(&snap, None, &flat, None, true, false).unwrap();
    assert_eq!(
        checksum_virtual_disk(&flat, None).unwrap().sha256,
        checksum_virtual_disk(&snap, None).unwrap().sha256
    );
    // Changing the base gives it a new CID, so the snapshot refuses the stale parent.
    put(&base, "LATER.TXT", b"x");
    let err = Volume::open(&snap, &OpenOptions::default()).err().unwrap();
    assert!(err.to_string().contains("CID"), "{err}");
}

/// `tests/data/qemu-stream.vmdk`: a 1.44 MB FAT12 floppy with NOTE.TXT, converted by
/// `qemu-img convert -O vmdk -o subformat=streamOptimized` (QEMU 11.1).
const STREAM_SHA256: &str = "62563d444c70004f5900fa94eec7569efb98e815b1090e550a58a12516a941fa";

fn stream_fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/qemu-stream.vmdk")
}

#[test]
fn stream_optimized_vmdk_reads_and_refuses_writes() {
    let img = stream_fixture();
    assert_eq!(
        checksum_virtual_disk(&img, None).unwrap().sha256,
        STREAM_SHA256
    );
    let (info, _) = probe(&img).unwrap();
    assert_eq!(info.variant, "streamOptimized");
    assert!(has(&img, "/NOTE.TXT"));
    let tmp = tempfile::tempdir().unwrap();
    let copy = tmp.path().join("s.vmdk");
    fs::copy(&img, &copy).unwrap();
    let host = tmp.path().join("X.TXT");
    fs::write(&host, b"x").unwrap();
    assert!(
        Volume::open(&copy, &rw())
            .unwrap()
            .inject(&host, "/", false)
            .is_err()
    );
    assert_eq!(fs::read(&copy).unwrap(), fs::read(&img).unwrap());
}

/// A ustar member header (name, size, regular file).
fn tar_header(name: &str, size: usize) -> Vec<u8> {
    let mut h = vec![0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..107].copy_from_slice(b"0000644");
    h[108..115].copy_from_slice(b"0000000");
    h[116..123].copy_from_slice(b"0000000");
    h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
    h[136..147].copy_from_slice(b"00000000000");
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    h
}

#[test]
fn ova_opens_its_disk_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let ova = tmp.path().join("appliance.ova");
    let ovf = b"<?xml version=\"1.0\"?><Envelope/>";
    let vmdk = fs::read(stream_fixture()).unwrap();
    let mut tar = Vec::new();
    for (name, data) in [
        ("appliance.ovf", &ovf[..]),
        ("appliance-disk1.vmdk", &vmdk[..]),
    ] {
        tar.extend(tar_header(name, data.len()));
        tar.extend_from_slice(data);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.extend(vec![0u8; 1024]);
    fs::write(&ova, &tar).unwrap();
    let (info, _) = probe(&ova).unwrap();
    assert_eq!(info.kind, ContainerKind::Vmdk);
    assert_eq!(info.compression.as_deref(), Some("tar"));
    assert_eq!(info.member.as_deref(), Some("appliance-disk1.vmdk"));
    assert_eq!(
        checksum_virtual_disk(&ova, None).unwrap().sha256,
        STREAM_SHA256
    );
    let out = tmp.path().join("disk.vhdx");
    convert(&ova, None, &out, None, true, false).unwrap();
    assert_eq!(
        checksum_virtual_disk(&out, None).unwrap().sha256,
        STREAM_SHA256
    );
}
