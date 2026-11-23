//! ISO 9660 CD/DVD images (read-only): plain ISO 9660 names, Joliet (UCS-2) and Rock Ridge
//! (`NM`) long names, multi-extent files, and El Torito boot catalogs (with boot image
//! extraction, as WinImage offers).

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::error::{Error, Result, display};
use crate::util::*;
use crate::volume::{Attributes, Entry, Stats, host_safe_name, join, norm, split_parent};
use crate::window::Window;

const SECTOR: u64 = 2048;

#[derive(Debug, Clone, Serialize)]
pub struct BootEntry {
    pub index: usize,
    /// "x86 BIOS", "UEFI", "PowerPC", "Mac".
    pub platform: String,
    pub bootable: bool,
    /// "no emulation", "1.2 MB floppy", "1.44 MB floppy", "2.88 MB floppy", "hard disk".
    pub media: String,
    pub load_segment: u16,
    /// Sectors (512-byte) the BIOS loads; for emulated media the image is bigger.
    pub sector_count: u16,
    pub lba: u32,
    /// Size of the boot image `boot-image` extracts.
    pub image_size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IsoInfo {
    pub volume_id: String,
    pub system_id: String,
    pub publisher: String,
    pub preparer: String,
    pub application: String,
    pub created: String,
    pub block_size: u32,
    pub volume_blocks: u32,
    pub joliet: bool,
    pub rock_ridge: bool,
    pub boot_entries: Vec<BootEntry>,
}

#[derive(Debug, Clone)]
struct Record {
    name: String,
    raw_name: String,
    extents: Vec<(u32, u32)>,
    size: u64,
    is_dir: bool,
    hidden: bool,
    date: String,
}

pub(crate) struct IsoFs {
    w: RefCell<Window>,
    root: Record,
    joliet: bool,
    rock_ridge: bool,
    info: IsoInfo,
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .trim_end_matches([' ', '\0'])
        .to_string()
}

fn ucs2(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
        .trim_end_matches([' ', '\0'])
        .to_string()
}

fn record_date(d: &[u8]) -> String {
    if d.iter().all(|&x| x == 0) {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        1900 + u32::from(d[0]),
        d[1],
        d[2],
        d[3],
        d[4],
        d[5]
    )
}

fn pvd_date(d: &[u8]) -> String {
    let s = String::from_utf8_lossy(&d[..14]).into_owned();
    if s.chars().all(|c| c == '0') || !s.chars().all(|c| c.is_ascii_digit()) {
        return String::new();
    }
    format!(
        "{}-{}-{} {}:{}:{}",
        &s[0..4],
        &s[4..6],
        &s[6..8],
        &s[8..10],
        &s[10..12],
        &s[12..14]
    )
}

impl IsoFs {
    pub(crate) fn open(mut w: Window) -> Result<IsoFs> {
        let mut pvd = None;
        let mut svd = None;
        let mut boot_catalog = None;
        for i in 16..16 + 64u64 {
            let mut d = vec![0u8; SECTOR as usize];
            w.read_at(i * SECTOR, &mut d)?;
            if &d[1..6] != b"CD001" {
                break;
            }
            match d[0] {
                0 if text(&d[7..39]) == "EL TORITO SPECIFICATION" => {
                    boot_catalog = Some(le32(&d, 71))
                }
                1 if pvd.is_none() => pvd = Some(d),
                2 if matches!(&d[88..91], b"%/@" | b"%/C" | b"%/E") => svd = Some(d),
                255 => break,
                _ => {}
            }
        }
        let pvd = pvd
            .ok_or_else(|| Error::Invalid("ISO 9660 primary volume descriptor not found".into()))?;
        let block_size = u32::from(le16(&pvd, 128));
        if block_size != 2048 {
            return Err(Error::Unsupported(format!(
                "ISO 9660 logical block size {block_size}"
            )));
        }
        let joliet = svd.is_some();
        let root_desc = svd.as_ref().unwrap_or(&pvd);
        let root = parse_record(&root_desc[156..190], joliet, false).expect("root record");
        // Rock Ridge: an "SP" entry in the system use area of the root's "." record.
        let mut rock_ridge = false;
        if !joliet {
            let pr = parse_record(&pvd[156..190], false, false).unwrap();
            let mut first = vec![0u8; SECTOR as usize];
            w.read_at(u64::from(pr.extents[0].0) * SECTOR, &mut first)?;
            let len = first[0] as usize;
            if len >= 34 {
                let su = 33 + first[32] as usize + (1 - first[32] as usize % 2);
                rock_ridge = len >= su + 7
                    && &first[su..su + 2] == b"SP"
                    && first[su + 4] == 0xBE
                    && first[su + 5] == 0xEF;
            }
        }
        let desc = svd.as_ref().unwrap_or(&pvd);
        let mut info = IsoInfo {
            volume_id: if joliet {
                ucs2(&desc[40..72])
            } else {
                text(&pvd[40..72])
            },
            system_id: text(&pvd[8..40]),
            publisher: if joliet {
                ucs2(&desc[318..446])
            } else {
                text(&pvd[318..446])
            },
            preparer: if joliet {
                ucs2(&desc[446..574])
            } else {
                text(&pvd[446..574])
            },
            application: if joliet {
                ucs2(&desc[574..702])
            } else {
                text(&pvd[574..702])
            },
            created: pvd_date(&pvd[813..830]),
            block_size,
            volume_blocks: le32(&pvd, 80),
            joliet,
            rock_ridge,
            boot_entries: vec![],
        };
        if let Some(lba) = boot_catalog {
            info.boot_entries = parse_catalog(&mut w, lba).unwrap_or_default();
        }
        let root = Record {
            name: String::new(),
            ..root
        };
        Ok(IsoFs {
            w: RefCell::new(w),
            root,
            joliet,
            rock_ridge,
            info,
        })
    }

