//! Compressed images: .gz, .xz, .zst, .bz2, and .zip / .7z archives holding one image.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use sectorzero_core::{
    ContainerKind, CreateOptions, FloppyFormat, NewImage, OpenOptions, Volume,
    checksum_virtual_disk, convert, probe,
};

const KINDS: [&str; 6] = ["img.gz", "img.xz", "img.zst", "img.bz2", "zip", "7z"];

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}

fn floppy(path: &Path) {
    Volume::create(
        path,
        NewImage::Floppy(FloppyFormat::find("1440").unwrap()),
        &CreateOptions::default(),
    )
    .unwrap();
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

fn get(img: &Path, name: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    Volume::open(img, &OpenOptions::default())
        .unwrap()
        .extract(name, dir.path(), false)
        .unwrap();
    fs::read(dir.path().join(name)).unwrap()
}

#[test]
fn create_edit_and_reopen_every_compression() {
    let tmp = tempfile::tempdir().unwrap();
    let text = b"hello from a compressed floppy\r\n".repeat(200);
    for ext in KINDS {
        let img = tmp.path().join(format!("disk.{ext}"));
        floppy(&img);
        put(&img, "HELLO.TXT", &text);
        assert_eq!(get(&img, "HELLO.TXT"), text, "{ext}");
        let (info, _) = probe(&img).unwrap();
        assert_eq!(info.kind, ContainerKind::Raw, "{ext}");
        assert_eq!(info.virtual_size, 1_474_560, "{ext}");
        assert!(info.compression.is_some(), "{ext}");
        // Much smaller than the 1.44 MB disk, and no temporary files left behind.
        assert!(fs::metadata(&img).unwrap().len() < 200_000, "{ext}");
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.contains("s0-tmp")), "{names:?}");
    }
}

#[test]
fn compressed_images_hold_the_same_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain.img");
    floppy(&plain);
    put(
        &plain,
        "DATA.BIN",
        &(0..60_000u32).map(|i| (i * 7) as u8).collect::<Vec<_>>(),
    );
    let want = checksum_virtual_disk(&plain, None).unwrap();
    let bytes = fs::read(&plain).unwrap();
    for ext in KINDS {
        let packed = tmp.path().join(format!("packed.{ext}"));
        convert(&plain, None, &packed, None, false, false).unwrap();
        assert_eq!(
            checksum_virtual_disk(&packed, None).unwrap().sha256,
            want.sha256,
            "{ext}"
        );
        let back = tmp
            .path()
            .join(format!("back-{}.img", ext.replace('.', "-")));
        convert(&packed, None, &back, None, false, false).unwrap();
        assert_eq!(fs::read(&back).unwrap(), bytes, "{ext}");
    }
    // The streams are standard ones: plain decoders read them back.
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(fs::File::open(tmp.path().join("packed.img.gz")).unwrap())
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, bytes);
}

#[test]
fn a_compressed_virtual_disk_opens_as_that_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let vhd = tmp.path().join("disk.vhd");
    floppy(&vhd);
    put(&vhd, "A.TXT", b"inside a vhd inside gzip");
    let gz = tmp.path().join("disk.vhd.gz");
    let mut enc = flate2::write::GzEncoder::new(
        fs::File::create(&gz).unwrap(),
        flate2::Compression::default(),
    );
    enc.write_all(&fs::read(&vhd).unwrap()).unwrap();
    enc.finish().unwrap();
    let (info, _) = probe(&gz).unwrap();
    assert_eq!(info.kind, ContainerKind::Vhd);
    assert_eq!(info.compression.as_deref(), Some("gzip"));
    assert_eq!(get(&gz, "A.TXT"), b"inside a vhd inside gzip");
    // New compressed files take the kind of the name inside: disk.vhd.xz is a VHD.
    let xz = tmp.path().join("copy.vhd.xz");
    convert(&gz, None, &xz, None, true, false).unwrap();
    let (info, _) = probe(&xz).unwrap();
    assert_eq!(info.kind, ContainerKind::Vhd);
    assert_eq!(info.variant, "dynamic");
    assert_eq!(get(&xz, "A.TXT"), b"inside a vhd inside gzip");
}

#[test]
fn a_zip_keeps_its_other_members_and_comment() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("raspi.img");
    floppy(&plain);
    let zip_path = tmp.path().join("raspi.zip");
    {
        let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("README.txt", opts).unwrap();
        z.write_all(b"read me").unwrap();
        z.start_file("images/raspi.img", opts).unwrap();
        z.write_all(&fs::read(&plain).unwrap()).unwrap();
        z.set_comment("a comment").unwrap();
        z.finish().unwrap();
    }
    let (info, _) = probe(&zip_path).unwrap();
    assert_eq!(info.member.as_deref(), Some("images/raspi.img"));
    assert_eq!(info.members, vec!["images/raspi.img".to_string()]);
    put(&zip_path, "NEW.TXT", b"added");
    assert_eq!(get(&zip_path, "NEW.TXT"), b"added");
    let mut z = zip::ZipArchive::new(fs::File::open(&zip_path).unwrap()).unwrap();
    assert_eq!(z.comment(), b"a comment");
    let mut readme = String::new();
    z.by_name("README.txt")
        .unwrap()
        .read_to_string(&mut readme)
        .unwrap();
    assert_eq!(readme, "read me");
}

#[test]
fn a_member_can_be_picked_and_a_7z_with_other_files_is_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.img");
    let b = tmp.path().join("b.img");
    floppy(&a);
    floppy(&b);
    put(&b, "B.TXT", b"second");
    let sz = tmp.path().join("two.7z");
    {
        let mut w = sevenz_rust2::ArchiveWriter::create(&sz).unwrap();
        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            w.push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_file(&name),
                Some(fs::File::open(p).unwrap()),
            )
            .unwrap();
        }
        w.finish().unwrap();
    }
    let vol = Volume::open(
        &sz,
        &OpenOptions {
            member: Some("2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(vol.list("/").unwrap().iter().any(|e| e.name == "B.TXT"));
    let host = tmp.path().join("C.TXT");
    fs::write(&host, b"x").unwrap();
    let err = Volume::open(&sz, &rw())
        .unwrap()
        .inject(&host, "/", false)
        .unwrap_err();
    assert!(err.to_string().contains("7z"), "{err}");
}

/// `tests/data/qemu-zstd.qcow2`: a 1.44 MB FAT12 floppy with README.TXT, converted by
/// `qemu-img convert -c -o compression_type=zstd,cluster_size=4096` (QEMU 11.1).
#[test]
fn qcow2_with_zstd_clusters_reads_like_qemu_wrote_it() {
    let img = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/qemu-zstd.qcow2");
    let sums = checksum_virtual_disk(&img, None).unwrap();
    assert_eq!(
        sums.sha256,
        "32ebce09ea122df382023f52b442e02ce4ee657719a9334f6b6ca6c42a01d0b0"
    );
    let text = get(&img, "README.TXT");
    assert!(text.starts_with(b"zstd-compressed qcow2 made by qemu-img 11.1"));
    assert_eq!(text.len(), 2300);
}
