//! Microsoft VHDX (Virtual Hard Disk v2), fixed and dynamic, per the published
//! [MS-VHDX] specification.
//!
//! Layout: a 64 KiB file identifier, two 4 KiB headers (the valid one with the higher
//! sequence number is current), two copies of the region table, the log, the metadata
//! region and the block allocation table (BAT). Payload blocks are found through the BAT;
//! every `chunk_ratio` payload entries are followed by one sector-bitmap entry (only used
//! by differencing disks, which are not supported).
//!
//! We never write through the log. A pending log left by another tool is replayed first
//! (into the file when opened writable, or as an in-memory overlay when read-only), and
//! the headers are rewritten with a zero LogGuid before our first change.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use super::{ContainerInfo, ContainerKind, Disk, read_up_to};
use crate::error::{Error, Result};
use crate::util::*;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const HEADER_OFFSETS: [u64; 2] = [64 * KIB, 128 * KIB];
const REGION_OFFSETS: [u64; 2] = [192 * KIB, 256 * KIB];
const HEADER_SIZE: usize = 4096;
const REGION_SIZE: usize = 64 * 1024;

const BAT_GUID: &str = "2DC27766-F623-4200-9D64-115E9BFD4A08";
const METADATA_GUID: &str = "8B7CA206-4790-4B9A-B8FE-575F050F886E";
const FILE_PARAMS: &str = "CAA16737-FA36-4D43-B3B6-33F0AA44E76B";
const VIRTUAL_SIZE: &str = "2FA54224-CD1B-4876-B211-5DBED83BF4B8";
const PAGE83: &str = "BECA12AB-B2E6-4523-93EF-C309E000C746";
const LOGICAL_SECTOR: &str = "8141BF1D-A96F-4709-BA47-F233A8FAAB5F";
const PHYSICAL_SECTOR: &str = "CDA348C7-445D-4471-9CC9-E9885251C556";
const PARENT_LOCATOR: &str = "A8D35F2D-B30B-454D-ABF7-D3D84834AB0C";

const STATE_MASK: u64 = 7;
const PAYLOAD_FULLY_PRESENT: u64 = 6;
const PAYLOAD_PARTIALLY_PRESENT: u64 = 7;
const OFFSET_MASK: u64 = !0xF_FFFF;

/// A byte range of the file as it would read after replaying the log.
struct Patch {
    offset: u64,
    len: u64,
    /// `None` for a zero descriptor.
    data: Option<Vec<u8>>,
}

pub(crate) struct VhdxDisk {
    file: File,
    writable: bool,
    header: [u8; HEADER_SIZE],
    header_slot: usize,
    headers_updated: bool,
    size: u64,
    block_size: u64,
    logical_sector: u32,
    chunk_ratio: u64,
    bat_offset: u64,
    bat: Vec<u64>,
    fixed: bool,
    file_len: u64,
    overlay: Vec<Patch>,
}

/// Read file bytes as they would be after the log replay; past EOF reads zeros.
fn pread(file: &mut File, overlay: &[Patch], offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let got = read_up_to(file, offset, buf)?;
    buf[got..].fill(0);
    let end = offset + buf.len() as u64;
    for p in overlay {
        let (s, e) = (p.offset.max(offset), (p.offset + p.len).min(end));
        if s >= e {
            continue;
        }
        let dst = &mut buf[(s - offset) as usize..(e - offset) as usize];
        match &p.data {
            Some(d) => dst.copy_from_slice(&d[(s - p.offset) as usize..(e - p.offset) as usize]),
            None => dst.fill(0),
        }
    }
    Ok(())
}

fn header_valid(h: &[u8]) -> bool {
    if &h[0..4] != b"head" || le16(h, 66) != 1 {
        return false;
    }
    let mut copy = h.to_vec();
    put_le32(&mut copy, 4, 0);
    crc32c(&copy) == le32(h, 4)
}

fn seal(buf: &mut [u8]) {
    put_le32(buf, 4, 0);
    let c = crc32c(buf);
    put_le32(buf, 4, c);
}

fn region_table_valid(r: &[u8]) -> bool {
    if &r[0..4] != b"regi" || le32(r, 8) > 2047 {
        return false;
    }
    let mut copy = r.to_vec();
    put_le32(&mut copy, 4, 0);
    crc32c(&copy) == le32(r, 4)
}