    pub(crate) fn info(&self) -> &IsoInfo {
        &self.info
    }

    fn read_dir(&self, dir: &Record) -> Result<Vec<Record>> {
        let mut out: Vec<Record> = Vec::new();
        let mut w = self.w.borrow_mut();
        let mut data = vec![0u8; dir.size.min(64 * 1024 * 1024) as usize];
        w.read_at(u64::from(dir.extents[0].0) * SECTOR, &mut data)?;
        let mut pos = 0usize;
        let mut continuing = false;
        while pos < data.len() {
            let len = data[pos] as usize;
            if len == 0 {
                pos = (pos / SECTOR as usize + 1) * SECTOR as usize;
                continue;
            }
            if pos + len > data.len() || len < 34 {
                break;
            }
            let rec = &data[pos..pos + len];
            pos += len;
            let name_len = rec[32] as usize;
            if name_len == 1 && (rec[33] == 0 || rec[33] == 1) {
                continue; // "." and ".."
            }
            let Some(mut r) = parse_record(rec, self.joliet, self.rock_ridge) else {
                continue;
            };
            if self.rock_ridge && rr_relocated(rec) {
                continue;
            }
            if self.rock_ridge
                && let Some(n) = self.rr_name(&mut w, rec)
            {
                r.name = n;
            }
            let multi = rec[25] & 0x80 != 0;
            if continuing {
                if let Some(last) = out.last_mut() {
                    last.extents.push(r.extents[0]);
                    last.size += r.size;
                }
            } else {
                out.push(r);
            }
            continuing = multi;
        }
        Ok(out)
    }

    /// Rock Ridge alternate name, following one `CE` continuation area if needed.
    fn rr_name(&self, w: &mut Window, rec: &[u8]) -> Option<String> {
        let name_len = rec[32] as usize;
        let su_start = 33 + name_len + (1 - name_len % 2);
        let mut area = rec.get(su_start..)?.to_vec();
        let mut name = Vec::new();
        let mut found = false;
        for _ in 0..4 {
            let mut ce = None;
            let mut i = 0;
            while i + 4 <= area.len() {
                let (sig, len) = (&area[i..i + 2], area[i + 2] as usize);
                if len < 4 || i + len > area.len() {
                    break;
                }
                match sig {
                    b"NM" if len >= 5 => {
                        if area[i + 4] & 0x06 == 0 {
                            name.extend_from_slice(&area[i + 5..i + len]);
                            found = true;
                        }
                    }
                    b"CE" if len >= 28 => {
                        ce = Some((le32(&area, i + 4), le32(&area, i + 12), le32(&area, i + 20)))
                    }
                    b"ST" => break,
                    _ => {}
                }
                i += len;
            }
            let Some((block, off, len)) = ce else { break };
            let mut next = vec![0u8; len.min(64 * 1024) as usize];
            w.read_at(u64::from(block) * SECTOR + u64::from(off), &mut next)
                .ok()?;
            area = next;
        }
        found.then(|| String::from_utf8_lossy(&name).into_owned())
    }

