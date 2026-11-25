//! Apple disk images, read-only: UDIF (`.dmg`, with `.dmgpart` segments), sparse images
//! (`.sparseimage`) and sparse bundles (`.sparsebundle` folders).
//!
//! Written from the format descriptions in the Keramics documentation (Apache-2.0):
//! `udif.md`, `sparseimage.md`, `sparsebundle.md` and `adc.md`. No Apple or GPL code was used.
//!
//! UDIF: a 512-byte big-endian `koly` trailer points at the data fork and at an XML property
//! list (newer images) or a resource fork (older ones). Its `blkx` resources are `mish` block
//! tables: runs of 512-byte sectors that are zero, stored, or compressed (ADC, zlib, bzip2,
//! LZFSE, xz), each at an offset in the data fork. A segmented image continues its data fork
//! in `name.002.dmgpart`, `name.003.dmgpart`, …

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::{ContainerInfo, ContainerKind, Disk, file_len, read_up_to};
use crate::error::{Error, Result};

const SECTOR: u64 = 512;
/// Largest run decoded in one piece (hdiutil writes runs of 2048 sectors or fewer).
const MAX_RUN: u64 = 64 << 20;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

fn read_only() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Apple disk images open read-only (convert the image to change it)",
    )
}

/// An encrypted DMG or sparse image (`encrcdsa` header, encryption version 2).
pub(crate) fn looks_encrypted(head: &[u8]) -> bool {
    head.starts_with(b"encrcdsa")
}

pub(crate) fn encrypted_error() -> Error {
    Error::Unsupported("encrypted Apple disk images".into())
}

// ---------------------------------------------------------------------------------------
// UDIF

/// The 512-byte `koly` trailer.
struct Koly {
    data_offset: u64,
    data_size: u64,
    rsrc_offset: u64,
    rsrc_size: u64,
    segment_number: u32,
    segment_count: u32,
    segment_id: [u8; 16],
    plist_offset: u64,
    plist_size: u64,
    sectors: u64,
}

impl Koly {
    fn parse(t: &[u8]) -> Option<Koly> {
        if t.len() < 512 || &t[0..4] != b"koly" || be32(t, 8) != 512 {
            return None;
        }
        Some(Koly {
            data_offset: be64(t, 24),
            data_size: be64(t, 32),
            rsrc_offset: be64(t, 40),
            rsrc_size: be64(t, 48),
            segment_number: be32(t, 56),
            segment_count: be32(t, 60),
            segment_id: t[64..80].try_into().unwrap(),
            plist_offset: be64(t, 216),
            plist_size: be64(t, 224),
            sectors: be64(t, 492),
        })
    }

    fn read(file: &mut File) -> io::Result<Option<Koly>> {
        let len = file_len(file)?;
        let mut t = [0u8; 512];
        if len < 512 || read_up_to(file, len - 512, &mut t)? != 512 {
            return Ok(None);
        }
        Ok(Koly::parse(&t))
    }
}

pub(crate) fn looks_like_udif(tail: &[u8]) -> bool {
    Koly::parse(tail).is_some()
}

/// The data forks of all segments, read as one byte stream.
struct Segments {
    /// (file, data fork offset in it, stream offset where it starts, length).
    parts: Vec<(File, u64, u64, u64)>,
}

impl Segments {
    fn open(path: &Path, koly: &Koly) -> io::Result<Segments> {
        let mut parts = vec![(File::open(path)?, koly.data_offset, 0, koly.data_size)];
        let count = koly.segment_count.max(1);
        if count > 1 && koly.segment_number > 1 {
            return Err(bad(
                "this is a later segment of a split disk image; open the .dmg file",
            ));
        }
        let mut at = koly.data_size;
        for n in 2..=count {
            let p = segment_path(path, n);
            let mut f = File::open(&p)
                .map_err(|e| io::Error::new(e.kind(), format!("segment {}: {e}", p.display())))?;
            let k = Koly::read(&mut f)?;
            let Some(k) = k.filter(|k| k.segment_id == koly.segment_id && k.segment_number == n)
            else {
                return Err(bad(format!(
                    "segment {} belongs to another disk image",
                    p.display()
                )));
            };
            parts.push((f, k.data_offset, at, k.data_size));
            at += k.data_size;
        }
        Ok(Segments { parts })
    }

