//! exFAT, written from Microsoft's published exFAT file system specification.
//!
//! Layout: a 12-sector main boot region (boot sector, 8 extended boot sectors, OEM
//! parameters, a reserved sector and a checksum sector) and its backup, the FAT, and the
//! cluster heap. The heap holds the allocation bitmap (which clusters are in use), the
//! up-case table (for case-insensitive names) and the root folder. Folders are arrays of
//! 32-byte entries; a file is an entry set: a file entry (attributes, times), a stream
//! extension (size, first cluster, whether the clusters are contiguous so the FAT is not
//! used) and name entries of 15 UTF-16 units each, protected by a 16-bit checksum.
//!
//! Supported: reading, and writing to volumes with one FAT that were cleanly unmounted
//! (inject, extract, folders, delete, rename/move, label, attributes and times), and
//! formatting. Files are stored contiguously (no FAT chain) when there is room, otherwise
//! through the FAT.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};

use crate::error::{Error, Result, display};
use crate::fatraw::EntryEdit;
use crate::util::{le16, le32, le64, put_le16, put_le32, put_le64};
use crate::volume::{Attributes, Entry, Stats, host_safe_name, join, split_parent};
use crate::window::Window;

const ENTRY: usize = 32;
const EOC: u32 = 0xFFFF_FFFF;
const BAD: u32 = 0xFFFF_FFF7;

const T_END: u8 = 0x00;
const T_BITMAP: u8 = 0x81;
const T_UPCASE: u8 = 0x82;
const T_LABEL: u8 = 0x83;
const T_FILE: u8 = 0x85;
const T_STREAM: u8 = 0xC0;
const T_NAME: u8 = 0xC1;
/// A volume label entry that is not in use (some formatters write it instead of 0x83).
const T_LABEL_UNUSED: u8 = 0x03;

const A_READ_ONLY: u16 = 0x01;
const A_HIDDEN: u16 = 0x02;
const A_SYSTEM: u16 = 0x04;
const A_DIR: u16 = 0x10;
const A_ARCHIVE: u16 = 0x20;

const F_ALLOC_POSSIBLE: u8 = 0x01;
const F_NO_FAT_CHAIN: u8 = 0x02;

const FLAG_ACTIVE_FAT: u16 = 0x01;
const FLAG_DIRTY: u16 = 0x02;
const FLAG_MEDIA_FAILURE: u16 = 0x04;

// --- checksums and names ------------------------------------------------------------------

/// The boot region checksum: sectors 0–10, skipping VolumeFlags and PercentInUse.
fn boot_checksum(region: &[u8], bps: usize) -> u32 {
    let mut c: u32 = 0;
    for (i, &b) in region[..11 * bps].iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        c = c.rotate_right(1).wrapping_add(u32::from(b));
    }
    c
}

fn table_checksum(data: &[u8]) -> u32 {
    data.iter()
        .fold(0u32, |c, &b| c.rotate_right(1).wrapping_add(u32::from(b)))
}

/// Entry set checksum: every byte of every entry except the checksum field itself.
fn set_checksum(set: &[u8]) -> u16 {
    let mut c: u16 = 0;
    for (i, &b) in set.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        c = c.rotate_right(1).wrapping_add(u16::from(b));
    }
    c
}

/// Up-case table: one u16 per UTF-16 code unit.
struct Upcase(Vec<u16>);

impl Upcase {
    /// The table written by `format`: simple one-to-one upper-casing of the Basic
    /// Multilingual Plane, from Unicode's case data.
    fn generated() -> Upcase {
        Upcase(
            (0..=0xFFFFu32)
                .map(|i| {
                    let Some(c) = char::from_u32(i) else {
                        return i as u16;
                    };
                    let mut up = c.to_uppercase();
                    match (up.next(), up.next()) {
                        (Some(u), None) if (u as u32) <= 0xFFFF => u as u16,
                        _ => i as u16,
                    }
                })
                .collect(),
        )
    }

    /// Decode a stored table; runs of identity mappings are stored as 0xFFFF, count.
    fn decode(data: &[u8]) -> Upcase {
        let mut map: Vec<u16> = (0..=0xFFFFu32).map(|i| i as u16).collect();
        let words: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let mut idx = 0usize;
        let mut k = 0;
        while k < words.len() && idx < map.len() {
            let v = words[k];
            if v == 0xFFFF && k + 1 < words.len() {
                idx += usize::from(words[k + 1]);
                k += 2;
                continue;
            }
            map[idx] = v;
            idx += 1;
            k += 1;
        }
        Upcase(map)
    }

    /// Store the table, compressing identity runs.
    fn encode(&self) -> Vec<u8> {
        let mut words: Vec<u16> = Vec::new();
        let mut i = 0usize;
        let n = self.0.len();
        while i < n {
            if usize::from(self.0[i]) == i {
                let mut j = i;
                while j < n && usize::from(self.0[j]) == j && j - i < 0xFFFF {
                    j += 1;
                }
                if j - i >= 3 {
                    words.push(0xFFFF);
                    words.push((j - i) as u16);
                    i = j;
                    continue;
                }
            }
            words.push(self.0[i]);
            i += 1;
        }
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn up(&self, c: u16) -> u16 {
        self.0[usize::from(c)]
    }

    fn eq(&self, a: &[u16], b: &[u16]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| self.up(x) == self.up(y))
    }

    fn hash(&self, name: &[u16]) -> u16 {
        let mut h: u16 = 0;
        for &c in name {
            for b in self.up(c).to_le_bytes() {
                h = h.rotate_right(1).wrapping_add(u16::from(b));
            }
        }
        h
    }
}

fn check_name(name: &str) -> Result<Vec<u16>> {
    let u: Vec<u16> = name.encode_utf16().collect();
    let bad = u.is_empty()
        || u.len() > 255
        || name == "."
        || name == ".."
        || u.iter().any(|&c| {
            c < 0x20
                || matches!(
                    c,
                    0x22 | 0x2A | 0x2F | 0x3A | 0x3C | 0x3E | 0x3F | 0x5C | 0x7C
                )
        });
    if bad {
        return Err(Error::InvalidPath(format!(
            "{name:?} is not a valid exFAT file name (1–255 characters, none of \" * / : < > ? \\ |)"
        )));
    }
    Ok(u)
}

// --- times -------------------------------------------------------------------------------

/// A timestamp field: DOS-style date and time (2-second units) in the writer's local time,
/// 10 ms units on top, and the writer's UTC offset in 15-minute units (bit 7: valid).
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Stamp {
    ts: u32,
    ms10: u8,
    utc: u8,
}

impl Stamp {
    fn from_time(t: SystemTime) -> Result<Stamp> {
        let (secs, nanos) = match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => (-(e.duration().as_secs() as i64), 0),
        };
        let local = Local
            .timestamp_opt(secs, nanos)
            .single()
            .ok_or_else(|| Error::Invalid("invalid time".into()))?;
        if !(1980..=2107).contains(&local.year()) {
            return Err(Error::Invalid(format!(
                "exFAT stores dates from 1980 to 2107 (not {})",
                local.year()
            )));
        }
        let offset_q = local.offset().local_minus_utc() / 900;
        let ts = ((local.year() as u32 - 1980) << 25)
            | (local.month() << 21)
            | (local.day() << 16)
            | (local.hour() << 11)
            | (local.minute() << 5)
            | (local.second() / 2);
        let ms10 = (local.second() % 2) * 100 + (local.nanosecond() % 1_000_000_000) / 10_000_000;
        Ok(Stamp {
            ts,
            ms10: ms10 as u8,
            utc: 0x80 | ((offset_q as i8 as u8) & 0x7F),
        })
    }

    /// For host times outside exFAT's range: clamp instead of failing.
    fn from_time_clamped(t: SystemTime) -> Stamp {
        Stamp::from_time(t).unwrap_or_else(|_| {
            let early = t < UNIX_EPOCH + std::time::Duration::from_secs(315_532_800 + 86_400);
            Stamp {
                ts: if early {
                    (1 << 21) | (1 << 16)
                } else {
                    (127 << 25) | (12 << 21) | (31 << 16) | (23 << 11) | (59 << 5) | 29
                },
                ms10: 0,
                utc: 0,
            }
        })
    }

    fn naive(self) -> Option<NaiveDateTime> {
        let t = self.ts;
        let sec = (t & 0x1F) * 2 + u32::from(self.ms10) / 100;
        NaiveDate::from_ymd_opt(1980 + (t >> 25) as i32, (t >> 21) & 0xF, (t >> 16) & 0x1F)?
            .and_hms_milli_opt(
                (t >> 11) & 0x1F,
                (t >> 5) & 0x3F,
                sec.min(59),
                (u32::from(self.ms10) % 100) * 10,
            )
    }

    fn to_local(self) -> Option<chrono::DateTime<Local>> {
        let naive = self.naive()?;
        if self.utc & 0x80 != 0 {
            let q = i64::from(((self.utc << 1) as i8) >> 1);
            let utc = naive - chrono::Duration::minutes(q * 15);
            Some(Local.from_utc_datetime(&utc))
        } else {
            Local.from_local_datetime(&naive).earliest()
        }
    }

    fn display(self) -> String {
        self.to_local()
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default()
    }

    fn system_time(self) -> Option<SystemTime> {
        let d = self.to_local()?;
        let secs = d.timestamp();
        Some(if secs >= 0 {
            UNIX_EPOCH + std::time::Duration::new(secs as u64, d.timestamp_subsec_nanos())
        } else {
            UNIX_EPOCH - std::time::Duration::from_secs(secs.unsigned_abs())
        })
    }
}

