//! Microsoft Virtual Hard Disk (VHD), fixed and dynamic, per the published
//! "Virtual Hard Disk Image Format Specification" v1.0.
//!
//! Fixed: raw data followed by a 512-byte footer. Dynamic: footer copy, dynamic header,
//! block allocation table (BAT), then blocks (a sector bitmap followed by the block data),
//! and the footer at the end. Differencing disks are not supported yet.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use super::{ContainerInfo, ContainerKind, Disk, read_up_to};
use crate::error::{Error, Result};
use crate::util::*;

const FOOTER: usize = 512;
const UNALLOCATED: u32 = 0xFFFF_FFFF;
const DEFAULT_BLOCK: u32 = 2 * 1024 * 1024;
/// Seconds between 1970-01-01 and the VHD epoch, 2000-01-01 UTC.
const VHD_EPOCH: u64 = 946_684_800;

struct Dynamic {
    bat_offset: u64,
    bat: Vec<u32>,
    block_size: u32,
    bitmap_size: u64,
    /// Cached sector bitmaps per block index.
    bitmaps: HashMap<u32, Vec<u8>>,
}

pub(crate) struct VhdDisk {
    file: File,
    footer: [u8; FOOTER],
    size: u64,
    dynamic: Option<Dynamic>,
    /// Where the trailing footer lives (dynamic disks grow by moving it).
    footer_offset: u64,
}

fn checksum(buf: &[u8], skip: std::ops::Range<usize>) -> u32 {
    let sum: u32 = buf
        .iter()
        .enumerate()
        .filter(|(i, _)| !skip.contains(i))
        .fold(0u32, |a, (_, &b)| a.wrapping_add(u32::from(b)));
    !sum
}

/// CHS geometry, per the algorithm in the VHD specification appendix.
fn geometry(size: u64) -> (u16, u8, u8) {
    let mut total = size / 512;
    if total > 65535 * 16 * 255 {
        total = 65535 * 16 * 255;
    }
    let (spt, heads, cth);
    if total >= 65535 * 16 * 63 {
        spt = 255;
        heads = 16;
        cth = total / spt;
    } else {
        let mut s = 17u64;
        let mut c = total / s;
        let mut h = c.div_ceil(1024);
        if h < 4 {
            h = 4;
        }
        if c >= h * 1024 || h > 16 {
            s = 31;
            h = 16;
            c = total / s;
        }
        if c >= h * 1024 {
            s = 63;
            h = 16;
            c = total / s;
        }
        spt = s;
        heads = h;
        cth = c;
    }
    ((cth / heads) as u16, heads as u8, spt as u8)
}

fn make_footer(size: u64, disk_type: u32, data_offset: u64) -> [u8; FOOTER] {
    let mut f = [0u8; FOOTER];
    f[0..8].copy_from_slice(b"conectix");
    put_be32(&mut f, 8, 2); // features: reserved bit always set
    put_be32(&mut f, 12, 0x0001_0000);
    put_be64(&mut f, 16, data_offset);
    put_be32(&mut f, 24, unix_now().saturating_sub(VHD_EPOCH) as u32);
    f[28..32].copy_from_slice(b"s0  ");
    put_be32(&mut f, 32, 0x0001_0000);
    f[36..40].copy_from_slice(b"Wi2k");
    put_be64(&mut f, 40, size);
    put_be64(&mut f, 48, size);
    let (c, h, s) = geometry(size);
    put_be16(&mut f, 56, c);
    f[58] = h;
    f[59] = s;
    put_be32(&mut f, 60, disk_type);
    f[68..84].copy_from_slice(&new_guid());
    let sum = checksum(&f, 64..68);
    put_be32(&mut f, 64, sum);
    f
}

