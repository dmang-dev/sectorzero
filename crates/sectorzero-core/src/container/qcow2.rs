//! QEMU copy-on-write images (QCOW2, versions 2 and 3), from the published qcow2 format
//! specification.
//!
//! Layout: a big-endian header in cluster 0, a two-level map from guest clusters to host
//! clusters (an L1 table of L2 table offsets; L2 entries pointing at data clusters, marking
//! zero clusters, or describing deflate-compressed clusters), and a refcount table of
//! refcount blocks that counts the references to every host cluster.
//!
//! Supported: reading plain, zero, and deflate- or zstd-compressed clusters, backing files (any image
//! format this crate opens), writing (new clusters are appended; refcounts are kept exact),
//! and creating new version 3 images. Not supported: encryption, external data files,
//! extended L2 entries, and writing images that have internal snapshots.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use super::{ContainerInfo, ContainerKind, Disk, read_up_to};
use crate::error::{Error, Result};
use crate::util::*;

const MAGIC: &[u8] = b"QFI\xfb";
const OFFSET_MASK: u64 = 0x00FF_FFFF_FFFF_FE00;
const COPIED: u64 = 1 << 63;
const COMPRESSED: u64 = 1 << 62;
const ZERO: u64 = 1;
const DEFAULT_CLUSTER_BITS: u32 = 16;
/// Header extension: the backing file's format name.
const EXT_BACKING_FORMAT: u32 = 0xE279_2ACA;
const INCOMPAT_DIRTY: u64 = 1;
const INCOMPAT_CORRUPT: u64 = 2;
const INCOMPAT_EXTERNAL_DATA: u64 = 4;
const INCOMPAT_COMPRESSION: u64 = 8;
const INCOMPAT_EXTENDED_L2: u64 = 16;
const L2_CACHE: usize = 256;