// --- on-disk structures --------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Boot {
    bps: u64,
    cluster: u64,
    fat_offset: u64,
    fat_length: u64,
    heap_offset: u64,
    clusters: u32,
    root: u32,
    serial: u32,
    revision: u16,
    flags: u16,
    fats: u8,
    volume_sectors: u64,
}

impl Boot {
    fn parse(s: &[u8], volume_bytes: u64) -> Result<Boot> {
        let bad = |what: &str| Error::Invalid(format!("not a valid exFAT volume: {what}"));
        if &s[3..11] != b"EXFAT   " {
            return Err(bad("no EXFAT signature"));
        }
        if le16(s, 510) != 0xAA55 {
            return Err(bad("no boot signature"));
        }
        if s[11..64].iter().any(|&b| b != 0) {
            return Err(bad("the BIOS parameter block area is not empty"));
        }
        let bps_shift = u32::from(s[108]);
        let spc_shift = u32::from(s[109]);
        if !(9..=12).contains(&bps_shift) || bps_shift + spc_shift > 25 {
            return Err(bad("sector or cluster size out of range"));
        }
        let b = Boot {
            bps: 1 << bps_shift,
            cluster: 1 << (bps_shift + spc_shift),
            fat_offset: u64::from(le32(s, 80)),
            fat_length: u64::from(le32(s, 84)),
            heap_offset: u64::from(le32(s, 88)),
            clusters: le32(s, 92),
            root: le32(s, 96),
            serial: le32(s, 100),
            revision: le16(s, 104),
            flags: le16(s, 106),
            fats: s[110],
            volume_sectors: le64(s, 72),
        };
        if !(1..=2).contains(&b.fats) {
            return Err(bad("number of FATs"));
        }
        if b.fat_offset < 24
            || b.fat_length * b.bps < (u64::from(b.clusters) + 2) * 4
            || b.heap_offset < b.fat_offset + b.fat_length * u64::from(b.fats)
        {
            return Err(bad("FAT or cluster heap position"));
        }
        if b.clusters == 0
            || (b.heap_offset * b.bps + u64::from(b.clusters) * b.cluster)
                > volume_bytes.max(b.volume_sectors * b.bps)
        {
            return Err(bad("cluster count larger than the volume"));
        }
        if !b.valid_cluster(b.root) {
            return Err(bad("root folder cluster"));
        }
        Ok(b)
    }

    fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && u64::from(c) < u64::from(self.clusters) + 2
    }

    fn cluster_pos(&self, c: u32) -> u64 {
        self.heap_offset * self.bps + u64::from(c - 2) * self.cluster
    }

    fn fat_bytes(&self) -> u64 {
        (u64::from(self.clusters) + 2) * 4
    }
}

/// Where a run of clusters is: contiguous from `first`, or chained through the FAT.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Alloc {
    first: u32,
    contiguous: bool,
}

/// A folder: its clusters, and where its own entry set is (none for the root).
#[derive(Clone, Debug)]
struct DirRef {
    alloc: Alloc,
    /// Allocated size in bytes (None for the root, which is followed through the FAT).
    size: Option<u64>,
    /// Parent folder path and the slot of this folder's file entry in it.
    owner: Option<(String, usize)>,
}

/// One file or folder (an entry set) in a folder.
#[derive(Clone, Debug)]
struct Item {
    slot: usize,
    count: usize,
    name: String,
    attrs: u16,
    created: Stamp,
    modified: Stamp,
    accessed: Stamp,
    alloc: Alloc,
    len: u64,
    valid: u64,
}

impl Item {
    fn is_dir(&self) -> bool {
        self.attrs & A_DIR != 0
    }
}

/// A folder's entries read into memory, with the clusters they came from.
struct DirData {
    clusters: Vec<u32>,
    data: Vec<u8>,
}

impl DirData {
    fn slots(&self) -> usize {
        self.data.len() / ENTRY
    }
    fn kind(&self, slot: usize) -> u8 {
        self.data[slot * ENTRY]
    }
    fn entry(&self, slot: usize) -> &[u8] {
        &self.data[slot * ENTRY..(slot + 1) * ENTRY]
    }
}

struct SetData {
    name: Vec<u16>,
    attrs: u16,
    created: Stamp,
    modified: Stamp,
    accessed: Stamp,
    alloc: Alloc,
    len: u64,
    valid: u64,
}

fn build_set(s: &SetData, up: &Upcase) -> Vec<u8> {
    let names = s.name.len().div_ceil(15);
    let mut set = vec![0u8; (2 + names) * ENTRY];
    set[0] = T_FILE;
    set[1] = (1 + names) as u8;
    put_le16(&mut set, 4, s.attrs);
    put_le32(&mut set, 8, s.created.ts);
    put_le32(&mut set, 12, s.modified.ts);
    put_le32(&mut set, 16, s.accessed.ts);
    set[20] = s.created.ms10;
    set[21] = s.modified.ms10;
    set[22] = s.created.utc;
    set[23] = s.modified.utc;
    set[24] = s.accessed.utc;
    let st = &mut set[ENTRY..2 * ENTRY];
    st[0] = T_STREAM;
    st[1] = F_ALLOC_POSSIBLE
        | if s.alloc.contiguous && s.alloc.first != 0 {
            F_NO_FAT_CHAIN
        } else {
            0
        };
    st[3] = s.name.len() as u8;
    put_le16(st, 4, up.hash(&s.name));
    put_le64(st, 8, s.valid);
    put_le32(st, 20, s.alloc.first);
    put_le64(st, 24, s.len);
    for (k, chunk) in s.name.chunks(15).enumerate() {
        let e = &mut set[(2 + k) * ENTRY..(3 + k) * ENTRY];
        e[0] = T_NAME;
        for (j, &c) in chunk.iter().enumerate() {
            put_le16(e, 2 + 2 * j, c);
        }
    }
    let sum = set_checksum(&set);
    put_le16(&mut set, 2, sum);
    set
}

fn item_entry(it: &Item, dir: &str) -> Entry {
    Entry {
        name: it.name.clone(),
        short_name: String::new(),
        path: join(dir, &it.name),
        is_dir: it.is_dir(),
        size: if it.is_dir() { 0 } else { it.len },
        modified: it.modified.display(),
        created: it.created.display(),
        accessed: it.accessed.display(),
        attributes: Attributes {
            read_only: it.attrs & A_READ_ONLY != 0,
            hidden: it.attrs & A_HIDDEN != 0,
            system: it.attrs & A_SYSTEM != 0,
            archive: it.attrs & A_ARCHIVE != 0,
        },
    }
}

fn no_space() -> Error {
    Error::Invalid("no free clusters left in the exFAT file system".into())
}

// --- the mounted file system ------------------------------------------------------------

struct Inner {
    w: Window,
    b: Boot,
    up: Upcase,
    bitmap: Vec<u8>,
    bitmap_alloc: Alloc,
    bitmap_dirty: Option<(usize, usize)>,
    label: String,
    fat: HashMap<u64, Vec<u8>>,
    fat_dirty: BTreeSet<u64>,
    /// Set once the dirty flag is on disk; cleared again by `flush`.
    marked_dirty: bool,
    blocker: Option<String>,
    /// Where the next allocation search starts.
    hint: u32,
}

const FAT_BLOCK: u64 = 64 * 1024;

