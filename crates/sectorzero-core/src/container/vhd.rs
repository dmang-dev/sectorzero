//! Microsoft Virtual Hard Disk (VHD), fixed and dynamic, per the published
//! "Virtual Hard Disk Image Format Specification" v1.0.
//!
//! Fixed: raw data followed by a 512-byte footer. Dynamic: footer copy, dynamic header,
//! block allocation table (BAT), then blocks (a sector bitmap followed by the block data),
//! and the footer at the end. Differencing (type 4, which includes Virtual PC undo disks,
//! .vud) is a dynamic disk whose unwritten sectors come from a parent VHD, found through the
//! parent locators in the dynamic header and checked against the parent's unique ID.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use super::{ContainerInfo, ContainerKind, Disk, read_up_to};
use crate::error::{Error, Result};
use crate::util::*;

const FOOTER: usize = 512;
const UNALLOCATED: u32 = 0xFFFF_FFFF;
const DEFAULT_BLOCK: u32 = 2 * 1024 * 1024;
/// Seconds between 1970-01-01 and the VHD epoch, 2000-01-01 UTC.
const VHD_EPOCH: u64 = 946_684_800;
/// Longest parent chain we follow (also stops locator loops).
const MAX_CHAIN: usize = 32;
/// Dynamic header: parent locator table (8 entries of 24 bytes).
const LOCATORS: usize = 576;

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
    /// Differencing disks: the parent that supplies sectors the child never wrote.
    parent: Option<Box<VhdDisk>>,
    parent_path: Option<PathBuf>,
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

/// Recompute the footer checksum after editing fields.
fn reseal_footer(f: &mut [u8; FOOTER]) {
    let sum = checksum(f, 64..68);
    put_be32(f, 64, sum);
}

fn dynamic_header(size: u64, block_size: u32) -> ([u8; 1024], u32, u64, u64) {
    let entries = size.div_ceil(u64::from(block_size)) as u32;
    let bat_offset = 512 + 1024;
    let bat_bytes = round_up(u64::from(entries) * 4, 512);
    let mut h = [0u8; 1024];
    h[0..8].copy_from_slice(b"cxsparse");
    put_be64(&mut h, 8, u64::MAX);
    put_be64(&mut h, 16, bat_offset);
    put_be32(&mut h, 24, 0x0001_0000);
    put_be32(&mut h, 28, entries);
    put_be32(&mut h, 32, block_size);
    (h, entries, bat_offset, bat_bytes)
}