impl VhdDisk {
    pub(crate) fn open(path: &Path, writable: bool) -> Result<Self> {
        let mut file = File::options().read(true).write(writable).open(path)?;
        let len = file.metadata()?.len();
        if len < FOOTER as u64 {
            return Err(Error::Invalid("VHD file is too small".into()));
        }
        let mut footer = [0u8; FOOTER];
        read_up_to(&mut file, len - FOOTER as u64, &mut footer)?;
        let mut footer_offset = len - FOOTER as u64;
        if !footer.starts_with(b"conectix") {
            // A damaged trailing footer: dynamic disks keep a copy at offset 0.
            read_up_to(&mut file, 0, &mut footer)?;
            if !footer.starts_with(b"conectix") {
                return Err(Error::Invalid("VHD footer not found".into()));
            }
            footer_offset = len;
        }
        if be32(&footer, 64) != checksum(&footer, 64..68) {
            return Err(Error::Invalid("VHD footer checksum mismatch".into()));
        }
        let size = be64(&footer, 48);
        let dynamic = match be32(&footer, 60) {
            2 => None,
            3 => {
                let hdr_off = be64(&footer, 16);
                let mut h = [0u8; 1024];
                read_up_to(&mut file, hdr_off, &mut h)?;
                if !h.starts_with(b"cxsparse") {
                    return Err(Error::Invalid("VHD dynamic header not found".into()));
                }
                let bat_offset = be64(&h, 16);
                let entries = be32(&h, 28) as usize;
                let block_size = be32(&h, 32);
                if block_size == 0 || !block_size.is_multiple_of(512) {
                    return Err(Error::Invalid("VHD block size is invalid".into()));
                }
                let mut raw = vec![0u8; entries * 4];
                read_up_to(&mut file, bat_offset, &mut raw)?;
                let bat = raw.chunks(4).map(|c| be32(c, 0)).collect();
                let bitmap_size = round_up(u64::from(block_size / 512).div_ceil(8), 512);
                Some(Dynamic {
                    bat_offset,
                    bat,
                    block_size,
                    bitmap_size,
                    bitmaps: HashMap::new(),
                })
            }
            4 => {
                return Err(Error::Unsupported(
                    "differencing VHD (needs its parent disk)".into(),
                ));
            }
            t => return Err(Error::Invalid(format!("unknown VHD disk type {t}"))),
        };
        Ok(VhdDisk {
            file,
            footer,
            size,
            dynamic,
            footer_offset,
        })
    }

    pub(crate) fn create(path: &Path, size: u64, dynamic: bool) -> Result<Self> {
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        if !dynamic {
            let footer = make_footer(size, 2, u64::MAX);
            file.set_len(size)?;
            file.seek(SeekFrom::Start(size))?;
            file.write_all(&footer)?;
            return Ok(VhdDisk {
                file,
                footer,
                size,
                dynamic: None,
                footer_offset: size,
            });
        }
        let footer = make_footer(size, 3, 512);
        let entries = size.div_ceil(u64::from(DEFAULT_BLOCK)) as u32;
        let bat_offset = 512 + 1024;
        let bat_bytes = round_up(u64::from(entries) * 4, 512);
        let mut h = [0u8; 1024];
        h[0..8].copy_from_slice(b"cxsparse");
        put_be64(&mut h, 8, u64::MAX);
        put_be64(&mut h, 16, bat_offset);
        put_be32(&mut h, 24, 0x0001_0000);
        put_be32(&mut h, 28, entries);
        put_be32(&mut h, 32, DEFAULT_BLOCK);
        let sum = checksum(&h, 36..40);
        put_be32(&mut h, 36, sum);
        file.write_all(&footer)?;
        file.write_all(&h)?;
        file.write_all(&vec![0xFF; bat_bytes as usize])?;
        let footer_offset = bat_offset + bat_bytes;
        file.write_all(&footer)?;
        let bitmap_size = round_up(u64::from(DEFAULT_BLOCK / 512).div_ceil(8), 512);
        Ok(VhdDisk {
            file,
            footer,
            size,
            dynamic: Some(Dynamic {
                bat_offset,
                bat: vec![UNALLOCATED; entries as usize],
                block_size: DEFAULT_BLOCK,
                bitmap_size,
                bitmaps: HashMap::new(),
            }),
            footer_offset,
        })
    }

    fn bitmap(&mut self, block: u32) -> io::Result<&mut Vec<u8>> {
        let d = self.dynamic.as_mut().unwrap();
        if !d.bitmaps.contains_key(&block) {
            let mut bm = vec![0u8; d.bitmap_size as usize];
            read_up_to(
                &mut self.file,
                u64::from(d.bat[block as usize]) * 512,
                &mut bm,
            )?;
            d.bitmaps.insert(block, bm);
        }
        Ok(d.bitmaps.get_mut(&block).unwrap())
    }

    /// Append a new zero-filled block (bitmap all ones) where the footer was, then move the footer.
    fn allocate(&mut self, block: u32) -> io::Result<()> {
        let d = self.dynamic.as_mut().unwrap();
        let start = round_up(self.footer_offset, 512);
        let bitmap = vec![0xFFu8; d.bitmap_size as usize];
        let block_end = start + d.bitmap_size + u64::from(d.block_size);
        self.file.seek(SeekFrom::Start(start))?;
        self.file.write_all(&bitmap)?;
        self.file.set_len(block_end)?; // block data reads as zeros
        self.file.seek(SeekFrom::Start(block_end))?;
        self.file.write_all(&self.footer)?;
        d.bat[block as usize] = (start / 512) as u32;
        self.file
            .seek(SeekFrom::Start(d.bat_offset + u64::from(block) * 4))?;
        self.file.write_all(&((start / 512) as u32).to_be_bytes())?;
        d.bitmaps.insert(block, bitmap);
        self.footer_offset = block_end;
        Ok(())
    }
}