struct LogEntry {
    offset: usize,
    len: usize,
    seq: u64,
    tail: usize,
    last_file_offset: u64,
    patches: Vec<Patch>,
}

/// Parse and fully validate the log entry starting at `at` (the log is circular).
fn parse_log_entry(log: &[u8], at: usize, guid: &[u8]) -> Option<LogEntry> {
    let n = log.len();
    let read =
        |off: usize, len: usize| -> Vec<u8> { (0..len).map(|i| log[(off + i) % n]).collect() };
    let hdr = read(at, 64);
    if &hdr[0..4] != b"loge" || &hdr[32..48] != guid {
        return None;
    }
    let len = le32(&hdr, 8) as usize;
    if len == 0 || !len.is_multiple_of(4096) || len > n {
        return None;
    }
    let mut whole = read(at, len);
    let sum = le32(&whole, 4);
    put_le32(&mut whole, 4, 0);
    if crc32c(&whole) != sum {
        return None;
    }
    let seq = le64(&hdr, 16);
    let count = le32(&hdr, 24) as usize;
    let desc_end = 64 + count * 32;
    if desc_end > len {
        return None;
    }
    let mut data_pos = round_up(desc_end as u64, 4096) as usize;
    let mut patches = Vec::with_capacity(count);
    for i in 0..count {
        let d = &whole[64 + i * 32..96 + i * 32];
        if le64(d, 24) != seq {
            return None;
        }
        match &d[0..4] {
            b"zero" => patches.push(Patch {
                offset: le64(d, 16),
                len: le64(d, 8),
                data: None,
            }),
            b"desc" => {
                if data_pos + 4096 > len {
                    return None;
                }
                let s = &whole[data_pos..data_pos + 4096];
                let sseq = (u64::from(le32(s, 4)) << 32) | u64::from(le32(s, 4092));
                if &s[0..4] != b"data" || sseq != seq {
                    return None;
                }
                let mut sector = vec![0u8; 4096];
                sector[0..8].copy_from_slice(&d[8..16]);
                sector[8..4092].copy_from_slice(&s[8..4092]);
                sector[4092..].copy_from_slice(&d[4..8]);
                patches.push(Patch {
                    offset: le64(d, 16),
                    len: 4096,
                    data: Some(sector),
                });
                data_pos += 4096;
            }
            _ => return None,
        }
    }
    Some(LogEntry {
        offset: at,
        len,
        seq,
        tail: le32(&hdr, 12) as usize,
        last_file_offset: le64(&hdr, 56),
        patches,
    })
}

/// The writes of the active log sequence, oldest first, plus the minimum file length.
fn log_replay(log: &[u8], guid: &[u8]) -> (Vec<Patch>, u64) {
    let mut entries: Vec<LogEntry> = (0..log.len())
        .step_by(4096)
        .filter_map(|at| parse_log_entry(log, at, guid))
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.seq));
    for head in 0..entries.len() {
        let (head_seq, head_off) = (entries[head].seq, entries[head].offset);
        let mut chain = Vec::new();
        let mut at = entries[head].tail;
        let mut expected = None;
        while let Some(i) = entries.iter().position(|e| e.offset == at) {
            if expected.is_some_and(|x| entries[i].seq != x) || chain.len() > entries.len() {
                break;
            }
            chain.push(i);
            if entries[i].offset == head_off {
                break;
            }
            expected = Some(entries[i].seq + 1);
            at = (entries[i].offset + entries[i].len) % log.len();
        }
        if chain
            .last()
            .is_some_and(|&i| entries[i].seq == head_seq && entries[i].offset == head_off)
        {
            let min_len = entries[head].last_file_offset;
            let mut slots: Vec<Option<LogEntry>> = entries.into_iter().map(Some).collect();
            let out = chain
                .iter()
                .filter_map(|&i| slots[i].take())
                .flat_map(|e| e.patches)
                .collect();
            return (out, min_len);
        }
    }
    (Vec::new(), 0)
}

