//! Macrium Reflect X images (.mrimgx), built here from the published file layout: a full
//! image, a delta incremental and a full-index differential over it, and an encrypted one.

use std::fs;
use std::io::Write;
use std::path::Path;

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use md5::{Digest, Md5};
use sectorzero_core::{
    ContainerKind, CreateOptions, Error, NewImage, OpenOptions, PartitionScheme, Volume,
    checksum_virtual_disk, probe, set_password,
};
use serde_json::json;
use sha2::Sha256;

const BS: usize = 65536;
const ID: &str = "0AD2FD9362D493B1";

#[derive(Clone, Copy, Default)]
struct Ref {
    pos: i64,
    md5: [u8; 16],
    len: u32,
    file: u16,
}

impl Ref {
    fn bytes(&self) -> Vec<u8> {
        let mut v = self.pos.to_le_bytes().to_vec();
        v.extend_from_slice(&self.md5);
        v.extend_from_slice(&self.len.to_le_bytes());
        v.extend_from_slice(&self.file.to_le_bytes());
        v
    }
}

fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    k[..key.len()].copy_from_slice(key);
    let mut i = Sha256::new();
    i.update(k.map(|b| b ^ 0x36));
    i.update(msg);
    let mut o = Sha256::new();
    o.update(k.map(|b| b ^ 0x5c));
    o.update(i.finalize());
    o.finalize()[..].try_into().unwrap()
}

fn sha256(d: &[u8]) -> [u8; 32] {
    Sha256::digest(d)[..].try_into().unwrap()
}

fn image_id() -> [u8; 8] {
    let mut b = [0u8; 8];
    for (i, x) in b.iter_mut().enumerate() {
        *x = u8::from_str_radix(&ID[i * 2..i * 2 + 2], 16).unwrap();
    }
    b
}

struct Enc {
    key: [u8; 32],
    iterations: u32,
}

impl Enc {
    fn new(password: &str, iterations: u32) -> Enc {
        let salt = sha256(&image_id());
        let mut s = salt.to_vec();
        s.extend_from_slice(&1u32.to_be_bytes());
        let mut u = hmac(password.as_bytes(), &s);
        let mut t = u;
        for _ in 1..iterations {
            u = hmac(password.as_bytes(), &u);
            t.iter_mut().zip(u).for_each(|(a, b)| *a ^= b);
        }
        Enc { key: t, iterations }
    }

    fn encrypt(&self, idx: u32, data: &mut Vec<u8>) {
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&image_id());
        iv[8..10].copy_from_slice(&1u16.to_le_bytes()); // disk_number
        iv[10..12].copy_from_slice(&1u16.to_le_bytes()); // partition_number
        iv[12..].copy_from_slice(&idx.to_le_bytes());
        let ivc = aes::Aes256::new_from_slice(&sha256(&self.key)).unwrap();
        let mut g = GenericArray::from(iv);
        ivc.encrypt_block(&mut g);
        let c = aes::Aes256::new_from_slice(&self.key).unwrap();
        data.resize(data.len().div_ceil(16) * 16, 0);
        let mut prev: [u8; 16] = g[..].try_into().unwrap();
        for b in data.as_chunks_mut::<16>().0 {
            b.iter_mut().zip(prev).for_each(|(x, p)| *x ^= p);
            c.encrypt_block(GenericArray::from_mut_slice(b));
            prev = *b;
        }
    }
}

fn block(out: &mut Vec<u8>, name: &[u8; 8], payload: &[u8], last: bool, compressed: bool) {
    let stored = if compressed {
        zstd::bulk::compress(payload, 3).unwrap()
    } else {
        payload.to_vec()
    };
    out.extend_from_slice(name);
    out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
    out.extend_from_slice(&Md5::digest(&stored));
    out.push(last as u8 | (compressed as u8) << 1);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(&stored);
}

fn part_of(disk: &[u8]) -> (usize, usize) {
    let e = &disk[446..462];
    let lba = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
    let n = u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;
    (lba * 512, n * 512)
}