    fn len(&self) -> u64 {
        self.parts.last().map(|p| p.2 + p.3).unwrap_or(0)
    }

    fn read(&mut self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        if offset
            .checked_add(buf.len() as u64)
            .is_none_or(|e| e > self.len())
        {
            return Err(bad("the disk image file is shorter than its trailer says"));
        }
        while !buf.is_empty() {
            let i = self.parts.partition_point(|p| p.2 + p.3 <= offset);
            let (file, base, start, len) = &mut self.parts[i];
            let n = ((*start + *len - offset) as usize).min(buf.len());
            let got = read_up_to(file, *base + offset - *start, &mut buf[..n])?;
            if got != n {
                return Err(bad("the disk image file is shorter than its trailer says"));
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }
}

/// `disk.dmg` → `disk.002.dmgpart` (numbers below 1000 are padded to 3 digits).
fn segment_path(first: &Path, n: u32) -> PathBuf {
    let stem = first
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    first.with_file_name(format!("{stem}.{n:03}.dmgpart"))
}

const RUN_ZERO: u32 = 0;
const RUN_RAW: u32 = 1;
const RUN_FREE: u32 = 2;
const RUN_ADC: u32 = 0x8000_0004;
const RUN_ZLIB: u32 = 0x8000_0005;
const RUN_BZIP2: u32 = 0x8000_0006;
const RUN_LZFSE: u32 = 0x8000_0007;
const RUN_XZ: u32 = 0x8000_0008;
const RUN_COMMENT: u32 = 0x7fff_fffe;
const RUN_END: u32 = 0xffff_ffff;

#[derive(Debug, Clone, Copy)]
struct Run {
    /// First sector on the disk, and number of sectors.
    sector: u64,
    count: u64,
    kind: u32,
    /// Where the stored bytes are in the data stream.
    offset: u64,
    size: u64,
}

impl Run {
    fn end(&self) -> u64 {
        self.sector + self.count
    }

    fn stored(&self) -> bool {
        !matches!(self.kind, RUN_ZERO | RUN_FREE)
    }
}

/// Runs of one `mish` block table.
fn parse_mish(b: &[u8], runs: &mut Vec<Run>) -> io::Result<()> {
    if b.len() < 204 || &b[0..4] != b"mish" {
        return Err(bad("bad block table in the disk image"));
    }
    let start = be64(b, 8);
    let base = be64(b, 24);
    let n = be32(b, 200) as usize;
    if b.len() < 204 + n * 40 {
        return Err(bad("bad block table in the disk image"));
    }
    for e in b[204..204 + n * 40].as_chunks::<40>().0 {
        let kind = be32(e, 0);
        match kind {
            RUN_END => break,
            RUN_COMMENT => continue,
            _ => {}
        }
        let run = Run {
            sector: start
                .checked_add(be64(e, 8))
                .ok_or_else(|| bad("bad block table in the disk image"))?,
            count: be64(e, 16),
            kind,
            offset: base
                .checked_add(be64(e, 24))
                .ok_or_else(|| bad("bad block table in the disk image"))?,
            size: be64(e, 32),
        };
        if run.count == 0 {
            continue;
        }
        if run.sector.checked_add(run.count).is_none()
            || (run.stored() && run.kind != RUN_RAW && run.count * SECTOR > MAX_RUN)
        {
            return Err(bad("bad block table in the disk image"));
        }
        runs.push(run);
    }
    Ok(())
}

/// The `Data` of every `blkx` resource in the XML property list.
fn plist_tables(xml: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let v = plist::Value::from_reader_xml(xml)
        .map_err(|e| bad(format!("bad property list in the disk image: {e}")))?;
    let blkx = v
        .as_dictionary()
        .and_then(|d| d.get("resource-fork"))
        .and_then(|r| r.as_dictionary())
        .and_then(|r| r.get("blkx"))
        .and_then(|b| b.as_array())
        .ok_or_else(|| bad("bad block table in the disk image"))?;
    Ok(blkx
        .iter()
        .filter_map(|e| {
            e.as_dictionary()?
                .get("Data")?
                .as_data()
                .map(<[u8]>::to_vec)
        })
        .collect())
}

/// The data of every `blkx` resource in a classic Mac resource fork.
fn rsrc_tables(r: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let short = || bad("bad block table in the disk image");
    let u16_at = |o: usize| -> io::Result<usize> {
        r.get(o..o + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            .ok_or_else(short)
    };
    let u32_at = |o: usize| -> io::Result<usize> {
        r.get(o..o + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize)
            .ok_or_else(short)
    };
    let data = u32_at(0)?;
    let map = u32_at(4)?;
    let types = map + u16_at(map + 24)?;
    let ntypes = u16_at(types)?.wrapping_add(1) & 0xffff;
    let mut out = vec![];
    for t in 0..ntypes {
        let e = types + 2 + t * 8;
        if r.get(e..e + 4).ok_or_else(short)? != b"blkx" {
            continue;
        }
        let nrefs = (u16_at(e + 4)? + 1) & 0xffff;
        let refs = types + u16_at(e + 6)?;
        for i in 0..nrefs {
            let d = refs + i * 12;
            let off = u32_at(d + 4)? & 0x00ff_ffff;
            let at = data + off;
            let len = u32_at(at)?;
            out.push(r.get(at + 4..at + 4 + len).ok_or_else(short)?.to_vec());
        }
    }
    Ok(out)
}

/// Apple Data Compression: literal runs (top bit set, 1–128 bytes) and back-references
/// (2 bytes: 3–18 bytes up to 1024 back; 3 bytes: 4–67 bytes up to 65536 back).
fn adc_decode(src: &[u8], out_len: usize) -> io::Result<Vec<u8>> {
    let short = || bad("damaged block in the disk image");
    let mut out = Vec::with_capacity(out_len);
    let mut i = 0;
    while i < src.len() && out.len() < out_len {
        let b = src[i];
        let (n, dist) = if b & 0x80 != 0 {
            let n = (b & 0x7f) as usize + 1;
            out.extend_from_slice(src.get(i + 1..i + 1 + n).ok_or_else(short)?);
            i += 1 + n;
            continue;
        } else if b & 0x40 != 0 {
            let d = src.get(i + 1..i + 3).ok_or_else(short)?;
            i += 3;
            (
                (b & 0x3f) as usize + 4,
                u16::from_be_bytes([d[0], d[1]]) as usize,
            )
        } else {
            let d = *src.get(i + 1).ok_or_else(short)?;
            i += 2;
            (
                ((b >> 2) & 0x0f) as usize + 3,
                ((b & 3) as usize) << 8 | d as usize,
            )
        };
        if dist >= out.len() {
            return Err(short());
        }
        let from = out.len() - 1 - dist;
        for k in 0..n {
            out.push(out[from + k]);
        }
    }
    Ok(out)
}

fn decode_run(kind: u32, src: &[u8], out_len: usize) -> io::Result<Vec<u8>> {
    let damaged = |_: &dyn std::fmt::Display| bad("damaged block in the disk image");
    let mut out = Vec::with_capacity(out_len);
    match kind {
        RUN_ADC => out = adc_decode(src, out_len)?,
        RUN_ZLIB => {
            flate2::read::ZlibDecoder::new(src)
                .take(out_len as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| damaged(&e))?;
        }
        RUN_BZIP2 => {
            bzip2::read::BzDecoder::new(src)
                .take(out_len as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| damaged(&e))?;
        }
        RUN_XZ => {
            lzma_rust2::XzReader::new(src, false)
                .take(out_len as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| damaged(&e))?;
        }
        RUN_LZFSE => {
            lzfse_rust::decode_bytes(src, &mut out).map_err(|e| damaged(&e))?;
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Apple disk image block type {}",
                    format_args!("0x{other:08x}")
                ),
            ));
        }
    }
    // Reading one byte past the block makes the zlib, bzip2 and xz decoders reach the end of
    // their stream and check it.
    if out.len() != out_len && !(kind == RUN_ADC && out.len() > out_len) {
        return Err(bad("damaged block in the disk image"));
    }
    out.truncate(out_len);
    Ok(out)
}