impl VhdxDisk {
    pub(crate) fn open(path: &Path, writable: bool) -> Result<Self> {
        let mut file = File::options().read(true).write(writable).open(path)?;
        let mut file_len = file.metadata()?.len();
        let mut ident = [0u8; 8];
        read_up_to(&mut file, 0, &mut ident)?;
        if &ident != b"vhdxfile" {
            return Err(Error::Invalid("VHDX file identifier not found".into()));
        }

        let mut best: Option<(usize, [u8; HEADER_SIZE])> = None;
        for (slot, &off) in HEADER_OFFSETS.iter().enumerate() {
            let mut h = [0u8; HEADER_SIZE];
            read_up_to(&mut file, off, &mut h)?;
            if header_valid(&h) && best.as_ref().is_none_or(|(_, b)| le64(&h, 8) > le64(b, 8)) {
                best = Some((slot, h));
            }
        }
        let (header_slot, header) =
            best.ok_or_else(|| Error::Invalid("no valid VHDX header".into()))?;

        // Replay a pending log.
        let mut overlay = Vec::new();
        let mut replayed = false;
        if !is_zero(&header[48..64]) {
            let log_len = u64::from(le32(&header, 68));
            let log_off = le64(&header, 72);
            if log_len > 0 && log_len % MIB == 0 && log_len <= 64 * MIB {
                let mut log = vec![0u8; log_len as usize];
                read_up_to(&mut file, log_off, &mut log)?;
                let (patches, min_len) = log_replay(&log, &header[48..64]);
                if !patches.is_empty() {
                    if writable {
                        for p in &patches {
                            file.seek(SeekFrom::Start(p.offset))?;
                            match &p.data {
                                Some(d) => file.write_all(d)?,
                                None => file.write_all(&vec![0u8; p.len as usize])?,
                            }
                        }
                        if file.metadata()?.len() < min_len {
                            file.set_len(min_len)?;
                        }
                        file.sync_data()?;
                        file_len = file.metadata()?.len();
                    } else {
                        overlay = patches;
                    }
                }
            }
            replayed = writable;
        }

        // Region table.
        let mut regions = None;
        for &off in &REGION_OFFSETS {
            let mut r = vec![0u8; REGION_SIZE];
            pread(&mut file, &overlay, off, &mut r)?;
            if region_table_valid(&r) {
                regions = Some(r);
                break;
            }
        }
        let regions = regions.ok_or_else(|| Error::Invalid("no valid VHDX region table".into()))?;
        let (bat_id, meta_id) = (guid(BAT_GUID), guid(METADATA_GUID));
        let (mut bat_region, mut meta_region) = (None, None);
        for i in 0..le32(&regions, 8) as usize {
            let e = &regions[16 + i * 32..48 + i * 32];
            let region = (le64(e, 16), u64::from(le32(e, 24)));
            if e[0..16] == bat_id {
                bat_region = Some(region);
            } else if e[0..16] == meta_id {
                meta_region = Some(region);
            } else if le32(e, 28) & 1 != 0 {
                return Err(Error::Unsupported(format!(
                    "VHDX required region {}",
                    guid_string(&e[0..16])
                )));
            }
        }
        let (bat_offset, bat_len) =
            bat_region.ok_or_else(|| Error::Invalid("VHDX has no BAT region".into()))?;
        let (meta_offset, meta_len) =
            meta_region.ok_or_else(|| Error::Invalid("VHDX has no metadata region".into()))?;

        // Metadata.
        let mut meta = vec![0u8; meta_len.min(16 * MIB) as usize];
        pread(&mut file, &overlay, meta_offset, &mut meta)?;
        if &meta[0..8] != b"metadata" {
            return Err(Error::Invalid("VHDX metadata table not found".into()));
        }
        let item = |id: &str, len: usize| -> Option<&[u8]> {
            let id = guid(id);
            (0..le16(&meta, 10) as usize).find_map(|i| {
                let e = &meta[32 + i * 32..64 + i * 32];
                let (off, l) = (le32(e, 16) as usize, le32(e, 20) as usize);
                (e[0..16] == id && l >= len && off + len <= meta.len())
                    .then(|| &meta[off..off + len])
            })
        };
        let known = [
            FILE_PARAMS,
            VIRTUAL_SIZE,
            PAGE83,
            LOGICAL_SECTOR,
            PHYSICAL_SECTOR,
            PARENT_LOCATOR,
        ]
        .map(guid);
        for i in 0..le16(&meta, 10) as usize {
            let e = &meta[32 + i * 32..64 + i * 32];
            if le32(e, 24) & 4 != 0 && !known.iter().any(|k| *k == e[0..16]) {
                return Err(Error::Unsupported(format!(
                    "VHDX required metadata {}",
                    guid_string(&e[0..16])
                )));
            }
        }
        let params = item(FILE_PARAMS, 8)
            .ok_or_else(|| Error::Invalid("VHDX file parameters missing".into()))?;
        let block_size = u64::from(le32(params, 0));
        let flags = le32(params, 4);
        if flags & 2 != 0 {
            return Err(Error::Unsupported(
                "differencing VHDX (needs its parent disk)".into(),
            ));
        }
        let size = le64(
            item(VIRTUAL_SIZE, 8)
                .ok_or_else(|| Error::Invalid("VHDX virtual size missing".into()))?,
            0,
        );
        let logical_sector = item(LOGICAL_SECTOR, 4).map(|b| le32(b, 0)).unwrap_or(512);
        if !(MIB..=256 * MIB).contains(&block_size) || !block_size.is_power_of_two() {
            return Err(Error::Invalid(format!(
                "VHDX block size {block_size} is invalid"
            )));
        }
        if logical_sector != 512 && logical_sector != 4096 {
            return Err(Error::Invalid(format!(
                "VHDX logical sector size {logical_sector} is invalid"
            )));
        }
        let chunk_ratio = (1u64 << 23) * u64::from(logical_sector) / block_size;
        let data_blocks = size.div_ceil(block_size);
        let entries = data_blocks + data_blocks.saturating_sub(1) / chunk_ratio;
        if entries * 8 > bat_len {
            return Err(Error::Invalid("VHDX BAT region is too small".into()));
        }
        let mut raw = vec![0u8; (entries * 8) as usize];
        pread(&mut file, &overlay, bat_offset, &mut raw)?;
        let bat = raw.chunks(8).map(|c| le64(c, 0)).collect();

        let mut disk = VhdxDisk {
            file,
            writable,
            header,
            header_slot,
            headers_updated: false,
            size,
            block_size,
            logical_sector,
            chunk_ratio,
            bat_offset,
            bat,
            fixed: flags & 1 != 0,
            file_len,
            overlay,
        };
        if replayed {
            disk.update_headers()?;
        }
        Ok(disk)
    }

