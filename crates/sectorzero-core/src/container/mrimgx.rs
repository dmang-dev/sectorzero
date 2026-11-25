//! Macrium Reflect X disk images (`.mrimgx`), read-only.
//!
//! Written from Macrium's published file layout and its MIT-licensed reference reader
//! (github.com/macrium/mrimgx_file_layout, Copyright (c) 2024 Paramount Software UK
//! Limited; see THIRD_PARTY_NOTICES). Images from Reflect v8 and older (`.mrimg`) use a
//! different, unpublished format.
//!
//! ```text
//! data blocks …  | metadata blocks …                          | footer (20 bytes)
//!                  $JSON (and $AUXDATA), then per disk:          u64 first metadata header
//!                  $TRACK0 [$EPT], per partition: … $INDEX       "MACRIUM_FILE"
//! ```
//!
//! Every metadata block has a 32-byte header: an 8-byte space-padded name, a u32 length,
//! the MD5 of the stored bytes, and flags (bit 0: last block of its group, bit 1: zstd
//! compressed, bit 2: AES-ECB encrypted). The `$JSON` block describes the backup: disks,
//! their geometry, and per partition its offsets and block size. `$TRACK0` is the start
//! of the disk up to the first partition (MBR or GPT), `$EPT` the extended partition
//! boot records of an MBR disk. `$INDEX` lists, for each fixed-size block of the file
//! system, where the stored block is (file position, stored length, MD5 of the decoded
//! data, and which file of the backup set holds it); a length of 0 means the block was not
//! imaged (free space). FAT volumes also list their reserved sectors in 4 MiB pieces.
//!
//! Incremental and differential images hold only changed blocks. Their index either lists
//! every block with the file it is in, or (a "delta index") only the changed ones, which
//! are laid over the index of the full image. The other files of the set are found in the
//! same folder by their image ID.
//!
//! Data blocks are zstd frames when the backup is compressed, and encrypted with AES-CBC
//! when it has a password: the key comes from PBKDF2-HMAC-SHA256 over the password with the
//! SHA-256 of the image ID as salt, and each block's IV is AES-256-ECB (key: SHA-256 of
//! that key) of the image ID, disk number, partition number and block number.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use md5::{Digest, Md5};
use serde_json::Value;
use sha2::Sha256;

use super::{ContainerInfo, ContainerKind, Disk, file_len, open_password_for};
use crate::error::{Error, Result};

const MAGIC: &[u8; 12] = b"MACRIUM_FILE";
const FOOTER: u64 = 20;
const HEADER: usize = 32;
/// Packed `DataBlockIndexElement`: i64 position, MD5, u32 length, u16 file number.
const ELEMENT: usize = 30;
/// Packed `DeltaDataBlock`: an element and the u32 block number it replaces.
const DELTA: usize = 34;
/// FAT reserved sectors are stored in pieces of this size.
const RESERVED_PIECE: u64 = 4 << 20;
/// Decoded blocks kept in memory.
const CACHE: usize = 8;