/// Write one file of a backup set. `prev`: the disk and the full block index of the
/// previous image; only blocks that differ from it are stored. Returns the full index.
fn write(
    path: &Path,
    disk: &[u8],
    file_number: u16,
    prev: Option<(&[u8], &[Ref])>,
    delta: bool,
    enc: Option<&Enc>,
) -> Vec<Ref> {
    let (start, len) = part_of(disk);
    let n = len.div_ceil(BS);
    let mut out = vec![];
    let mut full = vec![];
    let mut changed = vec![];
    for i in 0..n {
        let chunk = &disk[start + i * BS..(start + (i + 1) * BS).min(start + len)];
        if let Some((old, refs)) = prev {
            let o = &old[start + i * BS..(start + (i + 1) * BS).min(start + len)];
            if o == chunk {
                full.push(refs[i]);
                continue;
            }
        } else if chunk.iter().all(|&b| b == 0) {
            full.push(Ref::default());
            continue;
        }
        let mut data = zstd::bulk::compress(chunk, 3).unwrap();
        if let Some(e) = enc {
            e.encrypt(i as u32, &mut data);
        }
        let r = Ref {
            pos: out.len() as i64,
            md5: Md5::digest(chunk)[..].try_into().unwrap(),
            len: data.len() as u32,
            file: file_number,
        };
        out.extend_from_slice(&data);
        full.push(r);
        changed.push((i as u32, r));
    }
    let index_pos = out.len();
    block(&mut out, b"$TRACK0 ", &disk[..start], true, true);
    let mut idx = 0i32.to_le_bytes().to_vec();
    if delta {
        idx.extend_from_slice(&(changed.len() as i32).to_le_bytes());
        for (i, r) in &changed {
            idx.extend_from_slice(&r.bytes());
            idx.extend_from_slice(&i.to_le_bytes());
        }
    } else {
        idx.extend_from_slice(&(full.len() as i32).to_le_bytes());
        for r in &full {
            idx.extend_from_slice(&r.bytes());
        }
    }
    block(&mut out, b"$INDEX  ", &idx, true, false);
    let encryption = match enc {
        Some(e) => json!({
            "aes_type": "aes-256", "enable": true, "key_derivation": "pbkdf2",
            "key_iterations": e.iterations,
            "hmac": hmac(&e.key, &[]).iter().map(|b| format!("{b:02X}")).collect::<String>(),
        }),
        None => json!({"aes_type": "none", "enable": false}),
    };
    let j = json!({
        "_header": {
            "imageid": ID, "file_number": file_number, "increment_number": file_number,
            "merged_files": [], "split_file": false, "delta_index": delta,
            "index_file_position": index_pos,
            "backup_type": if file_number == 0 { "full" } else if delta { "inc" } else { "diff" },
        },
        "_compression": {"compression_level": "medium", "compression_method": "zstd"},
        "_encryption": encryption,
        "disks": [{
            "_header": {"disk_format": "mbr", "disk_number": 1, "imaged_partition_count": 1},
            "_geometry": {"bytes_per_sector": 512, "disk_size": disk.len()},
            "partitions": [{
                "_header": {"block_size": BS, "block_count": n, "partition_number": 1},
                "_geometry": {"start": start, "boot_sector_offset": 0, "length": len},
                "_file_system": {"start": start, "lcn0_offset": start,
                                 "reserved_sectors_byte_length": 0, "type": "FAT16"},
            }],
        }],
    });
    let first = out.len() as u64;
    block(&mut out, b"$JSON   ", j.to_string().as_bytes(), true, false);
    out.extend_from_slice(&first.to_le_bytes());
    out.extend_from_slice(b"MACRIUM_FILE");
    fs::File::create(path).unwrap().write_all(&out).unwrap();
    full
}

/// A 16 MiB MBR disk with a FAT partition and one file.
fn source(dir: &Path) -> std::path::PathBuf {
    let img = dir.join("src.img");
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 16 << 20,
        },
        &CreateOptions {
            label: Some("MACRIUM".into()),
            scheme: PartitionScheme::Mbr,
            ..Default::default()
        },
    )
    .unwrap();
    put(&img, "FIRST.TXT", &vec![b'1'; 200_000]);
    img
}

fn put(img: &Path, name: &str, data: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let host = dir.path().join(name);
    fs::write(&host, data).unwrap();
    let rw = OpenOptions {
        writable: true,
        ..Default::default()
    };
    Volume::open(img, &rw)
        .unwrap()
        .inject(&host, "/", false)
        .unwrap();
}

fn file(img: &Path, name: &str) -> Option<Vec<u8>> {
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let mut out = vec![];
    v.copy_out(name, &mut out).ok().map(|_| out)
}