    pub(crate) fn create(path: &Path, size: u64, dynamic: bool) -> Result<Self> {
        let logical: u32 = 512;
        let bs = if size <= GIB { 2 * MIB } else { 32 * MIB };
        let chunk_ratio = (1u64 << 23) * u64::from(logical) / bs;
        let data_blocks = size.div_ceil(bs);
        let entries = data_blocks + (data_blocks - 1) / chunk_ratio;
        let (log_off, log_len) = (MIB, MIB);
        let (meta_off, meta_len) = (2 * MIB, MIB);
        let bat_off = 3 * MIB;
        let bat_len = round_up(entries * 8, MIB);
        let data_start = bat_off + bat_len;

        let mut bat = vec![0u64; entries as usize];
        let mut file_len = data_start;
        if !dynamic {
            for b in 0..data_blocks {
                bat[(b + b / chunk_ratio) as usize] = (data_start + b * bs) | PAYLOAD_FULLY_PRESENT;
            }
            file_len = data_start + data_blocks * bs;
        }

        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(file_len)?;

        let mut ident = vec![0u8; 64 * KIB as usize];
        ident[0..8].copy_from_slice(b"vhdxfile");
        for (i, u) in "SectorZero".encode_utf16().enumerate() {
            put_le16(&mut ident, 8 + i * 2, u);
        }
        write_at(&mut file, 0, &ident)?;

        let mut h = [0u8; HEADER_SIZE];
        h[0..4].copy_from_slice(b"head");
        h[16..32].copy_from_slice(&new_guid());
        h[32..48].copy_from_slice(&new_guid());
        put_le16(&mut h, 64, 0);
        put_le16(&mut h, 66, 1);
        put_le32(&mut h, 68, log_len as u32);
        put_le64(&mut h, 72, log_off);
        for (seq, &off) in HEADER_OFFSETS.iter().enumerate() {
            put_le64(&mut h, 8, seq as u64);
            seal(&mut h);
            write_at(&mut file, off, &h)?;
        }

        let mut r = vec![0u8; REGION_SIZE];
        r[0..4].copy_from_slice(b"regi");
        put_le32(&mut r, 8, 2);
        for (i, (id, off, len)) in [
            (BAT_GUID, bat_off, bat_len),
            (METADATA_GUID, meta_off, meta_len),
        ]
        .into_iter()
        .enumerate()
        {
            let e = 16 + i * 32;
            r[e..e + 16].copy_from_slice(&guid(id));
            put_le64(&mut r, e + 16, off);
            put_le32(&mut r, e + 24, len as u32);
            put_le32(&mut r, e + 28, 1);
        }
        seal(&mut r);
        for &off in &REGION_OFFSETS {
            write_at(&mut file, off, &r)?;
        }

        let mut m = vec![0u8; 64 * KIB as usize + 64];
        m[0..8].copy_from_slice(b"metadata");
        let mut params = [0u8; 8];
        put_le32(&mut params, 0, bs as u32);
        put_le32(&mut params, 4, if dynamic { 0 } else { 1 });
        let items: [(&str, Vec<u8>, u32); 5] = [
            (FILE_PARAMS, params.to_vec(), 4),
            (VIRTUAL_SIZE, size.to_le_bytes().to_vec(), 6),
            (LOGICAL_SECTOR, logical.to_le_bytes().to_vec(), 6),
            (PHYSICAL_SECTOR, 4096u32.to_le_bytes().to_vec(), 6),
            (PAGE83, new_guid().to_vec(), 6),
        ];
        put_le16(&mut m, 10, items.len() as u16);
        let mut data_at = 64 * KIB as usize;
        for (i, (id, data, flags)) in items.iter().enumerate() {
            let e = 32 + i * 32;
            m[e..e + 16].copy_from_slice(&guid(id));
            put_le32(&mut m, e + 16, data_at as u32);
            put_le32(&mut m, e + 20, data.len() as u32);
            put_le32(&mut m, e + 24, *flags);
            m[data_at..data_at + data.len()].copy_from_slice(data);
            data_at += data.len();
        }
        write_at(&mut file, meta_off, &m)?;

        let raw: Vec<u8> = bat.iter().flat_map(|e| e.to_le_bytes()).collect();
        write_at(&mut file, bat_off, &raw)?;
        file.sync_all()?;
        drop(file);
        Self::open(path, true)
    }