/// The footer's magic is in the last 12 bytes of the file.
pub(crate) fn looks_like_mrimgx(tail: &[u8]) -> bool {
    tail.len() >= FOOTER as usize && tail.ends_with(MAGIC)
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

fn io_bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn read_exact_at(f: &mut File, pos: u64, buf: &mut [u8]) -> io::Result<()> {
    f.seek(SeekFrom::Start(pos))?;
    f.read_exact(buf)
}

// ---------------------------------------------------------------------------------------
// Encryption

enum Cipher {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

impl Cipher {
    fn new(aes_type: &str, key: &[u8; 32]) -> Result<Cipher> {
        Ok(match aes_type {
            "aes-128" => Cipher::A128(aes::Aes128::new_from_slice(&key[..16]).unwrap()),
            "aes-192" => Cipher::A192(aes::Aes192::new_from_slice(&key[..24]).unwrap()),
            "aes-256" => Cipher::A256(aes::Aes256::new_from_slice(key).unwrap()),
            other => return Err(Error::Unsupported(format!("Macrium encryption {other}"))),
        })
    }

    fn decrypt_block(&self, b: &mut [u8]) {
        let b = GenericArray::from_mut_slice(b);
        match self {
            Cipher::A128(c) => c.decrypt_block(b),
            Cipher::A192(c) => c.decrypt_block(b),
            Cipher::A256(c) => c.decrypt_block(b),
        }
    }

    fn ecb_decrypt(&self, data: &mut [u8]) -> io::Result<()> {
        if !data.len().is_multiple_of(16) {
            return Err(io_bad(
                "encrypted Macrium block is not a whole number of AES blocks",
            ));
        }
        for b in data.as_chunks_mut::<16>().0 {
            self.decrypt_block(b);
        }
        Ok(())
    }

    fn cbc_decrypt(&self, iv: [u8; 16], data: &mut [u8]) -> io::Result<()> {
        if !data.len().is_multiple_of(16) {
            return Err(io_bad(
                "encrypted Macrium block is not a whole number of AES blocks",
            ));
        }
        let mut prev = iv;
        for b in data.as_chunks_mut::<16>().0 {
            let cipher = *b;
            self.decrypt_block(b);
            b.iter_mut().zip(prev).for_each(|(x, p)| *x ^= p);
            prev = cipher;
        }
        Ok(())
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(data));
    out
}

/// HMAC-SHA256 with the inner and outer pad states prepared once.
struct Hmac {
    inner: Sha256,
    outer: Sha256,
}

impl Hmac {
    fn new(key: &[u8]) -> Hmac {
        let mut k = [0u8; 64];
        if key.len() > 64 {
            k[..32].copy_from_slice(&sha256(key));
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let mut inner = Sha256::new();
        let mut outer = Sha256::new();
        inner.update(k.map(|b| b ^ 0x36));
        outer.update(k.map(|b| b ^ 0x5c));
        Hmac { inner, outer }
    }

    fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut h = self.inner.clone();
        for p in parts {
            h.update(p);
        }
        let inner = h.finalize();
        let mut o = self.outer.clone();
        o.update(inner);
        let mut out = [0u8; 32];
        out.copy_from_slice(&o.finalize());
        out
    }
}

/// PBKDF2-HMAC-SHA256, one 32-byte block.
fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let prf = Hmac::new(password);
    let mut u = prf.mac(&[salt, &1u32.to_be_bytes()]);
    let mut t = u;
    for _ in 1..iterations.max(1) {
        u = prf.mac(&[&u]);
        t.iter_mut().zip(u).for_each(|(a, b)| *a ^= b);
    }
    t
}

struct Crypto {
    cipher: Cipher,
    /// AES-256 key (SHA-256 of the derived key) that makes the data block IVs.
    iv_cipher: aes::Aes256,
    image_id: [u8; 8],
}

impl Crypto {
    fn iv(&self, disk: u16, partition: u16, block: u32) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&self.image_id);
        b[8..10].copy_from_slice(&disk.to_le_bytes());
        b[10..12].copy_from_slice(&partition.to_le_bytes());
        b[12..].copy_from_slice(&block.to_le_bytes());
        let mut g = GenericArray::from(b);
        self.iv_cipher.encrypt_block(&mut g);
        let mut iv = [0u8; 16];
        iv.copy_from_slice(&g);
        iv
    }
}

// ---------------------------------------------------------------------------------------
// Parsing

struct BlockHeader {
    name: [u8; 8],
    len: u32,
    md5: [u8; 16],
    last: bool,
    compressed: bool,
    encrypted: bool,
}

fn read_header(f: &mut File, pos: u64) -> io::Result<BlockHeader> {
    let mut h = [0u8; HEADER];
    read_exact_at(f, pos, &mut h)?;
    Ok(BlockHeader {
        name: h[..8].try_into().unwrap(),
        len: u32_at(&h, 8),
        md5: h[12..28].try_into().unwrap(),
        last: h[28] & 1 != 0,
        compressed: h[28] & 2 != 0,
        encrypted: h[28] & 4 != 0,
    })
}