pub(crate) struct DmgDisk {
    data: Segments,
    /// Sorted by first sector, not overlapping.
    runs: Vec<Run>,
    size: u64,
    variant: &'static str,
    /// Decoded runs, most recent last.
    cache: Vec<(usize, Vec<u8>)>,
}

impl DmgDisk {
    /// `tail`: the last 512 bytes of the file (the trailer).
    pub(crate) fn open(path: &Path, tail: &[u8]) -> Result<DmgDisk> {
        let mut file = File::open(path)?;
        let koly = Koly::parse(tail)
            .ok_or_else(|| Error::Invalid("bad block table in the disk image".into()))?;
        let fail = |e: io::Error| match e.kind() {
            io::ErrorKind::Unsupported => Error::Unsupported(e.to_string()),
            _ => Error::Invalid(e.to_string()),
        };
        let data = Segments::open(path, &koly).map_err(fail)?;
        let mut read_blob = |off: u64, len: u64| -> Result<Vec<u8>> {
            if len > 256 << 20 {
                return Err(Error::Invalid("bad block table in the disk image".into()));
            }
            let mut b = vec![0u8; len as usize];
            if read_up_to(&mut file, off, &mut b)? != b.len() {
                return Err(Error::Invalid(
                    "the disk image file is shorter than its trailer says".into(),
                ));
            }
            Ok(b)
        };
        let tables = if koly.plist_size > 0 {
            plist_tables(&read_blob(koly.plist_offset, koly.plist_size)?).map_err(fail)?
        } else if koly.rsrc_size > 0 {
            rsrc_tables(&read_blob(koly.rsrc_offset, koly.rsrc_size)?).map_err(fail)?
        } else {
            vec![]
        };
        let mut runs = vec![];
        for t in &tables {
            parse_mish(t, &mut runs).map_err(fail)?;
        }
        if tables.is_empty() {
            // No block tables: the data fork is the disk.
            runs.push(Run {
                sector: 0,
                count: koly.sectors.max(data.len() / SECTOR),
                kind: RUN_RAW,
                offset: 0,
                size: data.len(),
            });
        }
        runs.sort_by_key(|r| r.sector);
        if runs.windows(2).any(|w| w[0].end() > w[1].sector) {
            return Err(Error::Invalid("bad block table in the disk image".into()));
        }
        let end = runs.last().map_or(0, Run::end);
        let sectors = if koly.sectors > 0 { koly.sectors } else { end };
        let variant = variant_of(&runs);
        Ok(DmgDisk {
            data,
            runs,
            size: sectors * SECTOR,
            variant,
            cache: vec![],
        })
    }