    fn bat_index(&self, block: u64) -> usize {
        (block + block / self.chunk_ratio) as usize
    }

    /// Before the first change: new write GUIDs, a cleared log GUID, both headers rewritten.
    fn update_headers(&mut self) -> io::Result<()> {
        let mut h = self.header;
        h[16..32].copy_from_slice(&new_guid());
        h[32..48].copy_from_slice(&new_guid());
        h[48..64].fill(0);
        for _ in 0..2 {
            let seq = le64(&h, 8) + 1;
            put_le64(&mut h, 8, seq);
            seal(&mut h);
            let slot = 1 - self.header_slot;
            write_at(&mut self.file, HEADER_OFFSETS[slot], &h)?;
            self.file.sync_data()?;
            self.header_slot = slot;
            self.header = h;
        }
        self.headers_updated = true;
        Ok(())
    }

    fn allocate(&mut self, idx: usize) -> io::Result<u64> {
        let start = round_up(self.file_len, MIB);
        self.file_len = start + self.block_size;
        self.file.set_len(self.file_len)?;
        let entry = start | PAYLOAD_FULLY_PRESENT;
        self.bat[idx] = entry;
        write_at(
            &mut self.file,
            self.bat_offset + idx as u64 * 8,
            &entry.to_le_bytes(),
        )?;
        Ok(start)
    }
}

fn write_at(file: &mut File, offset: u64, data: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)
}

fn present(entry: u64) -> bool {
    matches!(
        entry & STATE_MASK,
        PAYLOAD_FULLY_PRESENT | PAYLOAD_PARTIALLY_PRESENT
    )
}