/// A metadata block's payload: MD5 over the stored bytes, then decrypt, then decompress.
fn read_payload(
    f: &mut File,
    pos: u64,
    h: &BlockHeader,
    crypto: Option<&Crypto>,
) -> io::Result<Vec<u8>> {
    let mut data = vec![0u8; h.len as usize];
    read_exact_at(f, pos + HEADER as u64, &mut data)?;
    if Md5::digest(&data)[..] != h.md5 {
        return Err(io_bad(format!(
            "damaged Macrium image: checksum of the {} block does not match",
            String::from_utf8_lossy(&h.name).trim_end()
        )));
    }
    if h.encrypted {
        let c = crypto.ok_or_else(|| io_bad("encrypted Macrium metadata without a key"))?;
        c.cipher.ecb_decrypt(&mut data)?;
    }
    if h.compressed {
        data = unzstd(&data)?;
    }
    Ok(data)
}

/// One zstd frame; whatever follows it (AES padding) is ignored.
fn unzstd(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = vec![];
    zstd::stream::read::Decoder::new(data)
        .and_then(|d| d.single_frame().read_to_end(&mut out))
        .map_err(|e| io_bad(format!("damaged Macrium image: {e}")))?;
    Ok(out)
}

#[derive(Clone, Copy, Default)]
struct BlockRef {
    pos: i64,
    md5: [u8; 16],
    len: u32,
    file: u16,
}

fn element(b: &[u8]) -> BlockRef {
    BlockRef {
        pos: u64_at(b, 0) as i64,
        md5: b[8..24].try_into().unwrap(),
        len: u32_at(b, 24),
        file: u16_at(b, 28),
    }
}

/// `$INDEX`: reserved-sector pieces, then the block index (or a delta index).
struct PartIndex {
    reserved: Vec<BlockRef>,
    blocks: Vec<BlockRef>,
    delta: Vec<(u32, BlockRef)>,
}

fn parse_index(p: &[u8], delta: bool) -> io::Result<PartIndex> {
    let short = || io_bad("damaged Macrium image: block index is cut short");
    let mut o = 0usize;
    let count = |o: &mut usize| -> io::Result<usize> {
        let n = p.get(*o..*o + 4).ok_or_else(short)?;
        *o += 4;
        Ok(i32::from_le_bytes(n.try_into().unwrap()).max(0) as usize)
    };
    let n = count(&mut o)?;
    let reserved = (0..n)
        .map(|i| {
            p.get(o + i * ELEMENT..o + (i + 1) * ELEMENT)
                .map(element)
                .ok_or_else(short)
        })
        .collect::<io::Result<Vec<_>>>()?;
    o += n * ELEMENT;
    let n = count(&mut o)?;
    let mut idx = PartIndex {
        reserved,
        blocks: vec![],
        delta: vec![],
    };
    let size = if delta { DELTA } else { ELEMENT };
    let body = p.get(o..o + n * size).ok_or_else(short)?;
    for e in body.chunks_exact(size) {
        if delta {
            idx.delta.push((u32_at(e, ELEMENT), element(e)));
        } else {
            idx.blocks.push(element(e));
        }
    }
    Ok(idx)
}

fn get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| v.get(k))
}

fn num(v: &Value, path: &[&str]) -> u64 {
    get(v, path)
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
        .unwrap_or(0)
}

fn text<'a>(v: &'a Value, path: &[&str]) -> &'a str {
    get(v, path).and_then(Value::as_str).unwrap_or("")
}

fn flag(v: &Value, path: &[&str]) -> bool {
    get(v, path).and_then(Value::as_bool).unwrap_or(false)
}

/// One file of a backup set: its JSON and where its metadata is.
struct ImageFile {
    path: PathBuf,
    json: Value,
}

impl ImageFile {
    fn open(path: &Path) -> Result<ImageFile> {
        let mut f = File::open(path)?;
        let len = file_len(&mut f)?;
        if len < FOOTER + HEADER as u64 {
            return Err(bad("not a Macrium Reflect X image (too short)"));
        }
        let mut footer = [0u8; FOOTER as usize];
        read_exact_at(&mut f, len - FOOTER, &mut footer)?;
        if !looks_like_mrimgx(&footer) {
            return Err(bad("not a Macrium Reflect X image"));
        }
        let mut pos = u64_at(&footer, 0);
        let mut json = None;
        for _ in 0..64 {
            if pos + HEADER as u64 > len {
                break;
            }
            let h = read_header(&mut f, pos)?;
            if &h.name == b"$JSON   " {
                if h.encrypted {
                    return Err(Error::Unsupported(
                        "Macrium images with encrypted descriptions".into(),
                    ));
                }
                let mut data = read_payload(&mut f, pos, &h, None)?;
                while data.last() == Some(&0) {
                    data.pop();
                }
                json = Some(
                    serde_json::from_slice::<Value>(&data)
                        .map_err(|e| bad(format!("damaged Macrium image description: {e}")))?,
                );
            }
            if h.last {
                break;
            }
            pos += HEADER as u64 + h.len as u64;
        }
        let json =
            json.ok_or_else(|| bad(format!("damaged Macrium image: no {} block", "$JSON")))?;
        Ok(ImageFile {
            path: path.to_path_buf(),
            json,
        })
    }