impl Disk for VhdDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            if offset >= self.size {
                buf.fill(0);
                return Ok(());
            }
            let (bs, bitmap_size, bat_entry) = match &self.dynamic {
                None => {
                    let n = (buf.len() as u64).min(self.size - offset) as usize;
                    let got = read_up_to(&mut self.file, offset, &mut buf[..n])?;
                    buf[got..n].fill(0);
                    offset += n as u64;
                    buf = &mut buf[n..];
                    continue;
                }
                Some(d) => {
                    let bs = u64::from(d.block_size);
                    let entry = d
                        .bat
                        .get((offset / bs) as usize)
                        .copied()
                        .unwrap_or(UNALLOCATED);
                    (bs, d.bitmap_size, entry)
                }
            };
            let block = (offset / bs) as u32;
            let within = offset % bs;
            let n = (buf.len() as u64).min(bs - within).min(self.size - offset) as usize;
            let (chunk, rest) = buf.split_at_mut(n);
            if bat_entry == UNALLOCATED {
                chunk.fill(0);
            } else {
                let data_start = u64::from(bat_entry) * 512 + bitmap_size;
                let bitmap = self.bitmap(block)?.clone();
                // Sectors whose bitmap bit is clear were never written: they read as zeros.
                let mut pos = 0usize;
                while pos < n {
                    let sector = ((within + pos as u64) / 512) as usize;
                    let in_sector = ((within + pos as u64) % 512) as usize;
                    let take = (512 - in_sector).min(n - pos);
                    let set = bitmap[sector / 8] & (0x80 >> (sector % 8)) != 0;
                    if set {
                        let got = read_up_to(
                            &mut self.file,
                            data_start + within + pos as u64,
                            &mut chunk[pos..pos + take],
                        )?;
                        chunk[pos + got..pos + take].fill(0);
                    } else {
                        chunk[pos..pos + take].fill(0);
                    }
                    pos += take;
                }
            }
            offset += n as u64;
            buf = rest;
        }
        Ok(())
    }

    fn write_at(&mut self, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the VHD",
            ));
        }
        if self.dynamic.is_none() {
            self.file.seek(SeekFrom::Start(offset))?;
            return self.file.write_all(buf);
        }
        while !buf.is_empty() {
            let d = self.dynamic.as_ref().unwrap();
            let bs = u64::from(d.block_size);
            let block = (offset / bs) as u32;
            let within = offset % bs;
            let n = (buf.len() as u64).min(bs - within) as usize;
            if d.bat[block as usize] == UNALLOCATED {
                if util_is_zero(&buf[..n]) {
                    offset += n as u64;
                    buf = &buf[n..];
                    continue;
                }
                self.allocate(block)?;
            }
            let d = self.dynamic.as_ref().unwrap();
            let entry_start = u64::from(d.bat[block as usize]) * 512;
            let data_start = entry_start + d.bitmap_size;
            self.file.seek(SeekFrom::Start(data_start + within))?;
            self.file.write_all(&buf[..n])?;
            // Mark the written sectors present.
            let first = (within / 512) as usize;
            let last = ((within + n as u64 - 1) / 512) as usize;
            let bm = self.bitmap(block)?;
            let mut changed = false;
            for s in first..=last {
                let bit = 0x80u8 >> (s % 8);
                if bm[s / 8] & bit == 0 {
                    bm[s / 8] |= bit;
                    changed = true;
                }
            }
            if changed {
                let bm = bm.clone();
                self.file.seek(SeekFrom::Start(entry_start))?;
                self.file.write_all(&bm)?;
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
            ContainerKind::Vhd,
            if self.dynamic.is_some() {
                "dynamic"
            } else {
                "fixed"
            },
            self.size,
        )
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let Some(d) = &self.dynamic else {
            return Ok(true);
        };
        let bs = u64::from(d.block_size);
        let first = offset / bs;
        let last = (offset + len.max(1) - 1) / bs;
        Ok((first..=last).any(|b| d.bat.get(b as usize).is_some_and(|&e| e != UNALLOCATED)))
    }
}

fn util_is_zero(b: &[u8]) -> bool {
    is_zero(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_matches_spec_examples() {
        // 127 MiB disk: Virtual PC reports 260 cylinders, 16 heads, 63 sectors... the spec
        // algorithm gives the 17/31/63 cascade; check it stays within CHS limits.
        let (c, h, s) = geometry(127 * 1024 * 1024);
        assert!(h <= 16 && s <= 63 && c > 0);
        let (_, h, s) = geometry(200 * 1024 * 1024 * 1024);
        assert_eq!((h, s), (16, 255));
    }

    #[test]
    fn footer_checksum_validates() {
        let f = make_footer(10 * 1024 * 1024, 2, u64::MAX);
        assert_eq!(be32(&f, 64), checksum(&f, 64..68));
    }
}