impl Disk for VhdxDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.logical_sector
    }

    fn read_at(&mut self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            if offset >= self.size {
                buf.fill(0);
                break;
            }
            let block = offset / self.block_size;
            let within = offset % self.block_size;
            let n = (buf.len() as u64)
                .min(self.block_size - within)
                .min(self.size - offset) as usize;
            let (chunk, rest) = buf.split_at_mut(n);
            let entry = self.bat[self.bat_index(block)];
            if present(entry) {
                pread(
                    &mut self.file,
                    &self.overlay,
                    (entry & OFFSET_MASK) + within,
                    chunk,
                )?;
            } else {
                chunk.fill(0);
            }
            offset += n as u64;
            buf = rest;
        }
        Ok(())
    }

    fn write_at(&mut self, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "VHDX opened read-only",
            ));
        }
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the VHDX",
            ));
        }
        while !buf.is_empty() {
            let block = offset / self.block_size;
            let within = offset % self.block_size;
            let n = (buf.len() as u64).min(self.block_size - within) as usize;
            let idx = self.bat_index(block);
            let entry = self.bat[idx];
            let base = if present(entry) {
                Some(entry & OFFSET_MASK)
            } else if is_zero(&buf[..n]) {
                None
            } else {
                if !self.headers_updated {
                    self.update_headers()?;
                }
                Some(self.allocate(idx)?)
            };
            if let Some(base) = base {
                if !self.headers_updated {
                    self.update_headers()?;
                }
                write_at(&mut self.file, base + within, &buf[..n])?;
            }
            offset += n as u64;
            buf = &buf[n..];
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn info(&self) -> ContainerInfo {
        ContainerInfo::simple(
            ContainerKind::Vhdx,
            if self.fixed { "fixed" } else { "dynamic" },
            self.size,
        )
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let first = offset / self.block_size;
        let last = (offset + len.max(1) - 1) / self.block_size;
        Ok((first..=last).any(|b| self.bat.get(self.bat_index(b)).is_some_and(|&e| present(e))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_write_reopen() {
        let dir = tempfile::tempdir().unwrap();
        for dynamic in [true, false] {
            let p = dir.path().join(format!("t{dynamic}.vhdx"));
            let mut d = VhdxDisk::create(&p, 5 * MIB + 512, dynamic).unwrap();
            assert_eq!(d.fixed, !dynamic);
            d.write_at(3 * MIB - 100, &[0xAB; 300]).unwrap();
            d.write_at(5 * MIB, &[0xCD; 512]).unwrap();
            d.sync().unwrap();
            drop(d);
            let mut d = VhdxDisk::open(&p, false).unwrap();
            assert_eq!(d.size(), 5 * MIB + 512);
            let mut b = [0u8; 400];
            d.read_at(3 * MIB - 150, &mut b).unwrap();
            assert!(b[..50].iter().all(|&x| x == 0));
            assert!(b[50..350].iter().all(|&x| x == 0xAB));
            assert!(b[350..].iter().all(|&x| x == 0));
            let mut b = [0u8; 512];
            d.read_at(5 * MIB, &mut b).unwrap();
            assert!(b.iter().all(|&x| x == 0xCD));
            if dynamic {
                assert!(!d.is_allocated(0, MIB).unwrap());
            }
        }
    }

    #[test]
    fn log_replay_applies_data_and_zero_descriptors() {
        let guid = new_guid();
        let mut log = vec![0u8; MIB as usize];
        // One entry: header sector + one data sector, at the start of the log.
        let mut e = vec![0u8; 8192];
        e[0..4].copy_from_slice(b"loge");
        put_le32(&mut e, 8, 8192);
        put_le32(&mut e, 12, 0); // tail = itself
        put_le64(&mut e, 16, 5);
        put_le32(&mut e, 24, 2);
        e[32..48].copy_from_slice(&guid);
        put_le64(&mut e, 56, 4 * MIB);
        let d = 64;
        e[d..d + 4].copy_from_slice(b"desc");
        e[d + 4..d + 8].copy_from_slice(&[9, 9, 9, 9]);
        e[d + 8..d + 16].copy_from_slice(&[7; 8]);
        put_le64(&mut e, d + 16, 3 * MIB);
        put_le64(&mut e, d + 24, 5);
        let z = 96;
        e[z..z + 4].copy_from_slice(b"zero");
        put_le64(&mut e, z + 8, 4096);
        put_le64(&mut e, z + 16, 2 * MIB);
        put_le64(&mut e, z + 24, 5);
        e[4096..4100].copy_from_slice(b"data");
        put_le32(&mut e, 4100, 0);
        e[4104..8188].fill(0x55);
        put_le32(&mut e, 8188, 5);
        seal(&mut e);
        log[..8192].copy_from_slice(&e);
        let (patches, min_len) = log_replay(&log, &guid);
        assert_eq!(min_len, 4 * MIB);
        assert_eq!(patches.len(), 2);
        let data = patches[0].data.as_ref().unwrap();
        assert_eq!(&data[0..8], &[7; 8]);
        assert_eq!(data[100], 0x55);
        assert_eq!(&data[4092..], &[9, 9, 9, 9]);
        assert!(patches[1].data.is_none());
        // A wrong GUID means nothing to replay.
        assert!(log_replay(&log, &new_guid()).0.is_empty());
    }
}