    fn find(&self, path: &str) -> Result<Option<Record>> {
        if path.is_empty() {
            return Ok(Some(self.root.clone()));
        }
        let mut cur = self.root.clone();
        for part in path.split('/') {
            if !cur.is_dir {
                return Ok(None);
            }
            let want = part.to_lowercase();
            let Some(next) = self
                .read_dir(&cur)?
                .into_iter()
                .find(|r| r.name.to_lowercase() == want || r.raw_name.to_lowercase() == want)
            else {
                return Ok(None);
            };
            cur = next;
        }
        Ok(Some(cur))
    }

    fn to_entry(r: &Record, parent: &str) -> Entry {
        Entry {
            name: r.name.clone(),
            short_name: r.raw_name.clone(),
            path: join(parent, &r.name),
            is_dir: r.is_dir,
            size: if r.is_dir { 0 } else { r.size },
            modified: r.date.clone(),
            created: String::new(),
            accessed: String::new(),
            attributes: Attributes {
                read_only: true,
                hidden: r.hidden,
                system: false,
                archive: false,
            },
        }
    }

    pub(crate) fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let dir = norm(dir)?;
        let rec = self
            .find(&dir)?
            .ok_or_else(|| Error::NotFound(display(&dir)))?;
        if !rec.is_dir {
            return Err(Error::Invalid(format!("not a folder: {}", display(&dir))));
        }
        let mut out: Vec<Entry> = self
            .read_dir(&rec)?
            .iter()
            .map(|r| Self::to_entry(r, &dir))
            .collect();
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    pub(crate) fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let path = norm(path)?;
        if path.is_empty() {
            return Ok(None);
        }
        let (parent, _) = split_parent(&path);
        Ok(self.find(&path)?.map(|r| Self::to_entry(&r, parent)))
    }

    pub(crate) fn extract(&self, src: &str, dest: &Path, overwrite: bool) -> Result<Stats> {
        let src = norm(src)?;
        fs::create_dir_all(dest)?;
        let rec = self
            .find(&src)?
            .ok_or_else(|| Error::NotFound(display(&src)))?;
        let mut stats = Stats::default();
        if src.is_empty() {
            for c in self.read_dir(&rec)? {
                self.extract_rec(&c, dest, overwrite, &mut stats)?;
            }
        } else {
            self.extract_rec(&rec, dest, overwrite, &mut stats)?;
        }
        Ok(stats)
    }

    fn extract_rec(
        &self,
        r: &Record,
        dest: &Path,
        overwrite: bool,
        stats: &mut Stats,
    ) -> Result<()> {
        let target = dest.join(host_safe_name(&r.name));
        if r.is_dir {
            fs::create_dir_all(&target)?;
            stats.dirs += 1;
            for c in self.read_dir(r)? {
                self.extract_rec(&c, &target, overwrite, stats)?;
            }
            return Ok(());
        }
        if !overwrite && target.exists() {
            return Err(Error::Exists(target.display().to_string()));
        }
        let mut out = File::create(&target)?;
        let mut w = self.w.borrow_mut();
        let mut buf = vec![0u8; 1024 * 1024];
        for &(lba, len) in &r.extents {
            let mut done = 0u64;
            while done < u64::from(len) {
                let n = (u64::from(len) - done).min(buf.len() as u64) as usize;
                w.read_at(u64::from(lba) * SECTOR + done, &mut buf[..n])?;
                out.write_all(&buf[..n])?;
                done += n as u64;
            }
        }
        stats.files += 1;
        stats.bytes += r.size;
        Ok(())
    }

    /// The El Torito boot image (1-based entry), as WinImage's "extract boot image".
    pub(crate) fn boot_image(&self, index: usize) -> Result<Vec<u8>> {
        let e = self
            .info
            .boot_entries
            .iter()
            .find(|e| e.index == index)
            .ok_or_else(|| Error::NotFound(format!("El Torito boot entry {index}")))?;
        let mut data = vec![0u8; e.image_size as usize];
        self.w
            .borrow_mut()
            .read_at(u64::from(e.lba) * SECTOR, &mut data)?;
        Ok(data)
    }
}

impl IsoFs {
    /// Write a file's contents to `out` (for checksums and "open with").
    pub(crate) fn copy_out(&self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let path = norm(path)?;
        let r = self
            .find(&path)?
            .ok_or_else(|| Error::NotFound(display(&path)))?;
        if r.is_dir {
            return Err(Error::Invalid(format!("{} is a folder", display(&path))));
        }
        let mut w = self.w.borrow_mut();
        let mut buf = vec![0u8; 1024 * 1024];
        for &(lba, len) in &r.extents {
            let mut done = 0u64;
            while done < u64::from(len) {
                let n = (u64::from(len) - done).min(buf.len() as u64) as usize;
                w.read_at(u64::from(lba) * SECTOR + done, &mut buf[..n])?;
                out.write_all(&buf[..n])?;
                done += n as u64;
            }
        }
        Ok(r.size)
    }
}