    fn decoded(&mut self, i: usize) -> io::Result<&[u8]> {
        if let Some(p) = self.cache.iter().position(|c| c.0 == i) {
            let c = self.cache.remove(p);
            self.cache.push(c);
        } else {
            let r = self.runs[i];
            if r.size > MAX_RUN {
                return Err(bad("bad block table in the disk image"));
            }
            let mut src = vec![0u8; r.size as usize];
            self.data.read(r.offset, &mut src)?;
            let out = decode_run(r.kind, &src, (r.count * SECTOR) as usize)?;
            if self.cache.len() >= 8 {
                self.cache.remove(0);
            }
            self.cache.push((i, out));
        }
        Ok(&self.cache.last().unwrap().1)
    }
}

/// The hdiutil format name for the compression the runs use.
fn variant_of(runs: &[Run]) -> &'static str {
    let mut counts = [0u64; 5];
    for r in runs {
        let i = match r.kind {
            RUN_ZLIB => 0,
            RUN_BZIP2 => 1,
            RUN_LZFSE => 2,
            RUN_XZ => 3,
            RUN_ADC => 4,
            _ => continue,
        };
        counts[i] += r.count;
    }
    let best = (0..5).max_by_key(|&i| counts[i]).unwrap();
    if counts[best] == 0 {
        return "UDRO";
    }
    ["UDZO", "UDBZ", "ULFO", "ULMO", "UDCO"][best]
}