pub(super) fn utf16_string(b: &[u8], big_endian: bool) -> String {
    let units: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            if big_endian {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

pub(super) fn utf16_bytes(s: &str, big_endian: bool) -> Vec<u8> {
    s.encode_utf16()
        .flat_map(|u| {
            if big_endian {
                u.to_be_bytes()
            } else {
                u.to_le_bytes()
            }
        })
        .collect()
}

/// `file://localhost/Volumes/x/parent.vhd` (the Mac OS X locator) as a path.
fn mac_url_path(url: &str) -> String {
    let rest = url.trim_end_matches('\0');
    let rest = rest.strip_prefix("file://").unwrap_or(rest);
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parent paths named by a differencing disk's dynamic header, best first: the Windows
/// relative and absolute locators (UTF-16), the deprecated ANSI ones, the Mac URL, and last
/// the parent's Unicode name.
fn parent_names(file: &mut File, h: &[u8]) -> io::Result<Vec<String>> {
    let mut found = Vec::new();
    for i in 0..8 {
        let e = &h[LOCATORS + i * 24..LOCATORS + (i + 1) * 24];
        let rank = match &e[0..4] {
            b"W2ru" => 0,
            b"W2ku" => 1,
            b"Wi2r" => 2,
            b"Wi2k" => 3,
            b"MacX" => 4,
            _ => continue,
        };
        let len = be32(e, 8) as usize;
        if len == 0 || len > 64 * 1024 {
            continue;
        }
        let mut data = vec![0u8; len];
        let got = read_up_to(file, be64(e, 16), &mut data)?;
        data.truncate(got);
        let text = match rank {
            0 | 1 => utf16_string(&data, false),
            4 => mac_url_path(&String::from_utf8_lossy(&data)),
            _ => String::from_utf8_lossy(&data)
                .trim_end_matches('\0')
                .to_string(),
        };
        if !text.is_empty() {
            found.push((rank, text));
        }
    }
    found.sort_by_key(|f| f.0);
    let mut names: Vec<String> = found.into_iter().map(|f| f.1).collect();
    let unicode = utf16_string(&h[64..576], true);
    if !unicode.is_empty() {
        names.push(unicode);
    }
    Ok(names)
}

/// A locator string as a host path, relative ones taken from the child's folder.
pub(super) fn resolve_name(child_dir: &Path, name: &str) -> PathBuf {
    let native = if cfg!(windows) {
        name.to_string()
    } else {
        name.replace('\\', "/")
    };
    child_dir.join(native)
}

fn find_parent(file: &mut File, h: &[u8], child: &Path) -> Result<PathBuf> {
    let dir = child
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let names = parent_names(file, h)?;
    for name in &names {
        let p = resolve_name(dir, name);
        if p.is_file() {
            return Ok(p);
        }
        // A locator path that went stale: the parent may sit next to the child.
        if let Some(base) = Path::new(&name.replace('\\', "/")).file_name() {
            let p = dir.join(base);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(Error::Invalid(if names.is_empty() {
        "differencing VHD has no parent locator".into()
    } else {
        format!(
            "parent disk of the differencing VHD not found (looked for {})",
            names.join(", ")
        )
    }))
}

fn component_key(c: &Component) -> String {
    let s = c.as_os_str().to_string_lossy();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.into_owned()
    }
}

/// `to` relative to the folder `from`, Windows style (`.\\parent.vhd`, `..\\base\\x.vhd`),
/// or `None` when they are on different drives.
pub(super) fn relative_windows_path(from: &Path, to: &Path) -> Option<String> {
    let a: Vec<Component> = from.components().collect();
    let b: Vec<Component> = to.components().collect();
    if a.first().map(component_key) != b.first().map(component_key) {
        return None;
    }
    let common = a
        .iter()
        .zip(&b)
        .take_while(|(x, y)| component_key(x) == component_key(y))
        .count();
    let mut parts: Vec<String> = Vec::new();
    if common == a.len() {
        parts.push(".".into());
    } else {
        parts.extend((common..a.len()).map(|_| "..".to_string()));
    }
    parts.extend(
        b[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    Some(parts.join("\\"))
}

pub(super) fn windows_path_string(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    if cfg!(windows) {
        s.to_string()
    } else {
        s.replace('/', "\\")
    }
}

impl VhdDisk {
    pub(crate) fn open(path: &Path, writable: bool) -> Result<Self> {
        Self::open_chain(path, writable, 0)
    }

    fn open_chain(path: &Path, writable: bool, depth: usize) -> Result<Self> {
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
        let disk_type = be32(&footer, 60);
        let mut parent = None;
        let mut parent_path = None;
        let dynamic = match disk_type {
            2 => None,
            3 | 4 => {
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
                if disk_type == 4 {
                    if depth >= MAX_CHAIN {
                        return Err(Error::Invalid(
                            "differencing VHD chain is too long (or loops)".into(),
                        ));
                    }
                    let found = find_parent(&mut file, &h, path)?;
                    let p = VhdDisk::open_chain(&found, false, depth + 1).map_err(|e| {
                        Error::Invalid(format!("parent disk {}: {e}", found.display()))
                    })?;
                    if p.footer[68..84] != h[40..56] {
                        return Err(Error::Invalid(format!(
                            "parent disk {} is not the one this differencing VHD was made \
                             from (unique ID {} expected, found {})",
                            found.display(),
                            guid_string(&h[40..56]),
                            guid_string(&p.footer[68..84]),
                        )));
                    }
                    parent = Some(Box::new(p));
                    parent_path = Some(found);
                }
                Some(Dynamic {
                    bat_offset,
                    bat,
                    block_size,
                    bitmap_size,
                    bitmaps: HashMap::new(),
                })
            }
            t => return Err(Error::Invalid(format!("unknown VHD disk type {t}"))),
        };
        Ok(VhdDisk {
            file,
            footer,
            size,
            dynamic,
            footer_offset,
            parent,
            parent_path,
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
                parent: None,
                parent_path: None,
            });
        }
        let footer = make_footer(size, 3, 512);
        let (mut h, entries, bat_offset, bat_bytes) = dynamic_header(size, DEFAULT_BLOCK);
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
            parent: None,
            parent_path: None,
        })
    }

    /// Create an empty differencing disk over `parent_path` (any VHD, itself possibly
    /// differencing). The child records the parent's unique ID and both an absolute (`W2ku`)
    /// and, when on the same drive, a relative (`W2ru`) parent locator.
    pub(crate) fn create_differencing(path: &Path, parent_path: &Path) -> Result<Self> {
        let parent = VhdDisk::open(parent_path, false).map_err(|e| {
            Error::Invalid(format!(
                "parent {} is not a usable VHD: {e}",
                parent_path.display()
            ))
        })?;
        let size = parent.size;
        let block_size = parent
            .dynamic
            .as_ref()
            .map_or(DEFAULT_BLOCK, |d| d.block_size);
        let mut footer = make_footer(size, 4, 512);
        footer[56..60].copy_from_slice(&parent.footer[56..60]); // same geometry as the parent
        reseal_footer(&mut footer);

        let abs_parent = std::path::absolute(parent_path)?;
        let abs_child = std::path::absolute(path)?;
        let child_dir = abs_child.parent().unwrap_or(Path::new("."));
        let abs_name = windows_path_string(&abs_parent);
        let mut locators: Vec<(&[u8; 4], Vec<u8>)> = vec![(b"W2ku", utf16_bytes(&abs_name, false))];
        if let Some(rel) = relative_windows_path(child_dir, &abs_parent) {
            locators.push((b"W2ru", utf16_bytes(&rel, false)));
        }

        let (mut h, entries, bat_offset, bat_bytes) = dynamic_header(size, block_size);
        h[40..56].copy_from_slice(&parent.footer[68..84]);
        let mtime = std::fs::metadata(parent_path)?
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        put_be32(&mut h, 56, mtime.saturating_sub(VHD_EPOCH) as u32);
        let unicode_name = if abs_name.encode_utf16().count() <= 255 {
            abs_name.clone()
        } else {
            abs_parent
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let name = utf16_bytes(&unicode_name, true);
        let n = name.len().min(510);
        h[64..64 + n].copy_from_slice(&name[..n]);
        let mut at = bat_offset + bat_bytes;
        let mut data_area = Vec::new();
        for (i, (code, data)) in locators.iter().enumerate() {
            let e = LOCATORS + i * 24;
            let space = round_up(data.len() as u64, 512);
            h[e..e + 4].copy_from_slice(&code[..]);
            // Windows records the reserved space in bytes (the spec says sectors); readers
            // only rely on the length and offset.
            put_be32(&mut h, e + 4, space as u32);
            put_be32(&mut h, e + 8, data.len() as u32);
            put_be64(&mut h, e + 16, at);
            let mut padded = data.clone();
            padded.resize(space as usize, 0);
            data_area.extend_from_slice(&padded);
            at += space;
        }
        let sum = checksum(&h, 36..40);
        put_be32(&mut h, 36, sum);

        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(&footer)?;
        file.write_all(&h)?;
        file.write_all(&vec![0xFF; bat_bytes as usize])?;
        file.write_all(&data_area)?;
        file.write_all(&footer)?;
        let bitmap_size = round_up(u64::from(block_size / 512).div_ceil(8), 512);
        Ok(VhdDisk {
            file,
            footer,
            size,
            dynamic: Some(Dynamic {
                bat_offset,
                bat: vec![UNALLOCATED; entries as usize],
                block_size,
                bitmap_size,
                bitmaps: HashMap::new(),
            }),
            footer_offset: at,
            parent: Some(Box::new(parent)),
            parent_path: Some(parent_path.to_path_buf()),
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

    /// Append a new zero-filled block where the footer was, then move the footer. Its sector
    /// bitmap is all ones on a dynamic disk (the zeros are the data) and all zeros on a
    /// differencing disk (unwritten sectors still come from the parent).
    fn allocate(&mut self, block: u32) -> io::Result<()> {
        let fill = if self.parent.is_some() { 0 } else { 0xFF };
        let d = self.dynamic.as_mut().unwrap();
        let start = round_up(self.footer_offset, 512);
        let bitmap = vec![fill; d.bitmap_size as usize];
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

    /// Sectors this disk does not store: the parent's data, or zeros.
    fn read_absent(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        match self.parent.as_mut() {
            Some(p) => p.read_at(offset, buf),
            None => {
                buf.fill(0);
                Ok(())
            }
        }
    }

    /// Dynamic/differencing write of whole sectors (any range on a dynamic disk).
    fn write_blocks(&mut self, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
        let differencing = self.parent.is_some();
        while !buf.is_empty() {
            let d = self.dynamic.as_ref().unwrap();
            let bs = u64::from(d.block_size);
            let block = (offset / bs) as u32;
            let within = offset % bs;
            let n = (buf.len() as u64).min(bs - within) as usize;
            if d.bat[block as usize] == UNALLOCATED {
                // Zeros need no block on a dynamic disk; on a differencing disk they must
                // hide the parent's data.
                if !differencing && util_is_zero(&buf[..n]) {
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
}

fn bit_set(bitmap: &[u8], sector: usize) -> bool {
    bitmap
        .get(sector / 8)
        .is_some_and(|b| b & (0x80 >> (sector % 8)) != 0)
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
                self.read_absent(offset, chunk)?;
            } else {
                let data_start = u64::from(bat_entry) * 512 + bitmap_size;
                let bitmap = self.bitmap(block)?.clone();
                // Runs of sectors with the same bitmap bit: set ones are stored here, clear
                // ones were never written (zeros, or the parent's data).
                let sector_at = |p: usize| ((within + p as u64) / 512) as usize;
                let mut pos = 0usize;
                while pos < n {
                    let set = bit_set(&bitmap, sector_at(pos));
                    let mut end = ((sector_at(pos) as u64 + 1) * 512 - within) as usize;
                    while end < n && bit_set(&bitmap, sector_at(end)) == set {
                        end += 512;
                    }
                    let end = end.min(n);
                    let part = &mut chunk[pos..end];
                    if set {
                        let got =
                            read_up_to(&mut self.file, data_start + within + pos as u64, part)?;
                        part[got..].fill(0);
                    } else {
                        self.read_absent(offset + pos as u64, part)?;
                    }
                    pos = end;
                }
            }
            offset += n as u64;
            buf = rest;
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
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
        let end = offset + buf.len() as u64;
        if self.parent.is_some() && (!offset.is_multiple_of(512) || !end.is_multiple_of(512)) {
            // A differencing disk marks whole sectors: merge partial ones with what reads
            // there now (the parent's data) first.
            let start = offset / 512 * 512;
            let mut whole = vec![0u8; (round_up(end, 512) - start) as usize];
            self.read_at(start, &mut whole)?;
            whole[(offset - start) as usize..][..buf.len()].copy_from_slice(buf);
            return self.write_blocks(start, &whole);
        }
        self.write_blocks(offset, buf)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn info(&self) -> ContainerInfo {
        let variant = match (&self.dynamic, &self.parent) {
            (None, _) => "fixed",
            (Some(_), None) => "dynamic",
            (Some(_), Some(_)) => "differencing",
        };
        let mut info = ContainerInfo::simple(ContainerKind::Vhd, variant, self.size);
        info.parent = self.parent_path.as_ref().map(|p| p.display().to_string());
        info
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let Some(d) = &self.dynamic else {
            return Ok(true);
        };
        let bs = u64::from(d.block_size);
        let first = offset / bs;
        let last = (offset + len.max(1) - 1) / bs;
        if (first..=last).any(|b| d.bat.get(b as usize).is_some_and(|&e| e != UNALLOCATED)) {
            return Ok(true);
        }
        match self.parent.as_mut() {
            Some(p) => p.is_allocated(offset, len),
            None => Ok(false),
        }
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

    #[test]
    fn relative_locator_paths() {
        let base = if cfg!(windows) { r"C:\vms" } else { "/vms" };
        let dir = Path::new(base).join("child");
        let same = dir.join("base.vhd");
        assert_eq!(
            relative_windows_path(&dir, &same).as_deref(),
            Some(r".\base.vhd")
        );
        let up = Path::new(base).join("parents").join("base.vhd");
        assert_eq!(
            relative_windows_path(&dir, &up).as_deref(),
            Some(r"..\parents\base.vhd")
        );
        assert_eq!(
            mac_url_path("file://localhost/Users/a%20b/p.vhd"),
            "/Users/a b/p.vhd"
        );
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect()
    }

    #[test]
    fn differencing_reads_parent_and_keeps_writes_in_child() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base.vhd");
        let size = 8 * 1024 * 1024;
        for dynamic in [false, true] {
            {
                let mut p = VhdDisk::create(&base, size, dynamic).unwrap();
                p.write_at(0, &pattern(3 * 1024 * 1024, 1)).unwrap();
                p.sync().unwrap();
            }
            let parent_before = std::fs::read(&base).unwrap();
            let sub = tmp.path().join("sub");
            std::fs::create_dir_all(&sub).unwrap();
            let child = sub.join("undo.vud");
            {
                let mut c = VhdDisk::create_differencing(&child, &base).unwrap();
                assert_eq!(c.info().variant, "differencing");
                // Unaligned write straddling sectors and a block boundary.
                c.write_at(2 * 1024 * 1024 - 100, &[0xAB; 300]).unwrap();
                c.write_at(5 * 1024 * 1024, &[0u8; 4096]).unwrap();
                c.sync().unwrap();
            }
            assert_eq!(
                std::fs::read(&base).unwrap(),
                parent_before,
                "parent untouched"
            );
            let mut c = VhdDisk::open(&child, false).unwrap();
            assert_eq!(c.size(), size);
            let mut expect = pattern(3 * 1024 * 1024, 1);
            expect.resize(size as usize, 0);
            expect[2 * 1024 * 1024 - 100..2 * 1024 * 1024 + 200].fill(0xAB);
            let mut got = vec![0u8; size as usize];
            c.read_at(0, &mut got).unwrap();
            assert!(got == expect, "merged view (dynamic parent: {dynamic})");
            assert!(c.is_allocated(0, 4096).unwrap());
            assert!(c.info().parent.unwrap().ends_with("base.vhd"));
            // A chain: grandchild over the child.
            let gc = tmp.path().join("gc.vhd");
            let mut g = VhdDisk::create_differencing(&gc, &child).unwrap();
            g.write_at(512, &[7u8; 512]).unwrap();
            drop(g);
            let mut g = VhdDisk::open(&gc, false).unwrap();
            let mut s = vec![0u8; 2048];
            g.read_at(0, &mut s).unwrap();
            assert_eq!(&s[..512], &expect[..512]);
            assert_eq!(&s[512..1024], &[7u8; 512]);
            assert_eq!(&s[1024..], &expect[1024..2048]);
        }
    }

    #[test]
    fn differencing_rejects_a_replaced_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base.vhd");
        VhdDisk::create(&base, 1 << 20, true).unwrap();
        let child = tmp.path().join("child.vhd");
        VhdDisk::create_differencing(&child, &base).unwrap();
        assert!(VhdDisk::open(&child, false).is_ok());
        VhdDisk::create(&base, 1 << 20, true).unwrap(); // new unique ID
        let err = VhdDisk::open(&child, false).err().unwrap().to_string();
        assert!(err.contains("unique ID"), "{err}");
        std::fs::remove_file(&base).unwrap();
        let err = VhdDisk::open(&child, false).err().unwrap().to_string();
        assert!(err.contains("not found"), "{err}");
    }
}