/// Byte positions of every directory record (in the primary and Joliet trees) whose extent
/// starts at `lba`.
fn records_for_extent(w: &mut Window, lba: u32) -> Result<Vec<u64>> {
    let mut roots = Vec::new();
    for i in 16..16 + 64u64 {
        let mut d = vec![0u8; SECTOR as usize];
        w.read_at(i * SECTOR, &mut d)?;
        if &d[1..6] != b"CD001" || d[0] == 255 {
            break;
        }
        if matches!(d[0], 1 | 2) {
            roots.push((le32(&d, 156 + 2), le32(&d, 156 + 10)));
        }
    }
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut queue: Vec<(u32, u32)> = roots;
    while let Some((dir_lba, dir_len)) = queue.pop() {
        if !seen.insert(dir_lba) {
            continue;
        }
        let mut data = vec![0u8; dir_len.min(16 * 1024 * 1024) as usize];
        w.read_at(u64::from(dir_lba) * SECTOR, &mut data)?;
        let mut pos = 0usize;
        while pos < data.len() {
            let len = data[pos] as usize;
            if len == 0 {
                pos = (pos / SECTOR as usize + 1) * SECTOR as usize;
                continue;
            }
            if len < 34 || pos + len > data.len() {
                break;
            }
            let rec = &data[pos..pos + len];
            let dot = rec[32] == 1 && (rec[33] == 0 || rec[33] == 1);
            if !dot {
                if rec[25] & 2 != 0 {
                    queue.push((le32(rec, 2), le32(rec, 10)));
                } else if le32(rec, 2) == lba {
                    found.push(u64::from(dir_lba) * SECTOR + pos as u64);
                }
            }
            pos += len;
        }
    }
    Ok(found)
}

/// Overwrite a file's data in place and update its size in every directory record that
/// points at it.
pub(crate) fn replace_file(mut w: Window, path: &str, data: &[u8]) -> Result<()> {
    let fs = IsoFs::open(w.reopen())?;
    let r = fs
        .find(path)?
        .ok_or_else(|| Error::NotFound(display(path)))?;
    drop(fs);
    if r.is_dir {
        return Err(Error::Invalid(format!("{} is a folder", display(path))));
    }
    if r.extents.len() != 1 {
        return Err(Error::Unsupported(
            "replacing multi-extent (over 4 GB) files".into(),
        ));
    }
    let (lba, old_len) = r.extents[0];
    let room = round_up(u64::from(old_len), SECTOR);
    if room == 0 {
        return Err(Error::Unsupported(
            "replacing an empty file (it has no sectors to write into)".into(),
        ));
    }
    if data.len() as u64 > room {
        return Err(Error::Invalid(format!(
            "the new file is {} bytes; at most {room} fit in place of {}",
            data.len(),
            display(path)
        )));
    }
    for i in 16..16 + 64u64 {
        let mut d = vec![0u8; 8];
        w.read_at(i * SECTOR, &mut d)?;
        if matches!(&d[1..6], b"NSR02" | b"NSR03") {
            return Err(Error::Unsupported(
                "editing ISO images that also carry a UDF file system".into(),
            ));
        }
    }
    let positions = records_for_extent(&mut w, lba)?;
    if positions.is_empty() {
        return Err(Error::Invalid(
            "the file's directory record was not found".into(),
        ));
    }
    let mut block = data.to_vec();
    block.resize(room as usize, 0);
    w.write_at(u64::from(lba) * SECTOR, &block)?;
    let len = data.len() as u32;
    for pos in positions {
        let mut head = [0u8; 18];
        w.read_at(pos, &mut head)?;
        put_le32(&mut head, 10, len);
        put_be32(&mut head, 14, len);
        w.write_at(pos, &head)?;
    }
    Ok(())
}