impl Disk for DmgDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut at = offset;
        let mut done = 0;
        while done < buf.len() {
            let sector = at / SECTOR;
            let i = self.runs.partition_point(|r| r.end() <= sector);
            let left = buf.len() - done;
            let Some(&r) = self.runs.get(i) else {
                buf[done..].fill(0);
                break;
            };
            if r.sector > sector {
                // A gap between runs reads as zeros.
                let n = ((r.sector * SECTOR - at) as usize).min(left);
                buf[done..done + n].fill(0);
                done += n;
                at += n as u64;
                continue;
            }
            let within = at - r.sector * SECTOR;
            let n = ((r.count * SECTOR - within) as usize).min(left);
            let out = &mut buf[done..done + n];
            match r.kind {
                RUN_ZERO | RUN_FREE => out.fill(0),
                RUN_RAW => {
                    // A stored run may be shorter than its sectors (the rest is zeros).
                    let have = r.size.saturating_sub(within).min(n as u64) as usize;
                    self.data.read(r.offset + within, &mut out[..have])?;
                    out[have..].fill(0);
                }
                _ => {
                    let d = self.decoded(i)?;
                    out.copy_from_slice(&d[within as usize..within as usize + n]);
                }
            }
            done += n;
            at += n as u64;
        }
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(read_only())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        ContainerInfo::simple(ContainerKind::Dmg, self.variant, self.size)
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let (first, last) = (offset / SECTOR, (offset + len).div_ceil(SECTOR));
        let i = self.runs.partition_point(|r| r.end() <= first);
        Ok(self.runs[i..]
            .iter()
            .take_while(|r| r.sector < last)
            .any(Run::stored))
    }
}

// ---------------------------------------------------------------------------------------
// Sparse image

pub(crate) fn looks_like_sparseimage(head: &[u8]) -> bool {
    head.len() >= 64 && head.starts_with(b"sprs")
}

pub(crate) struct SparseImageDisk {
    file: File,
    file_len: u64,
    band: u64,
    size: u64,
    /// Media band → file offset of its data (`None`: never written, reads as zeros).
    slots: Vec<Option<u64>>,
}

impl SparseImageDisk {
    pub(crate) fn open(path: &Path) -> Result<SparseImageDisk> {
        let bad_header = || Error::Invalid("bad sparse image header".into());
        let mut file = File::open(path)?;
        let file_len = file_len(&mut file)?;
        let mut h = [0u8; 4096];
        if read_up_to(&mut file, 0, &mut h)? != h.len() || !looks_like_sparseimage(&h) {
            return Err(bad_header());
        }
        let band = be32(&h, 8) as u64 * SECTOR;
        let size = be32(&h, 16) as u64 * SECTOR;
        if band == 0 {
            return Err(bad_header());
        }
        let bands = size.div_ceil(band) as usize;
        let mut slots = vec![None; bands];
        // The first header maps up to 1008 bands (table at 64), stored right after it. When
        // they are all used, hdiutil writes another header after the last band, linked from
        // offset 20 of the first; each further header is 4096 bytes too, maps up to 1010 bands
        // (table at 56) and links the next from offset 12 (0: none). Seen on macOS 26.
        let mut at = 0u64;
        loop {
            let first = at == 0;
            let table = if first { &h[64..] } else { &h[56..] };
            for (slot, c) in table.as_chunks::<4>().0.iter().enumerate() {
                let b = u32::from_be_bytes(*c);
                if b == 0 {
                    continue;
                }
                let m = (b - 1) as usize;
                if m >= bands || slots[m].is_some() {
                    return Err(bad_header());
                }
                slots[m] = Some(at + 4096 + slot as u64 * band);
            }
            let next = be64(&h, if first { 20 } else { 12 });
            if next == 0 {
                break;
            }
            if next <= at || next >= file_len {
                return Err(bad_header());
            }
            at = next;
            if read_up_to(&mut file, at, &mut h)? != h.len() || !looks_like_sparseimage(&h) {
                return Err(bad_header());
            }
        }
        Ok(SparseImageDisk {
            file,
            file_len,
            band,
            size,
            slots,
        })
    }
}