impl Inner {
    fn mount(mut w: Window, writable: bool) -> Result<Inner> {
        let mut s = vec![0u8; 512];
        w.read_at(0, &mut s)?;
        let b = Boot::parse(&s, w.len())?;
        let bps = b.bps as usize;
        let mut region = vec![0u8; 12 * bps];
        w.read_at(0, &mut region)?;
        let sum = boot_checksum(&region, bps);
        if region[11 * bps..]
            .as_chunks::<4>()
            .0
            .iter()
            .any(|c| u32::from_le_bytes(*c) != sum)
        {
            return Err(Error::Invalid(
                "the exFAT boot region checksum does not match (damaged boot sector)".into(),
            ));
        }
        let mut fs = Inner {
            w,
            b,
            up: Upcase(Vec::new()),
            bitmap: Vec::new(),
            bitmap_alloc: Alloc {
                first: 0,
                contiguous: false,
            },
            bitmap_dirty: None,
            label: String::new(),
            fat: HashMap::new(),
            fat_dirty: BTreeSet::new(),
            marked_dirty: false,
            blocker: None,
            hint: 2,
        };
        let root = fs.read_dir(&fs.root())?;
        let active = usize::from(fs.b.flags & FLAG_ACTIVE_FAT != 0);
        let (mut bitmap, mut upcase) = (None, None);
        let mut bitmaps = 0;
        for slot in 0..root.slots() {
            let e = root.entry(slot);
            match e[0] {
                T_END => break,
                T_BITMAP => {
                    if bitmaps == active || fs.b.fats == 1 {
                        bitmap = Some((le32(e, 20), le64(e, 24)));
                    }
                    bitmaps += 1;
                }
                T_UPCASE => upcase = Some((le32(e, 20), le64(e, 24), le32(e, 4))),
                T_LABEL => {
                    let n = usize::from(e[1]).min(11);
                    let u: Vec<u16> = (0..n).map(|k| le16(e, 2 + 2 * k)).collect();
                    fs.label = String::from_utf16_lossy(&u);
                }
                _ => {}
            }
        }
        let (bfirst, blen) = bitmap
            .ok_or_else(|| Error::Invalid("exFAT volume without an allocation bitmap".into()))?;
        if blen < u64::from(fs.b.clusters).div_ceil(8) {
            return Err(Error::Invalid(
                "exFAT allocation bitmap is too small".into(),
            ));
        }
        fs.bitmap_alloc = Alloc {
            first: bfirst,
            contiguous: false,
        };
        fs.bitmap = fs.read_chain(fs.bitmap_alloc, Some(blen), blen)?;
        let (ufirst, ulen, usum) =
            upcase.ok_or_else(|| Error::Invalid("exFAT volume without an up-case table".into()))?;
        let table = fs.read_chain(
            Alloc {
                first: ufirst,
                contiguous: false,
            },
            Some(ulen),
            ulen,
        )?;
        if table_checksum(&table) != usum {
            return Err(Error::Invalid(
                "the exFAT up-case table checksum does not match".into(),
            ));
        }
        fs.up = Upcase::decode(&table);
        fs.blocker = if fs.b.revision >> 8 != 1 {
            Some(format!(
                "exFAT revision {}.{} is not supported for writing",
                fs.b.revision >> 8,
                fs.b.revision & 0xFF
            ))
        } else if fs.b.fats != 1 {
            Some("exFAT volumes with two FATs (TexFAT) open read-only".into())
        } else if fs.b.flags & FLAG_MEDIA_FAILURE != 0 {
            Some("the volume is marked as having media failures".into())
        } else if fs.b.flags & FLAG_DIRTY != 0 {
            Some(
                "the volume was not cleanly unmounted; check it with chkdsk or fsck.exfat first"
                    .into(),
            )
        } else {
            None
        };
        let _ = writable;
        Ok(fs)
    }

    fn root(&self) -> DirRef {
        DirRef {
            alloc: Alloc {
                first: self.b.root,
                contiguous: false,
            },
            size: None,
            owner: None,
        }
    }

    // --- FAT and bitmap --------------------------------------------------------------

    fn fat_block(&mut self, block: u64) -> Result<&mut Vec<u8>> {
        if !self.fat.contains_key(&block) {
            let start = block * FAT_BLOCK;
            let len = FAT_BLOCK.min(self.b.fat_bytes() - start) as usize;
            let mut buf = vec![0u8; len];
            let at = self.b.fat_offset * self.b.bps + start;
            self.w.read_at(at, &mut buf)?;
            self.fat.insert(block, buf);
        }
        Ok(self.fat.get_mut(&block).expect("just inserted"))
    }

    fn fat_get(&mut self, c: u32) -> Result<u32> {
        let at = u64::from(c) * 4;
        let off = (at % FAT_BLOCK) as usize;
        Ok(le32(self.fat_block(at / FAT_BLOCK)?, off))
    }

    fn fat_set(&mut self, c: u32, v: u32) -> Result<()> {
        let at = u64::from(c) * 4;
        let off = (at % FAT_BLOCK) as usize;
        let block = at / FAT_BLOCK;
        put_le32(self.fat_block(block)?, off, v);
        self.fat_dirty.insert(block);
        Ok(())
    }

    fn used(&self, c: u32) -> bool {
        let i = (c - 2) as usize;
        self.bitmap[i / 8] & (1 << (i % 8)) != 0
    }

    fn set_used(&mut self, c: u32, on: bool) {
        let i = (c - 2) as usize;
        if on {
            self.bitmap[i / 8] |= 1 << (i % 8);
        } else {
            self.bitmap[i / 8] &= !(1 << (i % 8));
        }
        let byte = i / 8;
        self.bitmap_dirty = Some(match self.bitmap_dirty {
            Some((a, b)) => (a.min(byte), b.max(byte + 1)),
            None => (byte, byte + 1),
        });
    }

    fn free_clusters(&self) -> u64 {
        (2..self.b.clusters + 2).filter(|&c| !self.used(c)).count() as u64
    }

    /// Clusters of an allocation: `len` bytes when known (contiguous runs need it), else
    /// the whole FAT chain.
    fn clusters(&mut self, a: Alloc, len: Option<u64>) -> Result<Vec<u32>> {
        if a.first == 0 {
            return Ok(Vec::new());
        }
        if !self.b.valid_cluster(a.first) {
            return Err(Error::Invalid(format!(
                "exFAT: cluster {} is outside the volume",
                a.first
            )));
        }
        if a.contiguous {
            let n = len.unwrap_or(self.b.cluster).div_ceil(self.b.cluster) as u32;
            if !self.b.valid_cluster(a.first + n.max(1) - 1) {
                return Err(Error::Invalid(
                    "exFAT: file runs past the volume end".into(),
                ));
            }
            return Ok((a.first..a.first + n).collect());
        }
        let want = len.map(|l| l.div_ceil(self.b.cluster));
        let mut out = vec![a.first];
        let mut c = a.first;
        loop {
            if want.is_some_and(|w| out.len() as u64 >= w) {
                break;
            }
            let next = self.fat_get(c)?;
            if next >= 0xFFFF_FFF8 {
                break;
            }
            if next == BAD || !self.b.valid_cluster(next) || out.len() > self.b.clusters as usize {
                return Err(Error::Invalid(format!(
                    "exFAT: broken cluster chain at cluster {c}"
                )));
            }
            out.push(next);
            c = next;
        }
        Ok(out)
    }

    fn read_chain(&mut self, a: Alloc, len: Option<u64>, bytes: u64) -> Result<Vec<u8>> {
        let clusters = self.clusters(a, len)?;
        let mut out = vec![0u8; (clusters.len() as u64 * self.b.cluster) as usize];
        let cs = self.b.cluster as usize;
        for (run_start, run) in runs(&clusters) {
            let at = self.b.cluster_pos(clusters[run_start]);
            self.w
                .read_at(at, &mut out[run_start * cs..(run_start + run) * cs])?;
        }
        out.truncate(bytes.min(out.len() as u64) as usize);
        Ok(out)
    }

    /// Find `n` free clusters: a contiguous run when there is one, else any free ones.
    fn allocate(&mut self, n: u64) -> Result<(Vec<u32>, bool)> {
        if n == 0 {
            return Ok((Vec::new(), true));
        }
        let total = self.b.clusters;
        let start = if self.b.valid_cluster(self.hint) {
            self.hint
        } else {
            2
        };
        let order = (start..total + 2).chain(2..start);
        let mut run_start = 0u32;
        let mut run = 0u64;
        let mut found = None;
        let mut prev = 0u32;
        for c in order {
            if self.used(c) || (run > 0 && c != prev + 1) {
                run = 0;
            }
            if !self.used(c) {
                if run == 0 {
                    run_start = c;
                }
                run += 1;
                if run == n {
                    found = Some(run_start);
                    break;
                }
            }
            prev = c;
        }
        let (list, contiguous) = match found {
            Some(s) => ((s..s + n as u32).collect::<Vec<u32>>(), true),
            None => {
                let free: Vec<u32> = (2..total + 2)
                    .filter(|&c| !self.used(c))
                    .take(n as usize)
                    .collect();
                if (free.len() as u64) < n {
                    return Err(no_space());
                }
                (free, false)
            }
        };
        for &c in &list {
            self.set_used(c, true);
        }
        self.hint = list.last().map_or(2, |&c| c + 1);
        Ok((list, contiguous))
    }

    fn link(&mut self, clusters: &[u32]) -> Result<()> {
        for (k, &c) in clusters.iter().enumerate() {
            let next = clusters.get(k + 1).copied().unwrap_or(EOC);
            self.fat_set(c, next)?;
        }
        Ok(())
    }