fn sum(p: &Path) -> String {
    checksum_virtual_disk(p, None).unwrap().sha256
}

#[test]
fn full_incremental_and_differential_images() {
    let dir = tempfile::tempdir().unwrap();
    let img = source(dir.path());
    let disk0 = fs::read(&img).unwrap();
    let f0 = dir.path().join(format!("{ID}-00-00.mrimgx"));
    let refs0 = write(&f0, &disk0, 0, None, false, None);

    let (info, _) = probe(&f0).unwrap();
    assert_eq!(info.kind, ContainerKind::Mrimgx);
    assert_eq!(info.variant, "full");
    assert_eq!(info.compression.as_deref(), Some("zstd"));
    assert_eq!(info.virtual_size, 16 << 20);
    assert_eq!(sum(&f0), sum(&img));
    assert_eq!(file(&f0, "/FIRST.TXT").unwrap(), vec![b'1'; 200_000]);

    // A delta incremental holding only the blocks that changed.
    put(&img, "SECOND.TXT", b"second backup");
    let disk1 = fs::read(&img).unwrap();
    let f1 = dir.path().join(format!("{ID}-01-01.mrimgx"));
    let refs1 = write(&f1, &disk1, 1, Some((&disk0, &refs0)), true, None);
    let (info, _) = probe(&f1).unwrap();
    assert_eq!(info.variant, "incremental (delta index)");
    assert_eq!(info.parent.as_deref(), f0.file_name().unwrap().to_str());
    assert_eq!(sum(&f1), sum(&img));
    assert_eq!(file(&f1, "/SECOND.TXT").unwrap(), b"second backup");
    assert_eq!(sum(&f0), {
        let p = dir.path().join("disk0.img");
        fs::write(&p, &disk0).unwrap();
        sum(&p)
    });

    // A differential with a full index that points into both earlier files.
    put(&img, "THIRD.TXT", b"third");
    let disk2 = fs::read(&img).unwrap();
    let f2 = dir.path().join(format!("{ID}-02-02.mrimgx"));
    write(&f2, &disk2, 2, Some((&disk1, &refs1)), false, None);
    assert_eq!(sum(&f2), sum(&img));
    assert_eq!(probe(&f2).unwrap().0.variant, "differential");

    // Read-only.
    let host = dir.path().join("X.TXT");
    fs::write(&host, b"x").unwrap();
    let rw = OpenOptions {
        writable: true,
        ..Default::default()
    };
    let err = Volume::open(&f2, &rw)
        .and_then(|v| v.inject(&host, "/", false))
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");

    // Without the full image, the incremental cannot be read.
    fs::remove_file(&f0).unwrap();
    let err = probe(&f1).map(|_| ()).unwrap_err();
    assert!(err.to_string().contains("full image"), "{err}");
}

#[test]
fn encrypted_image_needs_its_password() {
    let dir = tempfile::tempdir().unwrap();
    let img = source(dir.path());
    let disk = fs::read(&img).unwrap();
    let f = dir.path().join(format!("{ID}-00-00.mrimgx"));
    let enc = Enc::new("correct horse", 1000);
    write(&f, &disk, 0, None, false, Some(&enc));

    assert!(matches!(probe(&f), Err(Error::PasswordNeeded(_))));
    set_password(&f, Some("wrong"));
    assert!(matches!(probe(&f), Err(Error::WrongPassword)));
    set_password(&f, Some("correct horse"));
    let (info, _) = probe(&f).unwrap();
    assert!(info.encrypted);
    assert_eq!(sum(&f), sum(&img));
    assert_eq!(file(&f, "/FIRST.TXT").unwrap(), vec![b'1'; 200_000]);
    set_password(&f, None);
}

#[test]
fn damaged_block_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let img = source(dir.path());
    let disk = fs::read(&img).unwrap();
    let f = dir.path().join(format!("{ID}-00-00.mrimgx"));
    let refs = write(&f, &disk, 0, None, false, None);
    let mut bytes = fs::read(&f).unwrap();
    let r = refs.iter().find(|r| r.len > 64).unwrap();
    bytes[r.pos as usize + 40] ^= 0xFF;
    fs::write(&f, &bytes).unwrap();
    assert!(checksum_virtual_disk(&f, None).is_err());
}
