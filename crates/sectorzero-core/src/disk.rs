//! Raw image access: the byte window a FAT volume lives in, and MBR partition detection.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::Serialize;

use crate::error::{Error, Result};

const SECTOR: u64 = 512;
/// Read cache window. Reads are served from it; writes go straight to the file (write-through)
/// so a failed write is reported to the caller instead of being lost in a later flush.
const CACHE_BYTES: usize = 64 * 1024;

/// A byte range of an image file presented as its own seekable stream (a partition, or the
/// whole file for a bare volume). Reads past the end of a short file return zeros, so images
/// saved with "truncate unused space" still open.
pub(crate) struct Window {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
    writable: bool,
    cache: Vec<u8>,
    cache_start: u64,
    cache_len: usize,
}

impl Window {
    pub(crate) fn new(file: File, start: u64, len: u64, writable: bool) -> Self {
        Window {
            file,
            start,
            len,
            pos: 0,
            writable,
            cache: vec![0; CACHE_BYTES],
            cache_start: 0,
            cache_len: 0,
        }
    }

    fn fill_cache(&mut self, at: u64) -> io::Result<()> {
        let aligned = at - at % SECTOR;
        let want = (self.len - aligned).min(CACHE_BYTES as u64) as usize;
        self.file.seek(SeekFrom::Start(self.start + aligned))?;
        let mut got = 0;
        while got < want {
            match self.file.read(&mut self.cache[got..want]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.cache[got..want].fill(0);
        self.cache_start = aligned;
        self.cache_len = want;
        Ok(())
    }
}

impl Read for Window {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let cache_end = self.cache_start + self.cache_len as u64;
        if self.pos < self.cache_start || self.pos >= cache_end {
            self.fill_cache(self.pos)?;
        }
        let offset = (self.pos - self.cache_start) as usize;
        let n = buf.len().min(self.cache_len - offset);
        buf[..n].copy_from_slice(&self.cache[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for Window {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "image is open read-only",
            ));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the volume",
            ));
        }
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        self.file.write_all(&buf[..n])?;
        // Keep the read cache coherent with what was just written.
        let (w_start, w_end) = (self.pos, self.pos + n as u64);
        let (c_start, c_end) = (self.cache_start, self.cache_start + self.cache_len as u64);
        let (o_start, o_end) = (w_start.max(c_start), w_end.min(c_end));
        if o_start < o_end {
            let dst = (o_start - c_start) as usize;
            let src = (o_start - w_start) as usize;
            let len = (o_end - o_start) as usize;
            self.cache[dst..dst + len].copy_from_slice(&buf[src..src + len]);
        }
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for Window {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(o) => i128::from(o),
            SeekFrom::End(o) => i128::from(self.len) + i128::from(o),
            SeekFrom::Current(o) => i128::from(self.pos) + i128::from(o),
        };
        if target < 0 || target > i128::from(self.len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek outside the volume",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

/// One primary partition from an MBR partition table.
#[derive(Debug, Clone, Serialize)]
pub struct PartitionInfo {
    /// 1-based slot in the MBR (1–4).
    pub index: usize,
    pub type_id: u8,
    pub type_name: &'static str,
    pub bootable: bool,
    pub start_lba: u64,
    pub sectors: u64,
    pub is_fat: bool,
}

impl PartitionInfo {
    pub fn offset(&self) -> u64 {
        self.start_lba * SECTOR
    }
    pub fn size_bytes(&self) -> u64 {
        self.sectors * SECTOR
    }
}

/// How the image is laid out.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "partitions")]
pub enum Layout {
    /// A FAT boot sector at offset 0, no partition table (floppies, WinImage partition images).
    BareVolume,
    Mbr(Vec<PartitionInfo>),
    Gpt,
    Unknown,
}

fn partition_type(id: u8) -> (&'static str, bool) {
    match id {
        0x01 => ("FAT12", true),
        0x04 => ("FAT16 <32M", true),
        0x05 => ("Extended", false),
        0x06 => ("FAT16", true),
        0x07 => ("NTFS/exFAT", false),
        0x0B => ("FAT32 (CHS)", true),
        0x0C => ("FAT32 (LBA)", true),
        0x0E => ("FAT16 (LBA)", true),
        0x0F => ("Extended (LBA)", false),
        0x11 | 0x14 | 0x16 | 0x1B | 0x1C | 0x1E => ("Hidden FAT", true),
        0x82 => ("Linux swap", false),
        0x83 => ("Linux", false),
        0xEE => ("GPT protective", false),
        0xEF => ("EFI system", true),
        _ => ("Unknown", false),
    }
}

/// Does this sector look like a FAT boot sector (as opposed to an MBR)?
pub(crate) fn looks_like_fat_boot_sector(s: &[u8; 512]) -> bool {
    let jump_ok = s[0] == 0xE9 || (s[0] == 0xEB && s[2] == 0x90);
    let bps = u16::from_le_bytes([s[11], s[12]]);
    let spc = s[13];
    let reserved = u16::from_le_bytes([s[14], s[15]]);
    let fats = s[16];
    jump_ok
        && matches!(bps, 512 | 1024 | 2048 | 4096)
        && spc != 0
        && spc.is_power_of_two()
        && reserved >= 1
        && (1..=4).contains(&fats)
}

pub(crate) fn layout_of(s: &[u8; 512]) -> Layout {
    if looks_like_fat_boot_sector(s) {
        return Layout::BareVolume;
    }
    if s[510] != 0x55 || s[511] != 0xAA {
        return Layout::Unknown;
    }
    let mut parts = Vec::new();
    for i in 0..4 {
        let e = &s[446 + i * 16..446 + (i + 1) * 16];
        let type_id = e[4];
        let start_lba = u64::from(u32::from_le_bytes([e[8], e[9], e[10], e[11]]));
        let sectors = u64::from(u32::from_le_bytes([e[12], e[13], e[14], e[15]]));
        if type_id == 0 || sectors == 0 {
            continue;
        }
        if type_id == 0xEE {
            return Layout::Gpt;
        }
        let (type_name, is_fat) = partition_type(type_id);
        parts.push(PartitionInfo {
            index: i + 1,
            type_id,
            type_name,
            bootable: e[0] == 0x80,
            start_lba,
            sectors,
            is_fat,
        });
    }
    if parts.is_empty() {
        Layout::Unknown
    } else {
        Layout::Mbr(parts)
    }
}

pub(crate) fn read_sector0(file: &mut File) -> Result<[u8; 512]> {
    let mut s = [0u8; 512];
    file.seek(SeekFrom::Start(0))?;
    let mut got = 0;
    while got < s.len() {
        match file.read(&mut s[got..])? {
            0 => break,
            n => got += n,
        }
    }
    if got < s.len() {
        return Err(Error::NotFat("file is smaller than one sector".into()));
    }
    Ok(s)
}

/// Inspect an image's layout without mounting anything.
pub fn probe(path: &Path) -> Result<Layout> {
    let mut file = File::open(path)?;
    Ok(layout_of(&read_sector0(&mut file)?))
}
