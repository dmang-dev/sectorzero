//! Password-encrypted IMZ images (WinImage's password protection).

use std::fs;
use std::path::Path;

use sectorzero_core::{
    CreateOptions, Error, FloppyFormat, Image, NewImage, OpenOptions, Volume, convert, probe,
    set_password,
};

fn rw() -> OpenOptions {
    OpenOptions {
        writable: true,
        ..Default::default()
    }
}

fn floppy(fmt: &str) -> NewImage {
    NewImage::Floppy(FloppyFormat::find(fmt).unwrap())
}

/// Pseudo-random bytes, so the compressed image spans several 32 KiB chunks.
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (x >> 16) as u8
        })
        .collect()
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
    fs::read(dir.path().join(name.trim_start_matches('/'))).unwrap()
}

fn stored(img: &Path) -> Vec<u8> {
    fs::read(img).unwrap()
}

#[test]
fn encrypted_imz_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    for (fmt, size) in [("360", 20_000usize), ("1440", 600_000)] {
        let img = tmp.path().join(format!("e{fmt}.imz"));
        set_password(&img, Some("Pässwort"));
        Volume::create(&img, floppy(fmt), &CreateOptions::default()).unwrap();
        let data = noise(size, size as u32);
        put(&img, "noise.bin", &data);
        // The payload is encrypted: the file contents do not show up in the archive.
        let raw = stored(&img);
        assert!(!raw.windows(32).any(|w| w == &data[1000..1032]));
        assert_eq!(raw[6] & 1, 1, "general purpose flag bit 0");
        assert!(probe(&img).unwrap().0.encrypted);

        set_password(&img, None);
        match Volume::open(&img, &OpenOptions::default()) {
            Err(Error::PasswordNeeded(n)) => assert!(n.contains(&format!("e{fmt}.imz"))),
            other => panic!("expected PasswordNeeded, got {:?}", other.err()),
        }
        set_password(&img, Some("Passwort"));
        assert!(matches!(
            Volume::open(&img, &OpenOptions::default()),
            Err(Error::WrongPassword)
        ));
        set_password(&img, Some("Pässwort"));
        assert_eq!(get(&img, "/noise.bin"), data);
    }
}

#[test]
fn change_and_remove_the_password() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("p.imz");
    Volume::create(&img, floppy("720"), &CreateOptions::default()).unwrap();
    let data = noise(50_000, 7);
    put(&img, "a.bin", &data);
    assert!(!probe(&img).unwrap().0.encrypted);

    Image::open(&img, &rw())
        .unwrap()
        .set_password(Some("one"))
        .unwrap();
    set_password(&img, None);
    assert!(matches!(
        Volume::open(&img, &OpenOptions::default()),
        Err(Error::PasswordNeeded(_))
    ));
    set_password(&img, Some("one"));
    assert_eq!(get(&img, "/a.bin"), data);

    Image::open(&img, &rw())
        .unwrap()
        .set_password(Some("two"))
        .unwrap();
    set_password(&img, Some("one"));
    assert!(matches!(
        Volume::open(&img, &OpenOptions::default()),
        Err(Error::WrongPassword)
    ));
    set_password(&img, Some("two"));
    // Writing to an encrypted image keeps it encrypted.
    put(&img, "b.txt", b"more");
    set_password(&img, None);
    assert!(Volume::open(&img, &OpenOptions::default()).is_err());
    set_password(&img, Some("two"));
    assert_eq!(get(&img, "/b.txt"), b"more");

    Image::open(&img, &rw())
        .unwrap()
        .set_password(None)
        .unwrap();
    set_password(&img, None);
    assert!(!probe(&img).unwrap().0.encrypted);
    assert_eq!(get(&img, "/a.bin"), data);
    // An unencrypted IMZ is a plain ZIP that any unzip tool reads.
    assert_eq!(stored(&img)[6] & 1, 0);
}

#[test]
fn encryption_survives_conversion_and_rebuilds() {
    let tmp = tempfile::tempdir().unwrap();
    let raw = tmp.path().join("plain.ima");
    Volume::create(&raw, floppy("1440"), &CreateOptions::default()).unwrap();
    let data = noise(200_000, 3);
    put(&raw, "c.bin", &data);

    let enc = tmp.path().join("enc.imz");
    set_password(&enc, Some("conv"));
    convert(&raw, None, &enc, None, false, false).unwrap();
    assert!(probe(&enc).unwrap().0.encrypted);
    assert_eq!(get(&enc, "/c.bin"), data);

    // Change format rebuilds the image in a new container of the same kind.
    Volume::change_format(&enc, &rw(), floppy("2880")).unwrap();
    assert!(probe(&enc).unwrap().0.encrypted);
    set_password(&enc, None);
    assert!(Volume::open(&enc, &OpenOptions::default()).is_err());
    set_password(&enc, Some("conv"));
    assert_eq!(get(&enc, "/c.bin"), data);

    // Back out to a raw image.
    let back = tmp.path().join("back.ima");
    convert(&enc, None, &back, None, false, false).unwrap();
    assert_eq!(get(&back, "/c.bin"), data);
}

/// Files saved by WinImage itself with a password: `SZ_IMZ_SAMPLES="a.imz=pw;b.imz=pw"`.
/// Each must open, and its image must match `<file>.ima` next to it when that exists.
#[test]
#[ignore]
fn winimage_samples() {
    let list = std::env::var("SZ_IMZ_SAMPLES").expect("set SZ_IMZ_SAMPLES");
    for item in list.split(';').filter(|s| !s.is_empty()) {
        let (file, pw) = item.split_once('=').expect("file=password");
        let path = Path::new(file);
        set_password(path, Some(pw));
        let info = probe(path).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert!(info.0.encrypted, "{file}");
        let vol = Volume::open(path, &OpenOptions::default()).unwrap();
        let entries = vol.list("/").unwrap();
        println!(
            "{file}: {} entries, {} bytes",
            entries.len(),
            info.0.virtual_size
        );
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("x.ima");
        convert(path, None, &out, None, false, false).unwrap();
        let reference = path.with_extension("ima");
        if reference.exists() {
            assert_eq!(
                fs::read(&out).unwrap(),
                fs::read(&reference).unwrap(),
                "{file}"
            );
        }
    }
}