fn parse_record(rec: &[u8], joliet: bool, _rr: bool) -> Option<Record> {
    let name_len = rec[32] as usize;
    let raw = rec.get(33..33 + name_len)?;
    let flags = rec[25];
    let is_dir = flags & 2 != 0;
    let mut name = if joliet {
        ucs2(raw)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    let raw_name = name.clone();
    if !is_dir {
        if let Some(i) = name.rfind(';') {
            name.truncate(i);
        }
        if name.ends_with('.') {
            name.pop();
        }
    }
    Some(Record {
        name,
        raw_name,
        extents: vec![(le32(rec, 2), le32(rec, 10))],
        size: u64::from(le32(rec, 10)),
        is_dir,
        hidden: flags & 1 != 0,
        date: record_date(&rec[18..25]),
    })
}

/// Rock Ridge `RE`: a directory relocated elsewhere (shown at its original place instead).
fn rr_relocated(rec: &[u8]) -> bool {
    let name_len = rec[32] as usize;
    let mut i = 33 + name_len + (1 - name_len % 2);
    while i + 4 <= rec.len() {
        let len = rec[i + 2] as usize;
        if len < 4 {
            break;
        }
        if &rec[i..i + 2] == b"RE" {
            return true;
        }
        i += len;
    }
    false
}

fn platform(id: u8) -> String {
    match id {
        0 => "x86 BIOS".into(),
        1 => "PowerPC".into(),
        2 => "Mac".into(),
        0xEF => "UEFI".into(),
        x => format!("0x{x:02X}"),
    }
}

fn parse_catalog(w: &mut Window, lba: u32) -> Result<Vec<BootEntry>> {
    let mut c = vec![0u8; SECTOR as usize];
    w.read_at(u64::from(lba) * SECTOR, &mut c)?;
    if c[0] != 1 || c[30] != 0x55 || c[31] != 0xAA {
        return Err(Error::Invalid(
            "El Torito validation entry is invalid".into(),
        ));
    }
    let sum = (0..16).fold(0u16, |s, i| s.wrapping_add(le16(&c, i * 2)));
    if sum != 0 {
        return Err(Error::Invalid(
            "El Torito validation checksum mismatch".into(),
        ));
    }
    let mut out = Vec::new();
    let mut plat = c[1];
    let push = |w: &mut Window, e: &[u8], plat: u8, out: &mut Vec<BootEntry>| -> Result<()> {
        let media_type = e[1] & 0x0F;
        let (media, mut size) = match media_type {
            0 => ("no emulation", u64::from(le16(e, 6)) * 512),
            1 => ("1.2 MB floppy", 1_228_800),
            2 => ("1.44 MB floppy", 1_474_560),
            3 => ("2.88 MB floppy", 2_949_120),
            4 => ("hard disk", 0),
            _ => ("unknown", u64::from(le16(e, 6)) * 512),
        };
        let lba = le32(e, 8);
        if media_type == 4 || (media_type == 0 && size <= 512) {
            // Size from the embedded MBR or FAT boot sector.
            let mut s = [0u8; 512];
            w.read_at(u64::from(lba) * SECTOR, &mut s)?;
            if crate::partition::looks_like_fat_boot_sector(&s) {
                if let Ok(b) = crate::fatraw::Bpb::parse(&s) {
                    size = b.total_sectors * b.bps;
                }
            } else if s[510] == 0x55 && s[511] == 0xAA {
                size = (0..4)
                    .map(|i| {
                        let p = &s[446 + i * 16..462 + i * 16];
                        (u64::from(le32(p, 8)) + u64::from(le32(p, 12))) * 512
                    })
                    .max()
                    .unwrap_or(0);
            }
            if size == 0 {
                size = u64::from(le16(e, 6)).max(1) * 512;
            }
        }
        let image_size = size.min(w.len().saturating_sub(u64::from(lba) * SECTOR));
        out.push(BootEntry {
            index: out.len() + 1,
            platform: platform(plat),
            bootable: e[0] == 0x88,
            media: media.into(),
            load_segment: le16(e, 2),
            sector_count: le16(e, 6),
            lba,
            image_size,
        });
        Ok(())
    };
    push(w, &c[32..64], plat, &mut out)?;
    let mut pos = 64;
    while pos + 32 <= c.len() {
        let h = c[pos];
        if h != 0x90 && h != 0x91 {
            break;
        }
        plat = c[pos + 1];
        let n = le16(&c, pos + 2) as usize;
        pos += 32;
        for _ in 0..n {
            if pos + 32 > c.len() {
                break;
            }
            if c[pos] == 0x44 {
                pos += 32; // extension entry
                continue;
            }
            push(w, &c[pos..pos + 32], plat, &mut out)?;
            pos += 32;
        }
        if h == 0x91 {
            break;
        }
    }
    Ok(out)
}