    fn header(&self, key: &str) -> &Value {
        get(&self.json, &["_header", key]).unwrap_or(&Value::Null)
    }

    fn image_id(&self) -> &str {
        self.header("imageid").as_str().unwrap_or("")
    }

    fn file_number(&self) -> u64 {
        num(&self.json, &["_header", "file_number"])
    }

    fn split(&self) -> bool {
        flag(&self.json, &["_header", "split_file"])
    }

    fn delta(&self) -> bool {
        flag(&self.json, &["_header", "delta_index"])
    }

    fn disks(&self) -> &[Value] {
        self.json
            .get("disks")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The metadata of every disk: ($TRACK0, $EPT payload, per partition its $INDEX).
    #[allow(clippy::type_complexity)]
    fn read_index(
        &self,
        crypto: Option<&Crypto>,
    ) -> io::Result<Vec<(Vec<u8>, Option<Vec<u8>>, Vec<PartIndex>)>> {
        let mut f = File::open(&self.path)?;
        let len = file_len(&mut f)?;
        let mut pos = num(&self.json, &["_header", "index_file_position"]);
        let delta = self.delta();
        let next = |f: &mut File, pos: &mut u64| -> io::Result<(BlockHeader, u64)> {
            if *pos + HEADER as u64 > len {
                return Err(io_bad("damaged Macrium image: metadata is cut short"));
            }
            let h = read_header(f, *pos)?;
            let at = *pos;
            *pos += HEADER as u64 + h.len as u64;
            Ok((h, at))
        };
        let mut out = vec![];
        for disk in self.disks() {
            let mut track0 = None;
            let mut ept = None;
            loop {
                let (h, at) = next(&mut f, &mut pos)?;
                match &h.name {
                    b"$TRACK0 " => track0 = Some(read_payload(&mut f, at, &h, crypto)?),
                    b"$EPT    " => ept = Some(read_payload(&mut f, at, &h, crypto)?),
                    _ => {}
                }
                if h.last {
                    break;
                }
            }
            let track0 = track0
                .ok_or_else(|| io_bad(format!("damaged Macrium image: no {} block", "$TRACK0")))?;
            let parts = disk
                .get("partitions")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let mut indexes = vec![];
            for _ in 0..parts {
                let mut index = None;
                loop {
                    let (h, at) = next(&mut f, &mut pos)?;
                    if &h.name == b"$INDEX  " {
                        index = Some(parse_index(&read_payload(&mut f, at, &h, crypto)?, delta)?);
                    }
                    if h.last {
                        break;
                    }
                }
                indexes.push(index.ok_or_else(|| {
                    io_bad(format!("damaged Macrium image: no {} block", "$INDEX"))
                })?);
            }
            out.push((track0, ept, indexes));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------
// The disk

struct Partition {
    disk_number: u16,
    number: u16,
    block_size: u64,
    reserved_start: u64,
    reserved_len: u64,
    reserved: Vec<BlockRef>,
    data_start: u64,
    blocks: Vec<BlockRef>,
}

/// Something that puts bytes on the disk, in restore order (later ones win).
enum Layer {
    Bytes { start: u64, data: Vec<u8> },
    Reserved(usize),
    Data(usize),
}

enum Hit {
    Bytes(usize, usize),
    Block {
        part: usize,
        reserved: bool,
        idx: usize,
        rel: usize,
    },
}

enum Cover {
    Yes(Hit, u64),
    /// Not covered here; the layer next covers something at this offset.
    No(u64),
}

pub(crate) struct MrimgxDisk {
    size: u64,
    sector: u32,
    parts: Vec<Partition>,
    layers: Vec<Layer>,
    files: HashMap<u16, PathBuf>,
    handles: HashMap<u16, File>,
    crypto: Option<Crypto>,
    compressed: bool,
    cache: Vec<((usize, bool, usize), Vec<u8>)>,
    variant: String,
    members: Vec<String>,
    member: Option<String>,
    parent: Option<String>,
}

/// Partition start LBAs in the partition table of `$TRACK0`.
fn table_starts(track0: &[u8], sector: u64) -> Vec<u64> {
    let mut v = vec![];
    if track0.len() < 512 || u16_at(track0, 510) != 0xAA55 {
        return v;
    }
    let mut gpt = false;
    for i in 0..4 {
        let e = &track0[446 + i * 16..462 + i * 16];
        match e[4] {
            0 => {}
            0xEE => gpt = true,
            _ => v.push(u32_at(e, 8) as u64),
        }
    }
    let h = sector as usize;
    if gpt && track0.len() >= h + 92 && &track0[h..h + 8] == b"EFI PART" {
        let lba = u64_at(track0, h + 72);
        let n = u32_at(track0, h + 80) as usize;
        let size = u32_at(track0, h + 84) as usize;
        for i in 0..n.min(1024) {
            let o = (lba * sector) as usize + i * size;
            if size < 40 || o + 40 > track0.len() {
                break;
            }
            let first = u64_at(track0, o + 32);
            if first != 0 {
                v.push(first);
            }
        }
    }
    v
}

impl MrimgxDisk {
    pub(crate) fn open(path: &Path, member: Option<&str>) -> Result<Self> {
        let target = ImageFile::open(path)?;
        if target.split() {
            return Err(bad(
                "this is a middle part of a split Macrium image (open the last file of the split)",
            ));
        }
        let disks = target.disks();
        if disks.is_empty() {
            return Err(bad(
                "this Macrium image holds no disk images (file and folder backups are not disk images)",
            ));
        }
        let labels: Vec<String> = disks
            .iter()
            .map(|d| format!("Disk {}", num(d, &["_header", "disk_number"])))
            .collect();
        let pick = match member {
            None => 0,
            Some(m) => match m.parse::<usize>() {
                Ok(n) if n >= 1 && n <= disks.len() => n - 1,
                _ => labels
                    .iter()
                    .position(|l| l.eq_ignore_ascii_case(m))
                    .ok_or_else(|| Error::NotFound(format!("archive member {m}")))?,
            },
        };

        // Encryption: the password unlocks every file of the set.
        let aes_type = text(&target.json, &["_encryption", "aes_type"]).to_string();
        let crypto = if aes_type.is_empty() || aes_type == "none" {
            None
        } else {
            let id = target.image_id();
            if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(bad("damaged Macrium image: bad image ID"));
            }
            let mut image_id = [0u8; 8];
            for (i, b) in image_id.iter_mut().enumerate() {
                *b = u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).unwrap();
            }
            let password = open_password_for(path).ok_or_else(|| {
                Error::PasswordNeeded(
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })?;
            let iterations = num(&target.json, &["_encryption", "key_iterations"]) as u32;
            let key = pbkdf2(password.as_bytes(), &sha256(&image_id), iterations);
            let want = text(&target.json, &["_encryption", "hmac"]).to_ascii_lowercase();
            let got: String = Hmac::new(&key)
                .mac(&[])
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if want != got {
                return Err(Error::WrongPassword);
            }
            Some(Crypto {
                cipher: Cipher::new(&aes_type, &key)?,
                iv_cipher: aes::Aes256::new_from_slice(&sha256(&key)).unwrap(),
                image_id,
            })
        };

        // The backup set: files in the same folder with the same image ID, up to this one.
        let increment = num(&target.json, &["_header", "increment_number"]);
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
        let mut set: Vec<ImageFile> = std::fs::read_dir(dir.unwrap_or(Path::new(".")))
            .map(|r| {
                r.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .is_some_and(|e| e.eq_ignore_ascii_case("mrimgx"))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !same_file(p, path))
            .filter_map(|p| ImageFile::open(&p).ok())
            .filter(|f| {
                f.image_id() == target.image_id()
                    && num(&f.json, &["_header", "increment_number"]) <= increment
            })
            .collect();
        let target_number = target.file_number();
        set.retain(|f| f.file_number() != target_number);
        set.push(target);
        set.sort_by_key(|f| std::cmp::Reverse(f.file_number()));
        let target_at = set
            .iter()
            .position(|f| f.file_number() == target_number)
            .unwrap();
        let mut files = HashMap::new();
        for f in &set {
            files.insert(f.file_number() as u16, f.path.clone());
            if let Some(m) = f.header("merged_files").as_array() {
                for n in m.iter().filter_map(Value::as_u64) {
                    files.insert(n as u16, f.path.clone());
                }
            }
        }

        let target = &set[target_at];
        let (track0, ept, mut indexes) = target
            .read_index(crypto.as_ref())?
            .into_iter()
            .nth(pick)
            .ok_or_else(|| bad("damaged Macrium image: disk metadata missing"))?;
        let mut base_file = None;
        if target.delta() {
            // The newest full index at or before this file, then every delta after it.
            let full = (target_at..set.len())
                .find(|&i| !set[i].delta() && !set[i].split())
                .ok_or_else(|| {
                    bad("the full image this Macrium incremental image builds on was not found next to it")
                })?;
            let full_index = set[full].read_index(crypto.as_ref())?;
            let full_disk = full_index
                .into_iter()
                .nth(pick)
                .ok_or_else(|| bad("damaged Macrium image: disk metadata missing"))?;
            let mut blocks: Vec<Vec<BlockRef>> =
                full_disk.2.into_iter().map(|p| p.blocks).collect();
            for i in (target_at..full).rev() {
                if set[i].split() {
                    continue;
                }
                let delta = if i == target_at {
                    std::mem::take(&mut indexes)
                        .into_iter()
                        .map(|p| (p.reserved, p.delta))
                        .collect::<Vec<_>>()
                } else {
                    set[i]
                        .read_index(crypto.as_ref())?
                        .into_iter()
                        .nth(pick)
                        .map(|d| d.2.into_iter().map(|p| (p.reserved, p.delta)).collect())
                        .unwrap_or_default()
                };
                for (p, (reserved, d)) in delta.into_iter().enumerate() {
                    if i == target_at {
                        indexes.push(PartIndex {
                            reserved,
                            blocks: vec![],
                            delta: vec![],
                        });
                    }
                    let Some(b) = blocks.get_mut(p) else { continue };
                    for (n, r) in d {
                        let n = n as usize;
                        if n >= b.len() {
                            b.resize(n + 1, BlockRef::default());
                        }
                        b[n] = r;
                    }
                }
            }
            for (p, b) in blocks.into_iter().enumerate() {
                if let Some(idx) = indexes.get_mut(p) {
                    idx.blocks = b;
                }
            }
            base_file = Some(set[full].path.clone());
        } else if let Some(first) = set.last()
            && set.len() > 1
        {
            base_file = Some(first.path.clone());
        }

        let disk = &set[target_at].disks()[pick];
        let sector = match num(disk, &["_geometry", "bytes_per_sector"]) {
            n @ (512 | 1024 | 2048 | 4096) => n,
            _ => 512,
        };
        let starts = table_starts(&track0, sector);
        let mut layers = vec![Layer::Bytes {
            start: 0,
            data: track0,
        }];
        if let Some(ept) = ept
            && ept.len() >= 4
        {
            let n = u32_at(&ept, 0) as usize;
            let stride = if n > 0 && 4 + n * 522 == ept.len() {
                522
            } else {
                528
            };
            for e in ept[4..].chunks_exact(stride) {
                layers.push(Layer::Bytes {
                    start: u64_at(e, 512),
                    data: e[..512].to_vec(),
                });
            }
        }
        let mut parts = vec![];
        let mut end = 0u64;
        let part_json = disk
            .get("partitions")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for (pj, idx) in part_json.iter().zip(indexes) {
            let start = num(pj, &["_geometry", "start"]);
            // The reference restore treats these as byte offsets; take sectors only when
            // the partition table says so.
            let scale = if start.is_multiple_of(sector) && starts.contains(&(start / sector)) {
                1
            } else if starts.contains(&start) {
                sector
            } else {
                1
            };
            let start = start * scale;
            let boot = num(pj, &["_geometry", "boot_sector_offset"]) * scale;
            let lcn0 = num(pj, &["_file_system", "lcn0_offset"]) * scale;
            let fs_start = num(pj, &["_file_system", "start"]) * scale;
            let block_size = num(pj, &["_header", "block_size"]).max(512);
            let data_start = start.saturating_add(lcn0.saturating_sub(fs_start));
            let reserved_len = num(pj, &["_file_system", "reserved_sectors_byte_length"]);
            let part = Partition {
                disk_number: num(disk, &["_header", "disk_number"]) as u16,
                number: num(pj, &["_header", "partition_number"]) as u16,
                block_size,
                reserved_start: start + boot,
                reserved_len,
                reserved: idx.reserved,
                data_start,
                blocks: idx.blocks,
            };
            end = end
                .max(part.data_start + part.blocks.len() as u64 * block_size)
                .max(part.reserved_start + part.reserved_len);
            let i = parts.len();
            parts.push(part);
            layers.push(Layer::Reserved(i));
            layers.push(Layer::Data(i));
        }
        for l in &layers {
            if let Layer::Bytes { start, data } = l {
                end = end.max(start + data.len() as u64);
            }
        }
        let size = num(disk, &["_geometry", "disk_size"]).max(end);
        let target = &set[target_at];
        let level = text(&target.json, &["_compression", "compression_level"]);
        let variant = match (
            text(&target.json, &["_header", "backup_type"]),
            target.delta(),
        ) {
            ("inc", false) => "incremental",
            ("inc", true) => "incremental (delta index)",
            ("diff", false) => "differential",
            ("diff", true) => "differential (delta index)",
            _ => "full",
        }
        .to_string();
        Ok(MrimgxDisk {
            size,
            sector: sector as u32,
            parts,
            layers,
            files,
            handles: HashMap::new(),
            crypto,
            compressed: !level.is_empty() && level != "none",
            cache: vec![],
            variant,
            member: (labels.len() > 1).then(|| labels[pick].clone()),
            members: if labels.len() > 1 { labels } else { vec![] },
            parent: base_file
                .filter(|p| !same_file(p, path))
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
        })
    }

    fn cover(&self, layer: &Layer, pos: u64) -> Cover {
        let (start, len, piece, reserved, part) = match *layer {
            Layer::Bytes { start, ref data } => {
                let end = start + data.len() as u64;
                return if pos < start {
                    Cover::No(start)
                } else if pos < end {
                    Cover::Yes(Hit::Bytes(0, (pos - start) as usize), end)
                } else {
                    Cover::No(u64::MAX)
                };
            }
            Layer::Reserved(i) => {
                let p = &self.parts[i];
                (p.reserved_start, p.reserved_len, RESERVED_PIECE, true, i)
            }
            Layer::Data(i) => {
                let p = &self.parts[i];
                let len = p.blocks.len() as u64 * p.block_size;
                (p.data_start, len, p.block_size, false, i)
            }
        };
        let end = start + len;
        if pos < start {
            return Cover::No(if len > 0 { start } else { u64::MAX });
        }
        if pos >= end {
            return Cover::No(u64::MAX);
        }
        let rel = pos - start;
        let idx = (rel / piece) as usize;
        let next = (start + (idx as u64 + 1) * piece).min(end);
        let p = &self.parts[part];
        let list = if reserved { &p.reserved } else { &p.blocks };
        match list.get(idx) {
            Some(b) if b.len != 0 => Cover::Yes(
                Hit::Block {
                    part,
                    reserved,
                    idx,
                    rel: (rel % piece) as usize,
                },
                next,
            ),
            _ => Cover::No(if next < end { next } else { u64::MAX }),
        }
    }

    /// What is at `pos`, and up to where.
    fn find(&self, pos: u64) -> (Option<Hit>, u64) {
        let mut limit = u64::MAX;
        for (n, layer) in self.layers.iter().enumerate().rev() {
            match self.cover(layer, pos) {
                Cover::Yes(hit, end) => {
                    let hit = match hit {
                        Hit::Bytes(_, rel) => Hit::Bytes(n, rel),
                        h => h,
                    };
                    return (Some(hit), end.min(limit));
                }
                Cover::No(next) => limit = limit.min(next),
            }
        }
        (None, limit)
    }

    fn block(&mut self, part: usize, reserved: bool, idx: usize) -> io::Result<&[u8]> {
        let key = (part, reserved, idx);
        if let Some(i) = self.cache.iter().position(|(k, _)| *k == key) {
            let e = self.cache.remove(i);
            self.cache.push(e);
            return Ok(&self.cache.last().unwrap().1);
        }
        let p = &self.parts[part];
        let r = if reserved {
            p.reserved[idx]
        } else {
            p.blocks[idx]
        };
        let (disk, number) = (p.disk_number, p.number);
        if !self.handles.contains_key(&r.file) {
            let path = self.files.get(&r.file).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "file number {} of this Macrium backup set was not found next to it",
                        r.file
                    ),
                )
            })?;
            self.handles.insert(r.file, File::open(path)?);
        }
        let f = self.handles.get_mut(&r.file).unwrap();
        let mut data = vec![0u8; r.len as usize];
        read_exact_at(f, r.pos.max(0) as u64, &mut data)?;
        if let Some(c) = &self.crypto {
            c.cipher
                .cbc_decrypt(c.iv(disk, number, idx as u32), &mut data)?;
        }
        if self.compressed && data.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            data = unzstd(&data)?;
            if Md5::digest(&data)[..] != r.md5 {
                return Err(io_bad(
                    "damaged Macrium image: a data block's checksum does not match",
                ));
            }
        }
        if self.cache.len() >= CACHE {
            self.cache.remove(0);
        }
        self.cache.push((key, data));
        Ok(&self.cache.last().unwrap().1)
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

impl Disk for MrimgxDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset + buf.len() as u64;
        let mut pos = offset;
        while pos < end {
            let (hit, stop) = self.find(pos);
            let stop = stop.min(end).max(pos + 1);
            let dst = &mut buf[(pos - offset) as usize..(stop - offset) as usize];
            match hit {
                None => dst.fill(0),
                Some(Hit::Bytes(n, rel)) => {
                    if let Layer::Bytes { data, .. } = &self.layers[n] {
                        dst.copy_from_slice(&data[rel..rel + dst.len()]);
                    }
                }
                Some(Hit::Block {
                    part,
                    reserved,
                    idx,
                    rel,
                }) => {
                    let b = self.block(part, reserved, idx)?;
                    let have = b.len().saturating_sub(rel).min(dst.len());
                    dst[..have].copy_from_slice(&b[rel..rel + have]);
                    dst[have..].fill(0);
                }
            }
            pos = stop;
        }
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Macrium Reflect images open read-only (convert the image to change it)",
        ))
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        let mut info = ContainerInfo::simple(ContainerKind::Mrimgx, &self.variant, self.size);
        info.members = self.members.clone();
        info.member = self.member.clone();
        info.parent = self.parent.clone();
        info.encrypted = self.crypto.is_some();
        info.compression = self.compressed.then(|| "zstd".to_string());
        info
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let end = offset.saturating_add(len).min(self.size);
        let mut pos = offset;
        while pos < end {
            let (hit, stop) = self.find(pos);
            if hit.is_some() {
                return Ok(true);
            }
            pos = stop.max(pos + 1);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_matches_rfc_7914() {
        // RFC 7914 section 11: PBKDF2-HMAC-SHA256 ("passwd", "salt", 1 iteration).
        let k = pbkdf2(b"passwd", b"salt", 1);
        assert_eq!(
            k[..16],
            [
                0x55, 0xac, 0x04, 0x6e, 0x56, 0xe3, 0x08, 0x9f, 0xec, 0x16, 0x91, 0xc2, 0x25, 0x44,
                0xb6, 0x05
            ]
        );
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // RFC 4231 test case 2.
        let m = Hmac::new(b"Jefe").mac(&[b"what do ya want ", b"for nothing?"]);
        assert_eq!(m[..8], [0x5b, 0xdc, 0xc1, 0x46, 0xbf, 0x60, 0x75, 0x4e]);
    }
}