thread_local! {
    /// Depth of nested backing-file opens (stops loops).
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn looks_like_qcow(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

pub(crate) struct Qcow2Disk {
    file: File,
    version: u32,
    cluster_bits: u32,
    size: u64,
    l1_offset: u64,
    l1: Vec<u64>,
    l2_cache: HashMap<u64, Vec<u64>>,
    refcount_table_offset: u64,
    refcount_table: Vec<u64>,
    backing: Option<Box<dyn Disk>>,
    backing_name: Option<String>,
    /// End of the file, cluster aligned: where new clusters go.
    next_free: u64,
    /// Last decompressed cluster (host descriptor, data).
    inflated: Option<(u64, Vec<u8>)>,
    /// Compressed clusters are zstd frames (compression type 1), not raw deflate.
    zstd: bool,
}

fn unsupported(what: &str) -> Error {
    Error::Unsupported(format!("QCOW2 images with {what}"))
}

fn open_backing(path: &Path, format: Option<&str>) -> Result<Box<dyn Disk>> {
    let depth = DEPTH.get();
    if depth >= 16 {
        return Err(Error::Invalid(
            "QCOW2 backing chain is too long (or loops)".into(),
        ));
    }
    DEPTH.set(depth + 1);
    let r = match format {
        Some("raw") => super::raw::RawDisk::open(path, false)
            .map(|d| Box::new(d) as Box<dyn Disk>)
            .map_err(Error::from),
        _ => super::open(path, false, None),
    };
    DEPTH.set(depth);
    r
}

impl Qcow2Disk {
    pub(crate) fn open(path: &Path, writable: bool) -> Result<Self> {
        let mut file = File::options().read(true).write(writable).open(path)?;
        let len = file.metadata()?.len();
        let mut h = vec![0u8; 4096];
        let got = read_up_to(&mut file, 0, &mut h)?;
        h.truncate(got);
        if h.len() < 72 || !looks_like_qcow(&h) {
            return Err(Error::Invalid("QCOW2 header not found".into()));
        }
        let version = be32(&h, 4);
        if version == 1 {
            return Err(Error::Unsupported("QCOW version 1 images".into()));
        }
        if !matches!(version, 2 | 3) {
            return Err(Error::Unsupported(format!("QCOW2 version {version}")));
        }
        let cluster_bits = be32(&h, 20);
        if !(9..=21).contains(&cluster_bits) {
            return Err(Error::Invalid("QCOW2 cluster size is invalid".into()));
        }
        let cs = 1u64 << cluster_bits;
        let size = be64(&h, 24);
        if be32(&h, 32) != 0 {
            return Err(unsupported("encryption"));
        }
        let l1_size = be32(&h, 36) as usize;
        let l1_offset = be64(&h, 40);
        let refcount_table_offset = be64(&h, 48);
        let refcount_clusters = u64::from(be32(&h, 56));
        let snapshots = be32(&h, 60);
        let (incompat, refcount_order, header_length) = if version >= 3 {
            if h.len() < 104 {
                return Err(Error::Invalid("QCOW2 header is truncated".into()));
            }
            (be64(&h, 72), be32(&h, 96), be32(&h, 100) as usize)
        } else {
            (0, 4, 72)
        };
        if incompat & INCOMPAT_EXTERNAL_DATA != 0 {
            return Err(unsupported("an external data file"));
        }
        if incompat & INCOMPAT_EXTENDED_L2 != 0 {
            return Err(unsupported("extended L2 entries (subclusters)"));
        }
        // Compression type (byte 104): 0 deflate, 1 zstd.
        let zstd = match (
            incompat & INCOMPAT_COMPRESSION != 0,
            h.get(104).copied().unwrap_or(0),
        ) {
            (false, _) | (true, 0) => false,
            (true, 1) => true,
            (true, n) => return Err(unsupported(&format!("compression type {n}"))),
        };
        let known = INCOMPAT_DIRTY
            | INCOMPAT_CORRUPT
            | INCOMPAT_EXTERNAL_DATA
            | INCOMPAT_COMPRESSION
            | INCOMPAT_EXTENDED_L2;
        if incompat & !known != 0 {
            return Err(unsupported(&format!(
                "unknown incompatible features {:#x}",
                incompat & !known
            )));
        }
        if writable {
            if incompat & (INCOMPAT_DIRTY | INCOMPAT_CORRUPT) != 0 {
                return Err(Error::Unsupported(
                    "writing a QCOW2 image marked dirty or corrupt (repair it with \
                     `qemu-img check -r all` first)"
                        .into(),
                ));
            }
            if snapshots != 0 {
                return Err(Error::Unsupported(
                    "writing QCOW2 images with internal snapshots".into(),
                ));
            }
            if refcount_order != 4 {
                return Err(Error::Unsupported(format!(
                    "writing QCOW2 images with {}-bit refcounts (16-bit only)",
                    1u32 << refcount_order.min(31)
                )));
            }
        }
        let per_l2 = cs / 8;
        if (l1_size as u64) * per_l2 * cs < size {
            return Err(Error::Invalid(
                "QCOW2 L1 table does not cover the disk".into(),
            ));
        }
        if l1_size > 32 * 1024 * 1024 || refcount_clusters * cs > 256 * 1024 * 1024 {
            return Err(Error::Invalid("QCOW2 tables are implausibly large".into()));
        }
        let l1 = read_be64s(&mut file, l1_offset, l1_size)?;
        let refcount_table = read_be64s(
            &mut file,
            refcount_table_offset,
            (refcount_clusters * cs / 8) as usize,
        )?;

        // Header extensions: only the backing format matters here.
        let mut backing_format = None;
        let mut at = if version >= 3 { header_length } else { 72 };
        while at + 8 <= h.len() {
            let (kind, n) = (be32(&h, at), be32(&h, at + 4) as usize);
            if kind == 0 {
                break;
            }
            if kind == EXT_BACKING_FORMAT
                && let Some(b) = h.get(at + 8..at + 8 + n)
            {
                backing_format = Some(String::from_utf8_lossy(b).into_owned());
            }
            at += 8 + round_up(n as u64, 8) as usize;
        }
        let (backing_offset, backing_len) = (be64(&h, 8), be32(&h, 16) as usize);
        let (mut backing, mut backing_name) = (None, None);
        if backing_offset != 0 && backing_len != 0 {
            let mut name = vec![0u8; backing_len.min(4096)];
            read_up_to(&mut file, backing_offset, &mut name)?;
            let name = String::from_utf8_lossy(&name).into_owned();
            if name.contains("://") || name.starts_with("json:") {
                return Err(unsupported(&format!(
                    "a network or JSON backing file ({name})"
                )));
            }
            let dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let bpath = dir.join(&name);
            if !bpath.is_file() {
                return Err(Error::Invalid(format!(
                    "QCOW2 backing file {} not found",
                    bpath.display()
                )));
            }
            backing = Some(
                open_backing(&bpath, backing_format.as_deref()).map_err(|e| {
                    Error::Invalid(format!("QCOW2 backing file {}: {e}", bpath.display()))
                })?,
            );
            backing_name = Some(bpath.display().to_string());
        }
        Ok(Qcow2Disk {
            file,
            version,
            cluster_bits,
            size,
            l1_offset,
            l1,
            l2_cache: HashMap::new(),
            refcount_table_offset,
            refcount_table,
            backing,
            backing_name,
            next_free: round_up(len, cs),
            inflated: None,
            zstd,
        })
    }

    /// A new, empty version 3 image with 64 KiB clusters and 16-bit refcounts.
    pub(crate) fn create(path: &Path, size: u64) -> Result<Self> {
        let cluster_bits = DEFAULT_CLUSTER_BITS;
        let cs = 1u64 << cluster_bits;
        let l1_size = size.div_ceil(cs * (cs / 8)).max(1);
        if l1_size > u64::from(u32::MAX) {
            return Err(Error::Invalid("disk too large for QCOW2".into()));
        }
        let l1_clusters = (l1_size * 8).div_ceil(cs);
        // Enough refcount blocks for the largest the file can grow to, so the refcount
        // table never has to move.
        let most = 4 + l1_clusters + l1_size + size.div_ceil(cs);
        let per_block = cs / 2;
        let blocks = most.div_ceil(per_block) + 1;
        let rt_clusters = (blocks * 8).div_ceil(cs);
        let rt_offset = cs;
        let block_offset = rt_offset + rt_clusters * cs;
        let l1_offset = block_offset + cs;
        let meta = l1_offset / cs + l1_clusters;
        if meta > per_block {
            return Err(Error::Invalid("disk too large for QCOW2".into()));
        }

        let mut h = vec![0u8; cs as usize];
        h[..4].copy_from_slice(MAGIC);
        put_be32(&mut h, 4, 3);
        put_be32(&mut h, 20, cluster_bits);
        put_be64(&mut h, 24, size);
        put_be32(&mut h, 36, l1_size as u32);
        put_be64(&mut h, 40, l1_offset);
        put_be64(&mut h, 48, rt_offset);
        put_be32(&mut h, 56, rt_clusters as u32);
        put_be32(&mut h, 96, 4);
        put_be32(&mut h, 100, 104);
        let mut rt = vec![0u8; (rt_clusters * cs) as usize];
        put_be64(&mut rt, 0, block_offset);
        let mut block = vec![0u8; cs as usize];
        for c in 0..meta as usize {
            put_be16(&mut block, c * 2, 1);
        }
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(&h)?;
        file.write_all(&rt)?;
        file.write_all(&block)?;
        file.set_len(meta * cs)?;
        let mut refcount_table = vec![0u64; (rt_clusters * cs / 8) as usize];
        refcount_table[0] = block_offset;
        Ok(Qcow2Disk {
            file,
            version: 3,
            cluster_bits,
            size,
            l1_offset,
            l1: vec![0; l1_size as usize],
            l2_cache: HashMap::new(),
            refcount_table_offset: rt_offset,
            refcount_table,
            backing: None,
            backing_name: None,
            next_free: meta * cs,
            inflated: None,
            zstd: false,
        })
    }

    fn cluster_size(&self) -> u64 {
        1 << self.cluster_bits
    }

    fn l2_table(&mut self, offset: u64) -> io::Result<&mut Vec<u64>> {
        if !self.l2_cache.contains_key(&offset) {
            if self.l2_cache.len() >= L2_CACHE {
                self.l2_cache.clear();
            }
            let entries = (self.cluster_size() / 8) as usize;
            let t = read_be64s(&mut self.file, offset, entries)?;
            self.l2_cache.insert(offset, t);
        }
        Ok(self.l2_cache.get_mut(&offset).unwrap())
    }

    /// (L2 table offset or 0, index in it) for guest cluster `vc`.
    fn l2_slot(&self, vc: u64) -> (u64, usize) {
        let per = self.cluster_size() / 8;
        let l1e = self.l1.get((vc / per) as usize).copied().unwrap_or(0);
        (l1e & OFFSET_MASK, (vc % per) as usize)
    }

    fn l2_entry(&mut self, vc: u64) -> io::Result<u64> {
        let (table, i) = self.l2_slot(vc);
        if table == 0 {
            return Ok(0);
        }
        Ok(self.l2_table(table)?[i])
    }

    /// Host byte offset and stored length of a compressed cluster descriptor.
    fn compressed_extent(&self, entry: u64) -> (u64, u64) {
        let x = 62 - (self.cluster_bits - 8);
        let host = entry & ((1u64 << x) - 1);
        let extra = (entry >> x) & ((1u64 << (self.cluster_bits - 8)) - 1);
        (host, (extra + 1) * 512 - (host & 511))
    }

    fn inflate(&mut self, entry: u64) -> io::Result<&[u8]> {
        if self.inflated.as_ref().map(|c| c.0) != Some(entry) {
            let (host, len) = self.compressed_extent(entry);
            let mut input = vec![0u8; len as usize];
            let got = read_up_to(&mut self.file, host, &mut input)?;
            input.truncate(got);
            let mut out = vec![0u8; self.cluster_size() as usize];
            let full = if self.zstd {
                zstd_cluster(&input, &mut out)
            } else {
                let mut d = flate2::Decompress::new(false);
                let r = d.decompress(&input, &mut out, flate2::FlushDecompress::Finish);
                r.is_ok() && d.total_out() == self.cluster_size()
            };
            if !full {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "corrupt compressed QCOW2 cluster",
                ));
            }
            self.inflated = Some((entry, out));
        }
        Ok(&self.inflated.as_ref().unwrap().1)
    }

    /// Fill `buf` from guest cluster `vc` starting `within` bytes into it.
    fn read_cluster(&mut self, vc: u64, within: u64, buf: &mut [u8]) -> io::Result<()> {
        let entry = self.l2_entry(vc)?;
        let cs = self.cluster_size();
        if entry & COMPRESSED != 0 {
            let data = self.inflate(entry)?;
            buf.copy_from_slice(&data[within as usize..within as usize + buf.len()]);
            return Ok(());
        }
        let host = entry & OFFSET_MASK;
        if self.version >= 3 && entry & ZERO != 0 {
            buf.fill(0);
        } else if host == 0 {
            match self.backing.as_mut() {
                Some(b) => b.read_at(vc * cs + within, buf)?,
                None => buf.fill(0),
            }
        } else {
            let got = read_up_to(&mut self.file, host + within, buf)?;
            buf[got..].fill(0);
        }
        Ok(())
    }

    fn write_be64(&mut self, at: u64, v: u64) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(at))?;
        self.file.write_all(&v.to_be_bytes())
    }

    /// Take the next cluster at the end of the file (zero-filled, refcount 1).
    fn alloc_cluster(&mut self) -> io::Result<u64> {
        let off = self.next_free;
        self.next_free += self.cluster_size();
        self.file.set_len(self.next_free)?;
        self.set_refcount(off, 1)?;
        Ok(off)
    }

    /// Where the 16-bit refcount of host cluster `host` lives, adding a refcount block when
    /// its range has none yet.
    fn refcount_slot(&mut self, host: u64) -> io::Result<u64> {
        let cs = self.cluster_size();
        let ci = host >> self.cluster_bits;
        let per_block = cs / 2;
        let ti = (ci / per_block) as usize;
        if ti >= self.refcount_table.len() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "QCOW2 refcount table is full (growing it is not supported)",
            ));
        }
        let mut block = self.refcount_table[ti] & OFFSET_MASK;
        if block == 0 {
            block = self.next_free;
            self.next_free += cs;
            self.file.set_len(self.next_free)?;
            self.refcount_table[ti] = block;
            self.write_be64(self.refcount_table_offset + ti as u64 * 8, block)?;
            self.set_refcount(block, 1)?;
        }
        Ok(block + (ci % per_block) * 2)
    }

    fn refcount(&mut self, host: u64) -> io::Result<u16> {
        let at = self.refcount_slot(host)?;
        let mut b = [0u8; 2];
        read_up_to(&mut self.file, at, &mut b)?;
        Ok(u16::from_be_bytes(b))
    }

    fn set_refcount(&mut self, host: u64, value: u16) -> io::Result<()> {
        let at = self.refcount_slot(host)?;
        self.file.seek(SeekFrom::Start(at))?;
        self.file.write_all(&value.to_be_bytes())
    }

    /// The L2 table for guest cluster `vc`, allocating an empty one if needed.
    fn ensure_l2(&mut self, vc: u64) -> io::Result<(u64, usize)> {
        let (table, i) = self.l2_slot(vc);
        if table != 0 {
            return Ok((table, i));
        }
        let off = self.alloc_cluster()?;
        let per = self.cluster_size() / 8;
        let l1i = (vc / per) as usize;
        self.l1[l1i] = off | COPIED;
        self.write_be64(self.l1_offset + l1i as u64 * 8, off | COPIED)?;
        self.l2_cache
            .insert(off, vec![0; (self.cluster_size() / 8) as usize]);
        Ok((off, i))
    }

    fn write_cluster(&mut self, vc: u64, within: u64, data: &[u8]) -> io::Result<()> {
        let cs = self.cluster_size();
        let entry = self.l2_entry(vc)?;
        let host = entry & OFFSET_MASK;
        let compressed = entry & COMPRESSED != 0;
        let zero = !compressed && self.version >= 3 && entry & ZERO != 0;
        if !compressed && !zero && host != 0 {
            self.file.seek(SeekFrom::Start(host + within))?;
            return self.file.write_all(data);
        }
        let reads_zero = zero || (!compressed && self.backing.is_none());
        if reads_zero && is_zero(data) {
            return Ok(());
        }
        // Materialise the whole cluster: what reads there now, with the new bytes over it.
        let mut whole = vec![0u8; cs as usize];
        if data.len() as u64 != cs {
            self.read_cluster(vc, 0, &mut whole)?;
        }
        whole[within as usize..within as usize + data.len()].copy_from_slice(data);
        let (table, i) = self.ensure_l2(vc)?;
        let target = if zero && host != 0 && !compressed {
            host // preallocated zero cluster: reuse it
        } else {
            self.alloc_cluster()?
        };
        self.file.seek(SeekFrom::Start(target))?;
        self.file.write_all(&whole)?;
        let new_entry = target | COPIED;
        self.l2_table(table)?[i] = new_entry;
        self.write_be64(table + i as u64 * 8, new_entry)?;
        if compressed {
            // Drop this cluster's reference to every host cluster its data touched.
            let (at, len) = self.compressed_extent(entry);
            let first = at >> self.cluster_bits;
            let last = (at + len - 1) >> self.cluster_bits;
            for c in first..=last {
                let n = self.refcount(c << self.cluster_bits)?;
                self.set_refcount(c << self.cluster_bits, n.saturating_sub(1))?;
            }
            self.inflated = None;
        }
        Ok(())
    }
}