impl Disk for SparseImageDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (b, within) = ((at / self.band) as usize, at % self.band);
            let n = ((self.band - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            out.fill(0);
            if let Some(Some(start)) = self.slots.get(b) {
                let pos = start + within;
                if pos < self.file_len {
                    read_up_to(&mut self.file, pos, out)?;
                }
            }
            done += n;
        }
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(read_only())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        ContainerInfo::simple(ContainerKind::SparseImage, "UDSP", self.size)
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let (first, last) = (offset / self.band, (offset + len).div_ceil(self.band));
        Ok((first..last).any(|b| matches!(self.slots.get(b as usize), Some(Some(_)))))
    }
}

// ---------------------------------------------------------------------------------------
// Sparse bundle

const BUNDLE_TYPE: &str = "com.apple.diskimage.sparsebundle";

fn bundle_plist(dir: &Path) -> Option<plist::Dictionary> {
    let f = File::open(dir.join("Info.plist")).ok()?;
    plist::Value::from_reader(io::BufReader::new(f))
        .ok()?
        .into_dictionary()
}

pub(crate) fn looks_like_sparsebundle(dir: &Path) -> bool {
    bundle_plist(dir).is_some_and(|d| {
        d.get("diskimage-bundle-type")
            .and_then(|t| t.as_string())
            .is_some_and(|t| t == BUNDLE_TYPE)
    })
}

pub(crate) struct SparseBundleDisk {
    bands: PathBuf,
    band: u64,
    size: u64,
    /// The last band file opened (band number, file; `None`: missing).
    open_band: Option<(u64, Option<File>)>,
}

impl SparseBundleDisk {
    pub(crate) fn open(dir: &Path) -> Result<SparseBundleDisk> {
        let d = bundle_plist(dir)
            .ok_or_else(|| Error::Invalid("bad Info.plist in the sparse bundle".into()))?;
        let int = |k: &str| d.get(k).and_then(|v| v.as_unsigned_integer());
        let (Some(band), Some(size)) = (int("band-size"), int("size")) else {
            return Err(Error::Invalid("bad Info.plist in the sparse bundle".into()));
        };
        if band == 0 || !band.is_multiple_of(SECTOR) {
            return Err(Error::Invalid("bad Info.plist in the sparse bundle".into()));
        }
        if std::fs::metadata(dir.join("token")).is_ok_and(|m| m.len() > 0) {
            return Err(encrypted_error());
        }
        Ok(SparseBundleDisk {
            bands: dir.join("bands"),
            band,
            size,
            open_band: None,
        })
    }

    fn band_file(&mut self, b: u64) -> io::Result<Option<&mut File>> {
        if self.open_band.as_ref().is_none_or(|o| o.0 != b) {
            let f = match File::open(self.bands.join(format!("{b:x}"))) {
                Ok(f) => Some(f),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
            self.open_band = Some((b, f));
        }
        Ok(self.open_band.as_mut().and_then(|o| o.1.as_mut()))
    }
}

impl Disk for SparseBundleDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (b, within) = (at / self.band, at % self.band);
            let n = ((self.band - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            out.fill(0);
            if at < self.size
                && let Some(f) = self.band_file(b)?
            {
                // A band file shorter than the band size reads as zeros past its end.
                read_up_to(f, within, out)?;
            }
            done += n;
        }
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(read_only())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        ContainerInfo::simple(ContainerKind::SparseBundle, "UDSB", self.size)
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let (first, last) = (offset / self.band, (offset + len).div_ceil(self.band));
        for b in first..last {
            if self.band_file(b)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adc_literals_and_back_references() {
        // "abc", then copy 3 from distance 2 ("abc"), then a 4-byte extended copy of the
        // last byte (an overlapping run).
        let src = [0x82, b'a', b'b', b'c', 0x00, 0x02, 0x40, 0x00, 0x00];
        assert_eq!(adc_decode(&src, 64).unwrap(), b"abcabccccc");
        assert!(adc_decode(&[0x00, 0x05], 8).is_err());
    }

    #[test]
    fn segment_names() {
        assert_eq!(
            segment_path(Path::new("d/disk.dmg"), 2),
            Path::new("d/disk.002.dmgpart")
        );
        assert_eq!(
            segment_path(Path::new("disk.dmg"), 1234),
            Path::new("disk.1234.dmgpart")
        );
    }
}