    fn release(&mut self, a: Alloc, len: Option<u64>) -> Result<()> {
        let clusters = self.clusters(a, len)?;
        for &c in &clusters {
            if self.b.valid_cluster(c) {
                self.set_used(c, false);
                if !a.contiguous {
                    self.fat_set(c, 0)?;
                }
            }
        }
        Ok(())
    }

    /// Before the first change: mark the volume dirty on disk (cleared by `flush`).
    fn begin_write(&mut self) -> Result<()> {
        if let Some(why) = &self.blocker {
            return Err(Error::Unsupported(format!("writing to this volume: {why}")));
        }
        if !self.marked_dirty {
            let mut f = [0u8; 2];
            put_le16(&mut f, 0, self.b.flags | FLAG_DIRTY);
            self.w.write_at(106, &f)?;
            self.marked_dirty = true;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.marked_dirty {
            return Ok(());
        }
        let blocks: Vec<u64> = std::mem::take(&mut self.fat_dirty).into_iter().collect();
        for block in blocks {
            let at = self.b.fat_offset * self.b.bps + block * FAT_BLOCK;
            let data = self.fat[&block].clone();
            self.w.write_at(at, &data)?;
        }
        if let Some((a, b)) = self.bitmap_dirty.take() {
            let clusters = self.clusters(self.bitmap_alloc, Some(self.bitmap.len() as u64))?;
            let cs = self.b.cluster as usize;
            let mut k = a / cs * cs;
            while k < b {
                let ci = k / cs;
                let end = ((ci + 1) * cs).min(self.bitmap.len());
                let at = self.b.cluster_pos(clusters[ci]);
                let data = self.bitmap[ci * cs..end].to_vec();
                self.w.write_at(at, &data)?;
                k = (ci + 1) * cs;
            }
        }
        let used = u64::from(self.b.clusters) - self.free_clusters();
        let percent = (used * 100 / u64::from(self.b.clusters)) as u8;
        let mut f = [0u8; 2];
        put_le16(&mut f, 0, self.b.flags & !FLAG_DIRTY);
        self.w.write_at(106, &f)?;
        self.w.write_at(112, &[percent])?;
        self.marked_dirty = false;
        Ok(())
    }

    // --- folders ----------------------------------------------------------------------

    fn read_dir(&mut self, d: &DirRef) -> Result<DirData> {
        let clusters = self.clusters(d.alloc, d.size)?;
        let bytes = clusters.len() as u64 * self.b.cluster;
        let data = self.read_chain(d.alloc, d.size, bytes)?;
        Ok(DirData { clusters, data })
    }

    fn items(&self, dd: &DirData) -> Vec<Item> {
        let mut out = Vec::new();
        let mut slot = 0;
        let n = dd.slots();
        while slot < n {
            let t = dd.kind(slot);
            if t == T_END {
                break;
            }
            if t != T_FILE {
                slot += 1;
                continue;
            }
            let count = usize::from(dd.entry(slot)[1]) + 1;
            if count < 3 || slot + count > n {
                slot += 1;
                continue;
            }
            let set = &dd.data[slot * ENTRY..(slot + count) * ENTRY];
            if set_checksum(set) != le16(set, 2) || set[ENTRY] != T_STREAM {
                slot += 1;
                continue;
            }
            let f = &set[..ENTRY];
            let st = &set[ENTRY..2 * ENTRY];
            let name_len = usize::from(st[3]);
            let mut name = Vec::with_capacity(name_len);
            for k in 2..count {
                let e = &set[k * ENTRY..(k + 1) * ENTRY];
                if e[0] != T_NAME {
                    break;
                }
                for j in 0..15 {
                    if name.len() < name_len {
                        name.push(le16(e, 2 + 2 * j));
                    }
                }
            }
            if name.len() != name_len || name_len == 0 {
                slot += 1;
                continue;
            }
            out.push(Item {
                slot,
                count,
                name: String::from_utf16_lossy(&name),
                attrs: le16(f, 4),
                created: Stamp {
                    ts: le32(f, 8),
                    ms10: f[20],
                    utc: f[22],
                },
                modified: Stamp {
                    ts: le32(f, 12),
                    ms10: f[21],
                    utc: f[23],
                },
                accessed: Stamp {
                    ts: le32(f, 16),
                    ms10: 0,
                    utc: f[24],
                },
                alloc: Alloc {
                    first: le32(st, 20),
                    contiguous: st[1] & F_NO_FAT_CHAIN != 0,
                },
                len: le64(st, 24),
                valid: le64(st, 8),
            });
            slot += count;
        }
        out
    }

    fn find(&self, dd: &DirData, name: &str) -> Option<Item> {
        let want: Vec<u16> = name.encode_utf16().collect();
        self.items(dd).into_iter().find(|it| {
            let have: Vec<u16> = it.name.encode_utf16().collect();
            self.up.eq(&have, &want)
        })
    }

    fn subdir(parent_path: &str, it: &Item) -> DirRef {
        DirRef {
            alloc: it.alloc,
            size: Some(it.len),
            owner: Some((parent_path.to_string(), it.slot)),
        }
    }

    /// The folder at `path`.
    fn dir_at(&mut self, path: &str) -> Result<DirRef> {
        let mut cur = self.root();
        let mut cur_path = String::new();
        if path.is_empty() {
            return Ok(cur);
        }
        for part in path.split('/') {
            let dd = self.read_dir(&cur)?;
            let it = self
                .find(&dd, part)
                .ok_or_else(|| Error::NotFound(display(&join(&cur_path, part))))?;
            if !it.is_dir() {
                return Err(Error::Invalid(format!(
                    "not a folder: {}",
                    display(&join(&cur_path, part))
                )));
            }
            cur = Inner::subdir(&cur_path, &it);
            cur_path = join(&cur_path, part);
        }
        Ok(cur)
    }

    /// The entry at `path` and its folder; `None` when it does not exist.
    fn lookup(&mut self, path: &str) -> Result<Option<(DirRef, Item)>> {
        let (parent, name) = split_parent(path);
        let dir = match self.dir_at(parent) {
            Ok(d) => d,
            Err(Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let dd = self.read_dir(&dir)?;
        Ok(self.find(&dd, name).map(|it| (dir, it)))
    }

    fn write_slots(&mut self, dd: &DirData, from: usize, to: usize) -> Result<()> {
        let cs = self.b.cluster as usize;
        let mut k = from * ENTRY;
        let end = to * ENTRY;
        while k < end {
            let ci = k / cs;
            let stop = end.min((ci + 1) * cs);
            let at = self.b.cluster_pos(dd.clusters[ci]) + (k % cs) as u64;
            let data = dd.data[k..stop].to_vec();
            self.w.write_at(at, &data)?;
            k = stop;
        }
        Ok(())
    }

    /// Put an entry set into a folder, growing the folder when it has no room.
    fn insert_set(&mut self, dir: &mut DirRef, set: &[u8]) -> Result<usize> {
        let n = set.len() / ENTRY;
        loop {
            let mut dd = self.read_dir(dir)?;
            let slots = dd.slots();
            let end = (0..slots).find(|&s| dd.kind(s) == T_END).unwrap_or(slots);
            let mut run = 0;
            let mut found = None;
            for s in 0..slots {
                let free = s >= end || dd.kind(s) & 0x80 == 0;
                run = if free { run + 1 } else { 0 };
                if run == n {
                    found = Some(s + 1 - n);
                    break;
                }
            }
            if let Some(at) = found {
                dd.data[at * ENTRY..(at + n) * ENTRY].copy_from_slice(set);
                let mut last = at + n;
                // Entries past the end marker are unused whatever they hold; once the set
                // reaches into that area, the slot after it must be an end marker.
                if at + n > end && at + n < slots && dd.kind(at + n) != T_END {
                    dd.data[(at + n) * ENTRY..(at + n + 1) * ENTRY].fill(0);
                    last += 1;
                }
                self.write_slots(&dd, at, last)?;
                return Ok(at);
            }
            self.grow_dir(dir, &dd)?;
        }
    }

    /// Add one zeroed cluster to a folder.
    fn grow_dir(&mut self, dir: &mut DirRef, dd: &DirData) -> Result<()> {
        if dd.data.len() as u64 + self.b.cluster > 256 << 20 {
            return Err(Error::Invalid(
                "exFAT folders cannot grow beyond 256 MiB".into(),
            ));
        }
        let last = *dd
            .clusters
            .last()
            .expect("folders have at least one cluster");
        self.hint = last + 1;
        let (new, _) = self.allocate(1)?;
        let c = new[0];
        let zero = vec![0u8; self.b.cluster as usize];
        self.w.write_at(self.b.cluster_pos(c), &zero)?;
        if dir.alloc.contiguous && c == last + 1 {
            // Still one run.
        } else {
            if dir.alloc.contiguous {
                self.link(&dd.clusters)?;
                dir.alloc.contiguous = false;
            }
            self.fat_set(last, c)?;
            self.fat_set(c, EOC)?;
        }
        if let Some(size) = dir.size.as_mut() {
            *size += self.b.cluster;
        }
        if let Some((parent_path, slot)) = dir.owner.clone() {
            let size = dir.size.unwrap_or(0);
            let contiguous = dir.alloc.contiguous;
            let parent = self.dir_at(&parent_path)?;
            let mut pd = self.read_dir(&parent)?;
            let count = usize::from(pd.entry(slot)[1]) + 1;
            {
                let set = &mut pd.data[slot * ENTRY..(slot + count) * ENTRY];
                let st = &mut set[ENTRY..2 * ENTRY];
                st[1] = F_ALLOC_POSSIBLE | if contiguous { F_NO_FAT_CHAIN } else { 0 };
                put_le64(st, 8, size);
                put_le64(st, 24, size);
                let sum = set_checksum(set);
                put_le16(set, 2, sum);
            }
            self.write_slots(&pd, slot, slot + count)?;
        }
        Ok(())
    }

    fn delete_set(&mut self, dir: &DirRef, slot: usize, count: usize) -> Result<()> {
        let mut dd = self.read_dir(dir)?;
        for s in slot..slot + count {
            dd.data[s * ENTRY] &= 0x7F;
        }
        self.write_slots(&dd, slot, slot + count)
    }

    fn rewrite_set(&mut self, dir: &DirRef, it: &Item) -> Result<()> {
        let mut dd = self.read_dir(dir)?;
        let set = build_set(
            &SetData {
                name: it.name.encode_utf16().collect(),
                attrs: it.attrs,
                created: it.created,
                modified: it.modified,
                accessed: it.accessed,
                alloc: it.alloc,
                len: it.len,
                valid: it.valid,
            },
            &self.up,
        );
        if set.len() / ENTRY != it.count {
            return Err(Error::Invalid("exFAT: entry set changed size".into()));
        }
        dd.data[it.slot * ENTRY..(it.slot + it.count) * ENTRY].copy_from_slice(&set);
        self.write_slots(&dd, it.slot, it.slot + it.count)
    }

    // --- operations -------------------------------------------------------------------

    fn mkdir_in(&mut self, dir: &mut DirRef, name: &str, when: SystemTime) -> Result<()> {
        let uname = check_name(name)?;
        let (c, _) = self.allocate(1)?;
        let zero = vec![0u8; self.b.cluster as usize];
        self.w.write_at(self.b.cluster_pos(c[0]), &zero)?;
        let now = Stamp::from_time_clamped(when);
        let set = build_set(
            &SetData {
                name: uname,
                attrs: A_DIR,
                created: now,
                modified: now,
                accessed: now,
                alloc: Alloc {
                    first: c[0],
                    contiguous: true,
                },
                len: self.b.cluster,
                valid: self.b.cluster,
            },
            &self.up,
        );
        if let Err(e) = self.insert_set(dir, &set) {
            self.set_used(c[0], false);
            return Err(e);
        }
        Ok(())
    }

    fn ensure_dir(&mut self, path: &str) -> Result<DirRef> {
        let mut cur = self.root();
        let mut cur_path = String::new();
        if path.is_empty() {
            return Ok(cur);
        }
        for part in path.split('/') {
            let dd = self.read_dir(&cur)?;
            let it = match self.find(&dd, part) {
                Some(it) => it,
                None => {
                    self.mkdir_in(&mut cur, part, SystemTime::now())?;
                    let dd = self.read_dir(&cur)?;
                    self.find(&dd, part).expect("just created")
                }
            };
            if !it.is_dir() {
                return Err(Error::Exists(format!(
                    "{} (a file, not a folder)",
                    display(&join(&cur_path, part))
                )));
            }
            cur = Inner::subdir(&cur_path, &it);
            cur_path = join(&cur_path, part);
        }
        Ok(cur)
    }

    /// Write `len` bytes from `src` into newly allocated clusters.
    fn write_data(&mut self, src: &mut dyn Read, len: u64) -> Result<Alloc> {
        let n = len.div_ceil(self.b.cluster);
        let (clusters, contiguous) = self.allocate(n)?;
        if clusters.is_empty() {
            return Ok(Alloc {
                first: 0,
                contiguous: false,
            });
        }
        if !contiguous {
            self.link(&clusters)?;
        }
        let cs = self.b.cluster;
        let mut left = len;
        let mut buf = Vec::new();
        for (start, run) in runs(&clusters) {
            let mut at = self.b.cluster_pos(clusters[start]);
            let mut run_bytes = run as u64 * cs;
            while run_bytes > 0 {
                let chunk = run_bytes.min(4 << 20);
                buf.resize(chunk as usize, 0);
                let want = chunk.min(left) as usize;
                read_fill(src, &mut buf[..want])?;
                buf[want..].fill(0);
                self.w.write_at(at, &buf)?;
                left -= want as u64;
                at += chunk;
                run_bytes -= chunk;
            }
        }
        Ok(Alloc {
            first: clusters[0],
            contiguous,
        })
    }

    fn read_data(&mut self, it: &Item, out: &mut dyn Write) -> Result<u64> {
        let clusters = self.clusters(it.alloc, Some(it.len))?;
        let cs = self.b.cluster;
        let mut done = 0u64;
        let mut buf = Vec::new();
        for (start, run) in runs(&clusters) {
            let mut at = self.b.cluster_pos(clusters[start]);
            let mut run_bytes = run as u64 * cs;
            while run_bytes > 0 && done < it.len {
                let chunk = run_bytes.min(4 << 20).min(it.len - done);
                buf.resize(chunk as usize, 0);
                self.w.read_at(at, &mut buf)?;
                // Past the valid data length a file reads as zeros.
                if done + chunk > it.valid {
                    let from = it.valid.saturating_sub(done) as usize;
                    buf[from..].fill(0);
                }
                out.write_all(&buf)?;
                done += chunk;
                at += chunk;
                run_bytes -= chunk;
            }
        }
        if done < it.len {
            // A sparse tail beyond the allocated clusters (corrupt, or zero clusters).
            let zeros = vec![0u8; 64 * 1024];
            while done < it.len {
                let k = (it.len - done).min(zeros.len() as u64) as usize;
                out.write_all(&zeros[..k])?;
                done += k as u64;
            }
        }
        Ok(done)
    }

    fn create_file(
        &mut self,
        dir: &mut DirRef,
        name: &str,
        src: &mut dyn Read,
        len: u64,
        meta: &fs::Metadata,
    ) -> Result<()> {
        let uname = check_name(name)?;
        let alloc = self.write_data(src, len)?;
        let modified =
            Stamp::from_time_clamped(meta.modified().unwrap_or_else(|_| SystemTime::now()));
        let created = meta
            .created()
            .map(Stamp::from_time_clamped)
            .unwrap_or(modified);
        let accessed = meta
            .accessed()
            .map(Stamp::from_time_clamped)
            .unwrap_or(modified);
        let mut attrs = A_ARCHIVE;
        if meta.permissions().readonly() {
            attrs |= A_READ_ONLY;
        }
        let set = build_set(
            &SetData {
                name: uname,
                attrs,
                created,
                modified,
                accessed: Stamp {
                    ms10: 0,
                    ..accessed
                },
                alloc,
                len,
                valid: len,
            },
            &self.up,
        );
        if let Err(e) = self.insert_set(dir, &set) {
            self.release(alloc, Some(len))?;
            return Err(e);
        }
        Ok(())
    }

    fn inject(
        &mut self,
        host: &Path,
        dest_dir: &str,
        overwrite: bool,
        stats: &mut Stats,
    ) -> Result<()> {
        let name = host
            .file_name()
            .ok_or_else(|| Error::InvalidPath(host.display().to_string()))?
            .to_string_lossy()
            .into_owned();
        let meta = fs::metadata(host)?;
        let img_path = join(dest_dir, &name);
        if meta.is_dir() {
            self.ensure_dir(&img_path)?;
            stats.dirs += 1;
            let mut children: Vec<_> = fs::read_dir(host)?.collect::<std::io::Result<_>>()?;
            children.sort_by_key(|c| c.file_name());
            for c in children {
                self.inject(&c.path(), &img_path, overwrite, stats)?;
            }
            return Ok(());
        }
        let mut dir = self.ensure_dir(dest_dir)?;
        let dd = self.read_dir(&dir)?;
        if let Some(old) = self.find(&dd, &name) {
            if old.is_dir() {
                return Err(Error::Exists(format!("{} (a folder)", display(&img_path))));
            }
            if !overwrite {
                return Err(Error::Exists(display(&img_path)));
            }
            self.delete_set(&dir, old.slot, old.count)?;
            self.release(old.alloc, Some(old.len))?;
        }
        let mut f = File::open(host)?;
        self.create_file(&mut dir, &name, &mut f, meta.len(), &meta)
            .map_err(|e| match e {
                Error::Invalid(m) if m.contains("no free clusters") => Error::Invalid(format!(
                    "not enough free space in the image for {}",
                    display(&img_path)
                )),
                other => other,
            })?;
        stats.files += 1;
        stats.bytes += meta.len();
        Ok(())
    }

    fn remove(&mut self, path: &str, recursive: bool, stats: &mut Stats) -> Result<()> {
        let (dir, it) = self
            .lookup(path)?
            .ok_or_else(|| Error::NotFound(display(path)))?;
        if it.is_dir() {
            let sub = Inner::subdir(split_parent(path).0, &it);
            let dd = self.read_dir(&sub)?;
            let children = self.items(&dd);
            if !children.is_empty() && !recursive {
                return Err(Error::Invalid(format!(
                    "folder is not empty: {}",
                    display(path)
                )));
            }
            for c in children {
                self.remove(&join(path, &c.name), true, stats)?;
            }
            stats.dirs += 1;
        } else {
            stats.files += 1;
            stats.bytes += it.len;
        }
        // The folder's set may have moved only if the parent changed, which removing
        // children does not do.
        self.delete_set(&dir, it.slot, it.count)?;
        self.release(it.alloc, Some(it.len))
    }

    fn upper(&self, s: &str) -> Vec<u16> {
        s.encode_utf16().map(|c| self.up.up(c)).collect()
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let (src_dir, it) = self
            .lookup(from)?
            .ok_or_else(|| Error::NotFound(display(from)))?;
        let (dst_dir_path, dst_name) = match self.lookup(to)? {
            // The same entry under another case: a case-only rename.
            Some((d, t)) if t.slot == it.slot && d.alloc.first == src_dir.alloc.first => {
                let (p, n) = split_parent(to);
                (p.to_string(), n.to_string())
            }
            Some((_, t)) if t.is_dir() => (to.to_string(), it.name.clone()),
            Some(_) => return Err(Error::Exists(display(to))),
            None if to.is_empty() => (String::new(), it.name.clone()),
            None => {
                let (p, n) = split_parent(to);
                (p.to_string(), n.to_string())
            }
        };
        let target = join(&dst_dir_path, &dst_name);
        if it.is_dir() {
            let (f, t) = (self.upper(from), self.upper(&target));
            if t.len() > f.len() && t.starts_with(&f) && t[f.len()] == u16::from(b'/') {
                return Err(Error::Invalid(format!(
                    "cannot move {} into itself",
                    display(from)
                )));
            }
        }
        if target == from {
            return Ok(());
        }
        let uname = check_name(&dst_name)?;
        let (src_parent, _) = split_parent(from);
        let mut dst = self.dir_at(&dst_dir_path)?;
        let same_dir = dst.alloc.first == src_dir.alloc.first;
        if !same_dir {
            let dd = self.read_dir(&dst)?;
            if self.find(&dd, &dst_name).is_some() {
                return Err(Error::Exists(display(&target)));
            }
        } else {
            dst = src_dir.clone();
        }
        let set = build_set(
            &SetData {
                name: uname,
                attrs: it.attrs,
                created: it.created,
                modified: it.modified,
                accessed: it.accessed,
                alloc: it.alloc,
                len: it.len,
                valid: it.valid,
            },
            &self.up,
        );
        if same_dir {
            // Remove first so a rename to a longer name can reuse the space.
            self.delete_set(&src_dir, it.slot, it.count)?;
            if let Err(e) = self.insert_set(&mut dst, &set) {
                let mut dd = self.read_dir(&src_dir)?;
                for s in it.slot..it.slot + it.count {
                    dd.data[s * ENTRY] |= 0x80;
                }
                self.write_slots(&dd, it.slot, it.slot + it.count)?;
                return Err(e);
            }
        } else {
            self.insert_set(&mut dst, &set)?;
            // Growing the destination can change the source folder's recorded size.
            let src_dir = self.dir_at(src_parent)?;
            self.delete_set(&src_dir, it.slot, it.count)?;
        }
        Ok(())
    }

    fn edit(&mut self, path: &str, ed: &Edit, recursive: bool) -> Result<u64> {
        let mut n = 0;
        if !path.is_empty() {
            let (dir, mut it) = self
                .lookup(path)?
                .ok_or_else(|| Error::NotFound(display(path)))?;
            ed.apply(&mut it);
            self.rewrite_set(&dir, &it)?;
            n += 1;
            if !(recursive && it.is_dir()) {
                return Ok(n);
            }
        } else if !recursive {
            return Err(Error::Invalid(
                "the root folder has no attributes or times on exFAT".into(),
            ));
        }
        let dir = self.dir_at(path)?;
        let dd = self.read_dir(&dir)?;
        for c in self.items(&dd) {
            n += self.edit(&join(path, &c.name), ed, true)?;
        }
        Ok(n)
    }

    fn set_label(&mut self, label: &str) -> Result<()> {
        let u: Vec<u16> = label.encode_utf16().collect();
        if u.len() > 11 || u.iter().any(|&c| c < 0x20) {
            return Err(Error::Invalid(
                "an exFAT volume label has at most 11 characters".into(),
            ));
        }
        let mut entry = [0u8; ENTRY];
        entry[0] = T_LABEL;
        entry[1] = u.len() as u8;
        for (k, &c) in u.iter().enumerate() {
            put_le16(&mut entry, 2 + 2 * k, c);
        }
        let mut root = self.root();
        let mut dd = self.read_dir(&root)?;
        let existing = (0..dd.slots())
            .take_while(|&s| dd.kind(s) != T_END)
            .find(|&s| matches!(dd.kind(s), T_LABEL | T_LABEL_UNUSED));
        match existing {
            Some(s) => {
                dd.data[s * ENTRY..(s + 1) * ENTRY].copy_from_slice(&entry);
                self.write_slots(&dd, s, s + 1)?;
            }
            None => {
                self.insert_set(&mut root, &entry)?;
            }
        }
        self.label = label.to_string();
        Ok(())
    }

    // --- consistency check (tests) ----------------------------------------------------

    fn check(&mut self) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        let mut owner: HashMap<u32, String> = HashMap::new();
        let mut claim = |clusters: &[u32], what: &str, problems: &mut Vec<String>| {
            for &c in clusters {
                if let Some(prev) = owner.insert(c, what.to_string()) {
                    problems.push(format!("cluster {c} is used by both {prev} and {what}"));
                }
            }
        };
        let bitmap = self.clusters(self.bitmap_alloc, Some(self.bitmap.len() as u64))?;
        claim(&bitmap, "the allocation bitmap", &mut problems);
        let root = self.root();
        let root_dd = self.read_dir(&root)?;
        claim(&root_dd.clusters, "the root folder", &mut problems);
        for s in 0..root_dd.slots() {
            let e = root_dd.entry(s).to_vec();
            if e[0] == T_END {
                break;
            }
            if e[0] == T_UPCASE {
                let a = Alloc {
                    first: le32(&e, 20),
                    contiguous: false,
                };
                let up = self.clusters(a, Some(le64(&e, 24)))?;
                claim(&up, "the up-case table", &mut problems);
            }
        }
        let mut stack = vec![(String::new(), root)];
        let mut seen_dirs = HashSet::new();
        while let Some((path, d)) = stack.pop() {
            let dd = self.read_dir(&d)?;
            // Sets the reader skipped (bad checksum, bad shape) are problems too.
            let items = self.items(&dd);
            let starts: HashSet<usize> = items.iter().map(|i| i.slot).collect();
            for s in 0..dd.slots() {
                if dd.kind(s) == T_END {
                    break;
                }
                if dd.kind(s) == T_FILE && !starts.contains(&s) {
                    problems.push(format!("{}: damaged entry set at slot {s}", display(&path)));
                }
            }
            let mut names = HashSet::new();
            for it in items {
                let p = join(&path, &it.name);
                let u: Vec<u16> = it.name.encode_utf16().collect();
                let upper: Vec<u16> = u.iter().map(|&c| self.up.up(c)).collect();
                if !names.insert(upper) {
                    problems.push(format!("{}: duplicate name", display(&p)));
                }
                let set = &dd.data[(it.slot + 1) * ENTRY..(it.slot + 2) * ENTRY];
                if le16(set, 4) != self.up.hash(&u) {
                    problems.push(format!("{}: wrong name hash", display(&p)));
                }
                if it.valid > it.len {
                    problems.push(format!("{}: valid length beyond the size", display(&p)));
                }
                if it.is_dir() && (it.len == 0 || !it.len.is_multiple_of(self.b.cluster)) {
                    problems.push(format!(
                        "{}: folder size is not whole clusters",
                        display(&p)
                    ));
                }
                if (it.alloc.first == 0) != (it.len == 0) {
                    problems.push(format!("{}: first cluster and size disagree", display(&p)));
                }
                match self.clusters(it.alloc, Some(it.len)) {
                    Ok(cl) => {
                        if (cl.len() as u64) < it.len.div_ceil(self.b.cluster) {
                            problems.push(format!("{}: cluster chain too short", display(&p)));
                        }
                        claim(&cl, &p, &mut problems);
                    }
                    Err(e) => problems.push(format!("{}: {e}", display(&p))),
                }
                if it.is_dir() && seen_dirs.insert(it.alloc.first) {
                    stack.push((p.clone(), Inner::subdir(&path, &it)));
                }
            }
        }
        for c in 2..self.b.clusters + 2 {
            match (self.used(c), owner.contains_key(&c)) {
                (true, false) => {
                    problems.push(format!("cluster {c} is marked used but nothing uses it"))
                }
                (false, true) => problems.push(format!(
                    "cluster {c} is in use ({}) but marked free",
                    owner[&c]
                )),
                _ => {}
            }
        }
        let bps = self.b.bps as usize;
        let mut region = vec![0u8; 24 * bps];
        self.w.read_at(0, &mut region)?;
        let sum = boot_checksum(&region, bps);
        for (k, base) in [(0, 0), (1, 12 * bps)] {
            if boot_checksum(&region[base..], bps) != sum
                || region[base + 11 * bps..base + 12 * bps]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|c| u32::from_le_bytes(*c) != sum)
            {
                problems.push(format!(
                    "{} boot region checksum mismatch",
                    if k == 0 { "main" } else { "backup" }
                ));
            }
        }
        if region[12 * bps..12 * bps + 106] != region[..106]
            || region[12 * bps + 108..12 * bps + 112] != region[108..112]
            || region[12 * bps + 113..13 * bps] != region[113..bps]
        {
            problems.push("backup boot sector differs from the main one".into());
        }
        Ok(problems)
    }
}

/// Runs of consecutive clusters: (index of the first, length).
fn runs(clusters: &[u32]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < clusters.len() {
        let mut j = i + 1;
        while j < clusters.len() && clusters[j] == clusters[j - 1] + 1 && (j - i) < 1024 {
            j += 1;
        }
        out.push((i, j - i));
        i = j;
    }
    out
}

fn read_fill(src: &mut dyn Read, buf: &mut [u8]) -> Result<()> {
    let mut got = 0;
    while got < buf.len() {
        match src.read(&mut buf[got..]) {
            Ok(0) => {
                buf[got..].fill(0);
                break;
            }
            Ok(k) => got += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// A parsed `EntryEdit`.
struct Edit {
    set: u16,
    clear: u16,
    modified: Option<Stamp>,
    created: Option<Stamp>,
    accessed: Option<Stamp>,
}

impl Edit {
    fn apply(&self, it: &mut Item) {
        it.attrs = (it.attrs | self.set) & !self.clear;
        if let Some(s) = self.modified {
            it.modified = s;
        }
        if let Some(s) = self.created {
            it.created = s;
        }
        if let Some(s) = self.accessed {
            it.accessed = Stamp { ms10: 0, ..s };
        }
    }
}

// --- public face ------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct ExFatInfo {
    pub label: String,
    pub serial: u32,
    pub sector_size: u64,
    pub cluster_size: u64,
    pub clusters: u32,
    pub free_clusters: u64,
}

pub(crate) struct ExFatFs {
    inner: RefCell<Inner>,
    writable: bool,
}

impl ExFatFs {
    pub(crate) fn open(w: Window, writable: bool) -> Result<ExFatFs> {
        Ok(ExFatFs {
            inner: RefCell::new(Inner::mount(w, writable)?),
            writable,
        })
    }

    pub(crate) fn info(&self) -> ExFatInfo {
        let i = self.inner.borrow();
        ExFatInfo {
            label: i.label.clone(),
            serial: i.b.serial,
            sector_size: i.b.bps,
            cluster_size: i.b.cluster,
            clusters: i.b.clusters,
            free_clusters: i.free_clusters(),
        }
    }

    pub(crate) fn write_blocker(&self) -> Option<String> {
        self.inner.borrow().blocker.clone()
    }

    fn rw(&self) -> Result<std::cell::RefMut<'_, Inner>> {
        if !self.writable {
            return Err(Error::Invalid("the image is open read-only".into()));
        }
        let mut i = self.inner.borrow_mut();
        i.begin_write()?;
        Ok(i)
    }

    pub(crate) fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let mut i = self.inner.borrow_mut();
        let d = i.dir_at(dir)?;
        let dd = i.read_dir(&d)?;
        let mut out: Vec<Entry> = i.items(&dd).iter().map(|it| item_entry(it, dir)).collect();
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    pub(crate) fn stat(&self, path: &str) -> Result<Option<Entry>> {
        if path.is_empty() {
            return Ok(None);
        }
        let mut i = self.inner.borrow_mut();
        let (parent, _) = split_parent(path);
        Ok(i.lookup(path)?.map(|(_, it)| item_entry(&it, parent)))
    }

    pub(crate) fn copy_out(&self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let mut i = self.inner.borrow_mut();
        let (_, it) = i
            .lookup(path)?
            .ok_or_else(|| Error::NotFound(display(path)))?;
        if it.is_dir() {
            return Err(Error::Invalid(format!("{} is a folder", display(path))));
        }
        i.read_data(&it, out)
    }

    pub(crate) fn extract(&self, src: &str, dest: &Path, overwrite: bool) -> Result<Stats> {
        let mut i = self.inner.borrow_mut();
        fs::create_dir_all(dest)?;
        let mut stats = Stats::default();
        if src.is_empty() {
            extract_dir(&mut i, "", dest, overwrite, &mut stats)?;
        } else {
            let (_, it) = i
                .lookup(src)?
                .ok_or_else(|| Error::NotFound(display(src)))?;
            extract_one(
                &mut i,
                split_parent(src).0,
                &it,
                dest,
                overwrite,
                &mut stats,
            )?;
        }
        Ok(stats)
    }

    pub(crate) fn inject(&self, host: &Path, dest_dir: &str, overwrite: bool) -> Result<Stats> {
        let mut i = self.rw()?;
        let mut stats = Stats::default();
        let r = i.inject(host, dest_dir, overwrite, &mut stats);
        i.flush()?;
        r.map(|_| stats)
    }

    pub(crate) fn mkdir(&self, path: &str) -> Result<()> {
        let mut i = self.rw()?;
        let r = i.ensure_dir(path).map(|_| ());
        i.flush()?;
        r
    }

    pub(crate) fn remove(&self, path: &str, recursive: bool) -> Result<Stats> {
        let mut i = self.rw()?;
        let mut stats = Stats::default();
        let r = i.remove(path, recursive, &mut stats);
        i.flush()?;
        r.map(|_| stats)
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> Result<()> {
        let mut i = self.rw()?;
        let r = i.rename(from, to);
        i.flush()?;
        r
    }

    pub(crate) fn set_label(&self, label: Option<&str>) -> Result<()> {
        let mut i = self.rw()?;
        let r = i.set_label(label.map(str::trim).unwrap_or(""));
        i.flush()?;
        r
    }

    /// Attributes (read-only, hidden, system, archive) and times of `path` (and, with
    /// `recursive`, of everything below it). Returns the number of entries changed.
    pub(crate) fn edit(&self, path: &str, edit: &EntryEdit, recursive: bool) -> Result<u64> {
        let mut set = 0;
        let mut clear = 0;
        for (v, bit) in [
            (edit.read_only, A_READ_ONLY),
            (edit.hidden, A_HIDDEN),
            (edit.system, A_SYSTEM),
            (edit.archive, A_ARCHIVE),
        ] {
            match v {
                Some(true) => set |= bit,
                Some(false) => clear |= bit,
                None => {}
            }
        }
        let parse = |s: &Option<String>| -> Result<Option<Stamp>> {
            s.as_deref()
                .map(|t| Stamp::from_time(crate::ext::parse_local_time(t)?))
                .transpose()
        };
        let ed = Edit {
            set,
            clear,
            modified: parse(&edit.modified)?,
            created: parse(&edit.created)?,
            accessed: parse(&edit.accessed)?,
        };
        let mut i = self.rw()?;
        let r = i.edit(path, &ed, recursive);
        i.flush()?;
        r
    }

    pub(crate) fn flush(&self) -> Result<()> {
        self.inner.borrow_mut().flush()
    }

    pub(crate) fn check(&self) -> Result<Vec<String>> {
        self.inner.borrow_mut().check()
    }
}

fn extract_one(
    i: &mut Inner,
    parent: &str,
    it: &Item,
    dest: &Path,
    overwrite: bool,
    stats: &mut Stats,
) -> Result<()> {
    let target = dest.join(host_safe_name(&it.name));
    let path = join(parent, &it.name);
    if it.is_dir() {
        fs::create_dir_all(&target)?;
        stats.dirs += 1;
        return extract_dir(i, &path, &target, overwrite, stats);
    }
    if !overwrite && target.exists() {
        return Err(Error::Exists(target.display().to_string()));
    }
    let mut out = File::create(&target)?;
    stats.bytes += i.read_data(it, &mut out)?;
    stats.files += 1;
    if let Some(t) = it.modified.system_time() {
        let _ = out.set_modified(t);
    }
    Ok(())
}

fn extract_dir(
    i: &mut Inner,
    path: &str,
    dest: &Path,
    overwrite: bool,
    stats: &mut Stats,
) -> Result<()> {
    let d = i.dir_at(path)?;
    let dd = i.read_dir(&d)?;
    for it in i.items(&dd) {
        extract_one(i, path, &it, dest, overwrite, stats)?;
    }
    Ok(())
}

// --- formatting ---------------------------------------------------------------------------

/// Format `w` as exFAT. `hidden` is the partition's first sector on the disk (0 for a bare
/// volume). Cluster sizes follow Windows' defaults: 4 KiB up to 256 MiB, 32 KiB up to
/// 32 GiB, 128 KiB above.
pub(crate) fn format(mut w: Window, label: Option<&str>, hidden: u64) -> Result<()> {
    const BPS: u64 = 512;
    let bytes = w.len();
    if bytes < 1 << 20 {
        return Err(Error::Invalid(
            "an exFAT volume needs at least 1 MiB".into(),
        ));
    }
    let cluster: u64 = if bytes <= 256 << 20 {
        4096
    } else if bytes <= 32 << 30 {
        32 * 1024
    } else {
        128 * 1024
    };
    let spc = cluster / BPS;
    let sectors = bytes / BPS;
    // FAT at 1 MiB on larger volumes (flash erase-block alignment), right after the two
    // boot regions on small ones; the heap starts on a cluster boundary.
    let fat_offset: u64 = if bytes >= 64 << 20 { 2048 } else { 24 };
    let mut clusters = (sectors - fat_offset) / spc;
    let (fat_length, heap_offset) = loop {
        let fat_length = ((clusters + 2) * 4).div_ceil(BPS);
        let heap = (fat_offset + fat_length).div_ceil(spc) * spc;
        let fit = (sectors.saturating_sub(heap)) / spc;
        if fit >= clusters {
            break (fat_length, heap);
        }
        clusters = fit;
    };
    let clusters = clusters.min(0xFFFF_FFF5) as u32;
    if clusters < 4 {
        return Err(Error::Invalid("the volume is too small for exFAT".into()));
    }
    let serial = crate::volume::new_volume_id();

    let up = Upcase::generated();
    let table = up.encode();
    let bitmap_len = u64::from(clusters).div_ceil(8);
    let bitmap_clusters = bitmap_len.div_ceil(cluster) as u32;
    let table_clusters = (table.len() as u64).div_ceil(cluster) as u32;
    let bitmap_first = 2u32;
    let table_first = bitmap_first + bitmap_clusters;
    let root_first = table_first + table_clusters;
    let used = bitmap_clusters + table_clusters + 1;
    if used + 1 > clusters {
        return Err(Error::Invalid("the volume is too small for exFAT".into()));
    }

    // Boot region.
    let bps = BPS as usize;
    let mut region = vec![0u8; 12 * bps];
    {
        let s = &mut region[..bps];
        s[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
        s[3..11].copy_from_slice(b"EXFAT   ");
        put_le64(s, 64, hidden);
        put_le64(s, 72, sectors);
        put_le32(s, 80, fat_offset as u32);
        put_le32(s, 84, fat_length as u32);
        put_le32(s, 88, heap_offset as u32);
        put_le32(s, 92, clusters);
        put_le32(s, 96, root_first);
        put_le32(s, 100, serial);
        put_le16(s, 104, 0x0100);
        s[108] = BPS.trailing_zeros() as u8;
        s[109] = spc.trailing_zeros() as u8;
        s[110] = 1;
        s[111] = 0x80;
        s[112] = (u64::from(used) * 100 / u64::from(clusters)) as u8;
        // No boot code: halt.
        s[120..510].fill(0xF4);
        put_le16(s, 510, 0xAA55);
    }
    for k in 1..=8 {
        put_le32(&mut region[k * bps..(k + 1) * bps], bps - 4, 0xAA55_0000);
    }
    let sum = boot_checksum(&region, bps);
    for k in 0..bps / 4 {
        put_le32(&mut region[11 * bps..], 4 * k, sum);
    }
    w.write_at(0, &region)?;
    w.write_at(12 * BPS, &region)?;

    // FAT: media descriptor, end marker, then the chains of the bitmap, table and root.
    let mut fat = vec![0u8; (fat_length * BPS) as usize];
    put_le32(&mut fat, 0, 0xFFFF_FFF8);
    put_le32(&mut fat, 4, 0xFFFF_FFFF);
    for (first, n) in [
        (bitmap_first, bitmap_clusters),
        (table_first, table_clusters),
        (root_first, 1),
    ] {
        for c in first..first + n {
            let next = if c + 1 < first + n { c + 1 } else { EOC };
            put_le32(&mut fat, 4 * c as usize, next);
        }
    }
    // Zero the space between the boot regions and the FAT, and the FAT itself.
    let gap = vec![0u8; ((fat_offset - 24) * BPS) as usize];
    if !gap.is_empty() {
        w.write_at(24 * BPS, &gap)?;
    }
    w.write_at(fat_offset * BPS, &fat)?;

    let pos = |c: u32| heap_offset * BPS + u64::from(c - 2) * cluster;
    let mut bitmap = vec![0u8; (u64::from(bitmap_clusters) * cluster) as usize];
    for i in 0..used as usize {
        bitmap[i / 8] |= 1 << (i % 8);
    }
    w.write_at(pos(bitmap_first), &bitmap)?;
    let mut tbl = table.clone();
    tbl.resize((u64::from(table_clusters) * cluster) as usize, 0);
    w.write_at(pos(table_first), &tbl)?;

    let mut root = vec![0u8; cluster as usize];
    let lab: Vec<u16> = label.map(str::trim).unwrap_or("").encode_utf16().collect();
    if lab.len() > 11 || lab.iter().any(|&c| c < 0x20) {
        return Err(Error::Invalid(
            "an exFAT volume label has at most 11 characters".into(),
        ));
    }
    root[0] = T_LABEL;
    root[1] = lab.len() as u8;
    for (k, &c) in lab.iter().enumerate() {
        put_le16(&mut root, 2 + 2 * k, c);
    }
    let e = &mut root[ENTRY..2 * ENTRY];
    e[0] = T_BITMAP;
    put_le32(e, 20, bitmap_first);
    put_le64(e, 24, bitmap_len);
    let e = &mut root[2 * ENTRY..3 * ENTRY];
    e[0] = T_UPCASE;
    put_le32(e, 4, table_checksum(&table));
    put_le32(e, 20, table_first);
    put_le64(e, 24, table.len() as u64);
    w.write_at(pos(root_first), &root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upcase_round_trip_and_basics() {
        let up = Upcase::generated();
        assert_eq!(up.up(u16::from(b'a')), u16::from(b'A'));
        assert_eq!(up.up(0x00E9), 0x00C9); // é → É
        assert_eq!(up.up(0x03C9), 0x03A9); // ω → Ω
        assert_eq!(up.up(u16::from(b'Z')), u16::from(b'Z'));
        let enc = up.encode();
        assert!(enc.len() < 8192, "{}", enc.len());
        assert_eq!(Upcase::decode(&enc).0, up.0);
    }

    #[test]
    fn checksums_follow_the_spec_algorithm() {
        // Rotate-right-and-add, worked by hand.
        assert_eq!(table_checksum(&[1, 2]), 0x8000_0000 + 2);
        let mut set = vec![0u8; 64];
        set[0] = 0x85;
        set[2] = 0xFF; // ignored
        set[3] = 0xFF;
        assert_eq!(set_checksum(&set), {
            let mut c: u16 = 0;
            for (i, &b) in set.iter().enumerate() {
                if i != 2 && i != 3 {
                    c = c.rotate_right(1).wrapping_add(u16::from(b));
                }
            }
            c
        });
    }

    #[test]
    fn stamps_round_trip() {
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_001_230);
        let s = Stamp::from_time(t).unwrap();
        assert_eq!(s.system_time().unwrap(), t);
        assert!(s.utc & 0x80 != 0);
        assert!(Stamp::from_time(UNIX_EPOCH).is_err());
    }

    #[test]
    fn runs_group_consecutive_clusters() {
        assert_eq!(runs(&[5, 6, 7, 10, 11, 3]), vec![(0, 3), (3, 2), (5, 1)]);
    }
}