fn read_be64s(file: &mut File, offset: u64, count: usize) -> io::Result<Vec<u64>> {
    let mut raw = vec![0u8; count * 8];
    read_up_to(file, offset, &mut raw)?;
    Ok(raw.chunks(8).map(|c| be64(c, 0)).collect())
}

impl Disk for Qcow2Disk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        let cs = self.cluster_size();
        while !buf.is_empty() {
            if offset >= self.size {
                buf.fill(0);
                return Ok(());
            }
            let within = offset % cs;
            let n = (buf.len() as u64).min(cs - within).min(self.size - offset) as usize;
            let (chunk, rest) = buf.split_at_mut(n);
            self.read_cluster(offset / cs, within, chunk)?;
            offset += n as u64;
            buf = rest;
        }
        Ok(())
    }

    fn write_at(&mut self, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the QCOW2 disk",
            ));
        }
        let cs = self.cluster_size();
        while !buf.is_empty() {
            let within = offset % cs;
            let n = (buf.len() as u64).min(cs - within) as usize;
            self.write_cluster(offset / cs, within, &buf[..n])?;
            offset += n as u64;
            buf = &buf[n..];
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn info(&self) -> ContainerInfo {
        let mut info = ContainerInfo::simple(
            ContainerKind::Qcow2,
            if self.version >= 3 { "v3" } else { "v2" },
            self.size,
        );
        info.parent = self.backing_name.clone();
        info
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let cs = self.cluster_size();
        let first = offset / cs;
        let last = (offset + len.max(1) - 1) / cs;
        for vc in first..=last {
            let entry = self.l2_entry(vc)?;
            if entry & COMPRESSED != 0 {
                return Ok(true);
            }
            if self.version >= 3 && entry & ZERO != 0 {
                continue;
            }
            if entry & OFFSET_MASK != 0 {
                return Ok(true);
            }
            if let Some(b) = self.backing.as_mut()
                && b.is_allocated(vc * cs, cs)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Decompress one zstd-compressed cluster. The stored extent may run past the end of the
/// frame (it is rounded up to sectors), so decode as a stream and stop once `out` is full.
fn zstd_cluster(input: &[u8], out: &mut [u8]) -> bool {
    use zstd::stream::raw::{Decoder, InBuffer, Operation, OutBuffer};
    let Ok(mut dec) = Decoder::new() else {
        return false;
    };
    let mut inb = InBuffer::around(input);
    let mut outb = OutBuffer::around(out);
    loop {
        match dec.run(&mut inb, &mut outb) {
            Err(_) => return false,
            Ok(_) if outb.pos() == outb.capacity() => return true,
            Ok(0) => return false,
            Ok(_) if inb.pos() == input.len() => return false,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recount every reference from the header, tables and maps and compare with the stored
    /// refcounts (what `qemu-img check` verifies).
    fn check_refcounts(path: &Path) {
        let mut d = Qcow2Disk::open(path, false).unwrap();
        let cs = d.cluster_size();
        let len = d.file.metadata().unwrap().len();
        let mut refs = vec![0u16; len.div_ceil(cs) as usize];
        let mut add = |off: u64, n: u64| {
            for c in (off / cs)..(off + n).div_ceil(cs) {
                refs[c as usize] += 1;
            }
        };
        add(0, cs);
        add(d.refcount_table_offset, (d.refcount_table.len() * 8) as u64);
        add(d.l1_offset, (d.l1.len() * 8) as u64);
        for &b in &d.refcount_table.clone() {
            if b != 0 {
                add(b & OFFSET_MASK, cs);
            }
        }
        for l1e in d.l1.clone() {
            let t = l1e & OFFSET_MASK;
            if t == 0 {
                continue;
            }
            add(t, cs);
            for e in d.l2_table(t).unwrap().clone() {
                if e & COMPRESSED != 0 {
                    let (h, n) = d.compressed_extent(e);
                    add(h, n);
                } else if e & OFFSET_MASK != 0 {
                    add(e & OFFSET_MASK, cs);
                }
            }
        }
        for (c, &want) in refs.iter().enumerate() {
            let got = d.refcount(c as u64 * cs).unwrap();
            assert_eq!(got, want, "refcount of host cluster {c}");
        }
    }

    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 ^ seed as u8)
            .collect()
    }

    #[test]
    fn create_write_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.qcow2");
        let size = 3 * 1024 * 1024 * 1024; // 3 GiB: several L2 tables
        let data = pattern(200_000, 1);
        {
            let mut d = Qcow2Disk::create(&path, size).unwrap();
            d.write_at(65536 - 1000, &data).unwrap();
            d.write_at(size - 512, &[0xAB; 512]).unwrap();
            d.write_at(1 << 30, &[0u8; 65536]).unwrap(); // zeros stay unallocated
            d.sync().unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() < 2 << 20);
        check_refcounts(&path);
        let mut d = Qcow2Disk::open(&path, true).unwrap();
        assert_eq!(d.size(), size);
        assert_eq!(d.info().variant, "v3");
        let mut got = vec![0u8; data.len()];
        d.read_at(65536 - 1000, &mut got).unwrap();
        assert_eq!(got, data);
        let mut s = [0u8; 512];
        d.read_at(size - 512, &mut s).unwrap();
        assert_eq!(s, [0xAB; 512]);
        assert!(!d.is_allocated(1 << 30, 65536).unwrap());
        assert!(d.is_allocated(0, 65536).unwrap());
        // Overwrite in place, reopen, verify.
        d.write_at(70_000, &[1, 2, 3]).unwrap();
        drop(d);
        let mut d = Qcow2Disk::open(&path, false).unwrap();
        let mut b = [0u8; 3];
        d.read_at(70_000, &mut b).unwrap();
        assert_eq!(b, [1, 2, 3]);
        check_refcounts(&path);
    }

    #[test]
    fn compressed_clusters_read_and_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.qcow2");
        let cs = 65536u64;
        let a = pattern(cs as usize, 7)
            .iter()
            .map(|b| b & 0x0F)
            .collect::<Vec<u8>>();
        let b = vec![0x42u8; cs as usize];
        {
            let mut d = Qcow2Disk::create(&path, 16 * cs).unwrap();
            // Store guest clusters 2 and 3 compressed, back to back in one host cluster.
            let (table, _) = d.ensure_l2(2).unwrap();
            let host = d.alloc_cluster().unwrap() + 100;
            let mut at = host;
            for (vc, data) in [(2u64, &a), (3, &b)] {
                let mut c = flate2::Compress::new(flate2::Compression::default(), false);
                let mut out = vec![0u8; 2 * cs as usize];
                c.compress(data, &mut out, flate2::FlushCompress::Finish)
                    .unwrap();
                out.truncate(c.total_out() as usize);
                d.file.seek(SeekFrom::Start(at)).unwrap();
                d.file.write_all(&out).unwrap();
                let sectors = (at + out.len() as u64).div_ceil(512) - at / 512;
                let x = 62 - (16 - 8);
                let entry = COMPRESSED | ((sectors - 1) << x) | at;
                d.write_be64(table + vc * 8, entry).unwrap();
                at += out.len() as u64;
            }
            assert!(at < host - 100 + cs, "both fit in the one host cluster");
            // That host cluster is referenced by two compressed clusters.
            d.set_refcount(host - 100, 2).unwrap();
        }
        check_refcounts(&path);
        let mut d = Qcow2Disk::open(&path, true).unwrap();
        let mut got = vec![0u8; 2 * cs as usize];
        d.read_at(2 * cs, &mut got).unwrap();
        assert_eq!(&got[..cs as usize], &a[..]);
        assert_eq!(&got[cs as usize..], &b[..]);
        assert!(d.is_allocated(2 * cs, 1).unwrap());
        // Rewrite part of a compressed cluster: it becomes a plain one.
        d.write_at(2 * cs + 10, &[9u8; 20]).unwrap();
        drop(d);
        check_refcounts(&path);
        let mut d = Qcow2Disk::open(&path, false).unwrap();
        d.read_at(2 * cs, &mut got).unwrap();
        let mut want = a.clone();
        want[10..30].fill(9);
        assert_eq!(&got[..cs as usize], &want[..]);
        assert_eq!(&got[cs as usize..], &b[..]);
    }

    #[test]
    fn backing_file_supplies_unwritten_clusters() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base.img");
        let base_data = pattern(4 * 65536, 3);
        std::fs::write(&base, &base_data).unwrap();
        let path = tmp.path().join("over.qcow2");
        drop(Qcow2Disk::create(&path, base_data.len() as u64).unwrap());
        {
            // Point the header at the backing file (name stored after the header).
            let mut f = File::options().write(true).open(&path).unwrap();
            let name = b"base.img";
            let mut ext = Vec::new();
            ext.extend_from_slice(&EXT_BACKING_FORMAT.to_be_bytes());
            ext.extend_from_slice(&3u32.to_be_bytes());
            ext.extend_from_slice(b"raw\0\0\0\0\0");
            f.seek(SeekFrom::Start(104)).unwrap();
            f.write_all(&ext).unwrap();
            f.seek(SeekFrom::Start(8)).unwrap();
            f.write_all(&512u64.to_be_bytes()).unwrap();
            f.write_all(&(name.len() as u32).to_be_bytes()).unwrap();
            f.seek(SeekFrom::Start(512)).unwrap();
            f.write_all(name).unwrap();
        }
        let mut d = Qcow2Disk::open(&path, true).unwrap();
        assert!(d.info().parent.unwrap().ends_with("base.img"));
        let mut got = vec![0u8; base_data.len()];
        d.read_at(0, &mut got).unwrap();
        assert_eq!(got, base_data);
        assert!(d.is_allocated(65536, 10).unwrap(), "backed data counts");
        d.write_at(65536 + 5, &[0u8; 10]).unwrap(); // zeros must override the base
        drop(d);
        let mut d = Qcow2Disk::open(&path, false).unwrap();
        d.read_at(0, &mut got).unwrap();
        let mut want = base_data.clone();
        want[65536 + 5..65536 + 15].fill(0);
        assert_eq!(got, want);
        assert_eq!(std::fs::read(&base).unwrap(), base_data, "base untouched");
        check_refcounts(&path);
    }
}
