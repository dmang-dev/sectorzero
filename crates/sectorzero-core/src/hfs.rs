//! HFS (Mac OS Standard) and HFS+ / HFSX (Mac OS Extended), written from Apple's
//! documentation: Inside Macintosh: Files (the HFS Master Directory Block, B-trees, catalog
//! and extents records) and Technical Note TN1150 (HFS Plus). HFS+ volumes wrapped in an HFS
//! volume (how classic Mac OS formatted HFS+ disks) open through the wrapper's embedded extent.
//!
//! Folders are listed by walking the catalog B-tree from the folder's thread record, which
//! is the smallest key for its parent ID under any name ordering, so reading needs no
//! collation tables. Names are Mac Roman (HFS) or UTF-16 (HFS+, shown composed as NFC);
//! a `/` in a Mac name is shown as `:`, as macOS does.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, TimeZone};
use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result, display};
use crate::util::{be32, be64, put_be16, put_be32, put_be64};
use crate::volume::{
    Attributes, Entry, ForkMode, MacInfo, Stats, host_safe_name, join, split_parent,
};
use crate::window::Window;

pub(crate) const HFS_SIG: u16 = 0x4244; // "BD"
pub(crate) const HFSPLUS_SIG: u16 = 0x482B; // "H+"
pub(crate) const HFSX_SIG: u16 = 0x4858; // "HX"

/// Catalog node IDs with a fixed meaning.
const ROOT_PARENT: u32 = 1;
const ROOT_FOLDER: u32 = 2;
const EXTENTS_FILE: u32 = 3;
const CATALOG_FILE: u32 = 4;

const FINDER_INVISIBLE: u16 = 0x4000;
/// Seconds from 1904-01-01 to 1970-01-01.
const MAC_EPOCH: i64 = 2_082_844_800;

/// Names at the root of an HFS+ volume that belong to the file system, not the user.
const PRIVATE_DATA: &str = "\0\0\0\0HFS+ Private Data";
const HIDDEN_ROOT: [&str; 4] = [
    PRIVATE_DATA,
    ".HFS+ Private Directory Data\r",
    ".journal",
    ".journal_info_block",
];

pub(crate) fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

/// The file system name for the volume header at byte 1024 of `head`, if it is HFS.
pub(crate) fn fs_name(head: &[u8]) -> Option<&'static str> {
    if head.len() < 1024 + 162 {
        return None;
    }
    let h = &head[1024..];
    match be16(h, 0) {
        HFS_SIG if be16(h, 124) == HFSPLUS_SIG => Some("HFS+"),
        HFS_SIG if be32(h, 20) != 0 && be32(h, 20).is_multiple_of(512) => Some("HFS"),
        HFSPLUS_SIG if be16(h, 2) == 4 => Some("HFS+"),
        HFSX_SIG if be16(h, 2) == 5 => Some("HFSX"),
        _ => None,
    }
}

/// Mac Roman bytes 0x80-0xFF in Unicode (Apple's mapping; 0xDB is the euro sign since Mac
/// OS 8.5, the currency sign before).
const MAC_ROMAN: [u16; 128] = [
    0x00C4, 0x00C5, 0x00C7, 0x00C9, 0x00D1, 0x00D6, 0x00DC, 0x00E1, 0x00E0, 0x00E2, 0x00E4, 0x00E3,
    0x00E5, 0x00E7, 0x00E9, 0x00E8, 0x00EA, 0x00EB, 0x00ED, 0x00EC, 0x00EE, 0x00EF, 0x00F1, 0x00F3,
    0x00F2, 0x00F4, 0x00F6, 0x00F5, 0x00FA, 0x00F9, 0x00FB, 0x00FC, 0x2020, 0x00B0, 0x00A2, 0x00A3,
    0x00A7, 0x2022, 0x00B6, 0x00DF, 0x00AE, 0x00A9, 0x2122, 0x00B4, 0x00A8, 0x2260, 0x00C6, 0x00D8,
    0x221E, 0x00B1, 0x2264, 0x2265, 0x00A5, 0x00B5, 0x2202, 0x2211, 0x220F, 0x03C0, 0x222B, 0x00AA,
    0x00BA, 0x03A9, 0x00E6, 0x00F8, 0x00BF, 0x00A1, 0x00AC, 0x221A, 0x0192, 0x2248, 0x2206, 0x00AB,
    0x00BB, 0x2026, 0x00A0, 0x00C0, 0x00C3, 0x00D5, 0x0152, 0x0153, 0x2013, 0x2014, 0x201C, 0x201D,
    0x2018, 0x2019, 0x00F7, 0x25CA, 0x00FF, 0x0178, 0x2044, 0x20AC, 0x2039, 0x203A, 0xFB01, 0xFB02,
    0x2021, 0x00B7, 0x201A, 0x201E, 0x2030, 0x00C2, 0x00CA, 0x00C1, 0x00CB, 0x00C8, 0x00CD, 0x00CE,
    0x00CF, 0x00CC, 0x00D3, 0x00D4, 0xF8FF, 0x00D2, 0x00DA, 0x00DB, 0x00D9, 0x0131, 0x02C6, 0x02DC,
    0x00AF, 0x02D8, 0x02D9, 0x02DA, 0x00B8, 0x02DD, 0x02DB, 0x02C7,
];

pub(crate) fn mac_roman_to_string(b: &[u8]) -> String {
    b.iter()
        .map(|&c| {
            if c < 0x80 {
                char::from(c)
            } else {
                char::from_u32(u32::from(MAC_ROMAN[usize::from(c - 0x80)])).unwrap_or('?')
            }
        })
        .collect()
}

/// Unicode to Mac Roman, or None if a character has no Mac Roman form.
pub(crate) fn string_to_mac_roman(s: &str) -> Option<Vec<u8>> {
    s.nfc()
        .map(|c| {
            if (c as u32) < 0x80 {
                Some(c as u8)
            } else {
                MAC_ROMAN
                    .iter()
                    .position(|&u| u32::from(u) == c as u32)
                    .map(|i| 0x80 + i as u8)
            }
        })
        .collect()
}

mod names;
mod tables;
mod write;

pub(crate) use names::Order;

/// `YYYY-MM-DD HH:MM:SS` for seconds since 1904; HFS stores local time, HFS+ UTC.
fn mac_date(secs: u32, utc: bool) -> String {
    if secs == 0 {
        return String::new();
    }
    let unix = i64::from(secs) - MAC_EPOCH;
    if utc {
        match Local.timestamp_opt(unix, 0).single() {
            Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
            None => String::new(),
        }
    } else {
        DateTime::from_timestamp(unix, 0)
            .map(|t| t.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default()
    }
}

/// Unix seconds of a Mac date, for setting extracted files' times.
fn mac_unix(secs: u32, utc: bool) -> Option<u64> {
    if secs == 0 {
        return None;
    }
    let mut unix = i64::from(secs) - MAC_EPOCH;
    if !utc {
        // Local wall-clock time: undo the local offset.
        let naive = DateTime::from_timestamp(unix, 0)?.naive_utc();
        unix = Local.from_local_datetime(&naive).earliest()?.timestamp();
    }
    u64::try_from(unix).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Hfs,
    Plus,
    X,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Extent {
    pub start: u32,
    pub count: u32,
}

/// A fork as the catalog (or volume header) describes it: size and its first extents.
#[derive(Debug, Clone, Default)]
pub(crate) struct ForkDesc {
    pub size: u64,
    pub extents: Vec<Extent>,
}

/// One catalog record.
#[derive(Debug, Clone)]
pub(crate) enum CatRec {
    Folder {
        id: u32,
        created: u32,
        modified: u32,
        accessed: u32,
        finder_flags: u16,
    },
    File(Box<FileRec>),
    Thread {
        name: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct FileRec {
    pub id: u32,
    pub locked: bool,
    pub file_type: [u8; 4],
    pub creator: [u8; 4],
    pub finder_flags: u16,
    /// The 16 bytes of FInfo (type, creator, flags, location, folder).
    pub finfo: [u8; 16],
    /// The 16 bytes of FXInfo / ExtendedFileInfo.
    pub fxinfo: [u8; 16],
    pub created: u32,
    pub modified: u32,
    pub accessed: u32,
    pub data: ForkDesc,
    pub rsrc: ForkDesc,
    /// HFS+ hard links: the iNode number (BSD `special` field) of a `hlnk`/`hfs+` file.
    pub link: Option<u32>,
}

/// A child of a folder: its (display) name and record.
#[derive(Debug, Clone)]
pub(crate) struct Child {
    pub name: String,
    /// The name as stored (Mac Roman bytes on HFS, UTF-16 on HFS+).
    pub key: Vec<u16>,
    pub rec: CatRec,
}

struct BTree {
    fork: Vec<Extent>,
    node_size: usize,
    root: u32,
    depth: u16,
    /// HFSX catalogs: 0xCF case-folding, 0xBC binary name order.
    compare: u8,
}

/// Information for `Volume::info`.
pub(crate) struct HfsInfo {
    pub kind: Kind,
    pub label: String,
    pub block_size: u32,
    pub total_blocks: u32,
    pub free_blocks: u32,
    /// HFS+ volume UUID (Finder info words 6-7), 0 when unset or on HFS.
    pub uuid: u64,
    pub journaled: bool,
    pub wrapped: bool,
}

pub(crate) struct HfsFs {
    w: RefCell<Window>,
    kind: Kind,
    /// Byte offset of allocation block 0 inside the window.
    base: u64,
    block: u64,
    total_blocks: u32,
    free_blocks: Cell<u32>,
    label: RefCell<String>,
    uuid: u64,
    journaled: bool,
    wrapped: bool,
    extents: RefCell<BTree>,
    catalog: RefCell<BTree>,
    /// HFS+: the folder holding hard-linked files' data ("iNode<n>").
    private_dir: Cell<Option<u32>>,
}

/// Whether a root folder name is one of the file system's own (hidden).
fn is_private(key: &[u16]) -> bool {
    HIDDEN_ROOT
        .iter()
        .any(|h| h.encode_utf16().eq(key.iter().copied()))
}

fn invalid(what: &str) -> Error {
    Error::Invalid(format!("HFS: {what}"))
}

/// Extent records: HFS has three 16-bit pairs, HFS+ eight 32-bit pairs.
fn extents_of(b: &[u8], plus: bool) -> Vec<Extent> {
    let (n, w) = if plus { (8, 8) } else { (3, 4) };
    (0..n)
        .map(|i| {
            let o = i * w;
            if plus {
                Extent {
                    start: be32(b, o),
                    count: be32(b, o + 4),
                }
            } else {
                Extent {
                    start: u32::from(be16(b, o)),
                    count: u32::from(be16(b, o + 2)),
                }
            }
        })
        .filter(|e| e.count != 0)
        .collect()
}

/// An HFS+ fork data structure (80 bytes).
fn plus_fork(b: &[u8]) -> ForkDesc {
    ForkDesc {
        size: be64(b, 0),
        extents: extents_of(&b[16..80], true),
    }
}

impl HfsFs {
    pub(crate) fn open(w: Window) -> Result<HfsFs> {
        let mut w = w;
        let mut mdb = vec![0u8; 512];
        w.read_at(1024, &mut mdb)?;
        let (mut offset, mut wrapped) = (0u64, false);
        let sig = be16(&mdb, 0);
        if sig == HFS_SIG && be16(&mdb, 124) == HFSPLUS_SIG {
            // An HFS wrapper around an HFS+ volume.
            let al_start = u64::from(be16(&mdb, 28)) * 512;
            let al_size = u64::from(be32(&mdb, 20));
            offset = al_start + u64::from(be16(&mdb, 126)) * al_size;
            wrapped = true;
            w.read_at(offset + 1024, &mut mdb)?;
        }
        match be16(&mdb, 0) {
            HFS_SIG => Self::open_hfs(w, &mdb),
            HFSPLUS_SIG | HFSX_SIG => Self::open_plus(w, &mdb, offset, wrapped),
            _ => Err(Error::NotFat("no HFS volume header".into())),
        }
    }

    fn open_hfs(w: Window, mdb: &[u8]) -> Result<HfsFs> {
        let block = u64::from(be32(mdb, 20));
        if block == 0 || block % 512 != 0 {
            return Err(invalid("bad allocation block size"));
        }
        let name_len = usize::from(mdb[36]).min(27);
        let mut fs = HfsFs {
            w: RefCell::new(w),
            kind: Kind::Hfs,
            base: u64::from(be16(mdb, 28)) * 512,
            block,
            total_blocks: u32::from(be16(mdb, 18)),
            free_blocks: Cell::new(u32::from(be16(mdb, 34))),
            label: RefCell::new(mac_roman_to_string(&mdb[37..37 + name_len])),
            uuid: 0,
            journaled: false,
            wrapped: false,
            extents: RefCell::new(BTree {
                fork: extents_of(&mdb[134..146], false),
                node_size: 512,
                root: 0,
                depth: 0,
                compare: 0,
            }),
            catalog: RefCell::new(BTree {
                fork: extents_of(&mdb[150..162], false),
                node_size: 512,
                root: 0,
                depth: 0,
                compare: 0,
            }),
            private_dir: Cell::new(None),
        };
        fs.load_trees(u64::from(be32(mdb, 146)))?;
        Ok(fs)
    }

    fn open_plus(w: Window, vh: &[u8], offset: u64, wrapped: bool) -> Result<HfsFs> {
        let block = u64::from(be32(vh, 40));
        if block < 512 || !block.is_power_of_two() {
            return Err(invalid("bad allocation block size"));
        }
        let ext = plus_fork(&vh[192..272]);
        let cat = plus_fork(&vh[272..352]);
        let mut fs = HfsFs {
            w: RefCell::new(w),
            kind: if be16(vh, 0) == HFSX_SIG {
                Kind::X
            } else {
                Kind::Plus
            },
            base: offset,
            block,
            total_blocks: be32(vh, 44),
            free_blocks: Cell::new(be32(vh, 48)),
            label: RefCell::new(String::new()),
            uuid: be64(vh, 80 + 24),
            journaled: be32(vh, 4) & (1 << 13) != 0,
            wrapped,
            extents: RefCell::new(BTree {
                fork: ext.extents,
                node_size: 512,
                root: 0,
                depth: 0,
                compare: 0,
            }),
            catalog: RefCell::new(BTree {
                fork: cat.extents,
                node_size: 512,
                root: 0,
                depth: 0,
                compare: 0,
            }),
            private_dir: Cell::new(None),
        };
        fs.load_trees(cat.size)?;
        // The volume name is the root folder's name, in its thread record.
        if let Some(CatRec::Thread { name, .. }) = fs.thread(ROOT_FOLDER)? {
            *fs.label.get_mut() = name;
        }
        let private = fs
            .all_children(ROOT_FOLDER)?
            .into_iter()
            .find(|c| c.key == PRIVATE_DATA.encode_utf16().collect::<Vec<_>>())
            .and_then(|c| match c.rec {
                CatRec::Folder { id, .. } => Some(id),
                _ => None,
            });
        fs.private_dir.set(private);
        Ok(fs)
    }

    /// Read both B-tree header nodes; extend the catalog fork from the extents tree when
    /// its first extents do not cover it.
    fn load_trees(&mut self, catalog_size: u64) -> Result<()> {
        let ext = std::mem::take(&mut self.extents.get_mut().fork);
        *self.extents.get_mut() = self.tree_header(ext)?;
        let mut cat = std::mem::take(&mut self.catalog.get_mut().fork);
        let have: u64 = cat.iter().map(|e| u64::from(e.count)).sum();
        if have * self.block < catalog_size {
            let start = have as u32;
            cat.extend(self.overflow(CATALOG_FILE, 0, start)?);
        }
        *self.catalog.get_mut() = self.tree_header(cat)?;
        Ok(())
    }

    fn tree_header(&self, fork: Vec<Extent>) -> Result<BTree> {
        let mut head = vec![0u8; 512];
        self.read_extents(&fork, 0, &mut head)?;
        if head[8] != 1 {
            return Err(invalid("B-tree header node missing"));
        }
        let h = &head[14..];
        let node_size = usize::from(be16(h, 18));
        if !(512..=32768).contains(&node_size) || !node_size.is_power_of_two() {
            return Err(invalid("bad B-tree node size"));
        }
        Ok(BTree {
            fork,
            node_size,
            root: be32(h, 2),
            depth: be16(h, 0),
            compare: h[37],
        })
    }

    /// Read `buf.len()` bytes at `offset` of a fork described by `extents`.
    fn read_extents(&self, extents: &[Extent], mut offset: u64, mut buf: &mut [u8]) -> Result<()> {
        let mut w = self.w.borrow_mut();
        for e in extents {
            let len = u64::from(e.count) * self.block;
            if offset >= len {
                offset -= len;
                continue;
            }
            let n = (len - offset).min(buf.len() as u64) as usize;
            let at = self.base + u64::from(e.start) * self.block + offset;
            w.read_at(at, &mut buf[..n])?;
            buf = &mut buf[n..];
            offset = 0;
            if buf.is_empty() {
                return Ok(());
            }
        }
        if buf.is_empty() {
            Ok(())
        } else {
            Err(invalid("read past the end of a fork"))
        }
    }

    fn node(&self, tree: &BTree, n: u32) -> Result<Vec<u8>> {
        let mut b = vec![0u8; tree.node_size];
        self.read_extents(&tree.fork, u64::from(n) * tree.node_size as u64, &mut b)?;
        Ok(b)
    }

    fn plus(&self) -> bool {
        self.kind != Kind::Hfs
    }

    /// Records of a node as (key, data) slices.
    fn records<'a>(&self, node: &'a [u8]) -> Vec<(&'a [u8], &'a [u8])> {
        let n = usize::from(be16(node, 10));
        let size = node.len();
        let off = |i: usize| usize::from(be16(node, size - 2 * (i + 1)));
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let (start, end) = (off(i), off(i + 1).min(size));
            if start >= end || end > size {
                break;
            }
            let rec = &node[start..end];
            let key_len = if self.plus() {
                2 + usize::from(be16(rec, 0))
            } else {
                (1 + usize::from(rec[0])).next_multiple_of(2)
            };
            if key_len > rec.len() {
                break;
            }
            out.push((&rec[..key_len], &rec[key_len..]));
        }
        out
    }

    /// Find the first leaf record not below `target` (`cmp(key)` orders a key against the
    /// target) and return records from there while `keep(key)` holds.
    fn scan(
        &self,
        tree: &BTree,
        cmp: &dyn Fn(&[u8]) -> Ordering,
        keep: &dyn Fn(&[u8]) -> bool,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        if tree.root == 0 {
            return Ok(out);
        }
        let mut n = tree.root;
        for _ in 0..=tree.depth.max(1) + 8 {
            let node = self.node(tree, n)?;
            match node[8] as i8 {
                0 => {
                    let recs = self.records(&node);
                    let pick = recs
                        .iter()
                        .rposition(|(k, _)| cmp(k) != Ordering::Greater)
                        .unwrap_or(0);
                    let Some((_, d)) = recs.get(pick) else {
                        return Ok(out);
                    };
                    n = be32(d, 0);
                }
                -1 => break,
                _ => return Err(invalid("unexpected B-tree node kind")),
            }
        }
        let mut guard = 0u32;
        loop {
            let node = self.node(tree, n)?;
            if node[8] as i8 != -1 {
                return Err(invalid("broken B-tree leaf chain"));
            }
            for (k, d) in self.records(&node) {
                if cmp(k) == Ordering::Less {
                    continue;
                }
                if !keep(k) {
                    return Ok(out);
                }
                out.push((k.to_vec(), d.to_vec()));
            }
            n = be32(&node, 0);
            guard += 1;
            if n == 0 || guard > 1 << 22 {
                return Ok(out);
            }
        }
    }

    /// Extents beyond a fork's first record, from the extents overflow tree.
    fn overflow(&self, file: u32, fork_type: u8, start_block: u32) -> Result<Vec<Extent>> {
        let plus = self.plus();
        let parse = move |k: &[u8]| -> (u32, u8, u32) {
            if plus {
                (be32(k, 4), k[2], be32(k, 8))
            } else {
                (be32(k, 2), k[1], u32::from(be16(k, 6)))
            }
        };
        // HFS and HFS+ both order extent keys by file ID, fork type, then start block.
        let target = (file, fork_type, start_block);
        let recs = self.scan(&self.extents.borrow(), &|k| parse(k).cmp(&target), &|k| {
            let (f, t, _) = parse(k);
            f == file && t == fork_type
        })?;
        Ok(recs.iter().flat_map(|(_, d)| extents_of(d, plus)).collect())
    }

    /// Parse a catalog key: (parent ID, stored name).
    fn cat_key(&self, k: &[u8]) -> (u32, Vec<u16>) {
        if self.plus() {
            let n = usize::from(be16(k, 6)).min((k.len().saturating_sub(8)) / 2);
            (be32(k, 2), (0..n).map(|i| be16(k, 8 + 2 * i)).collect())
        } else {
            let n = usize::from(k[6]).min(k.len().saturating_sub(7));
            (
                be32(k, 2),
                k[7..7 + n].iter().map(|&b| u16::from(b)).collect(),
            )
        }
    }

    /// How a stored name is shown.
    pub(crate) fn display(&self, units: &[u16]) -> String {
        if self.plus() {
            names::plus_display(units)
        } else {
            names::hfs_display(units)
        }
    }

    /// The stored form of a shown name.
    pub(crate) fn disk_name(&self, name: &str) -> std::result::Result<Vec<u16>, String> {
        if self.plus() {
            names::plus_disk(name)
        } else {
            names::hfs_disk(name, 31)
        }
    }

    /// The catalog's name order.
    pub(crate) fn order(&self) -> Order {
        match self.kind {
            Kind::Hfs => Order::Hfs,
            Kind::X if self.catalog.borrow().compare == 0xBC => Order::Binary,
            _ => Order::Folded,
        }
    }

    fn cat_rec(&self, d: &[u8]) -> Option<CatRec> {
        if self.plus() {
            if d.len() < 2 {
                return None;
            }
            match be16(d, 0) {
                1 if d.len() >= 88 => Some(CatRec::Folder {
                    id: be32(d, 8),
                    created: be32(d, 12),
                    modified: be32(d, 16),
                    accessed: be32(d, 24),
                    finder_flags: be16(d, 56),
                }),
                2 if d.len() >= 248 => {
                    let mut finfo = [0u8; 16];
                    finfo.copy_from_slice(&d[48..64]);
                    let mut fxinfo = [0u8; 16];
                    fxinfo.copy_from_slice(&d[64..80]);
                    let file_type: [u8; 4] = d[48..52].try_into().unwrap();
                    let creator: [u8; 4] = d[52..56].try_into().unwrap();
                    Some(CatRec::File(Box::new(FileRec {
                        id: be32(d, 8),
                        locked: be16(d, 2) & 1 != 0,
                        file_type,
                        creator,
                        finder_flags: be16(d, 56),
                        finfo,
                        fxinfo,
                        created: be32(d, 12),
                        modified: be32(d, 16),
                        accessed: be32(d, 24),
                        data: plus_fork(&d[88..168]),
                        rsrc: plus_fork(&d[168..248]),
                        link: (&file_type == b"hlnk" && &creator == b"hfs+").then(|| be32(d, 44)),
                    })))
                }
                3 | 4 if d.len() >= 10 => {
                    let n = usize::from(be16(d, 8)).min((d.len() - 10) / 2);
                    let units: Vec<u16> = (0..n).map(|i| be16(d, 10 + 2 * i)).collect();
                    Some(CatRec::Thread {
                        name: names::plus_display(&units),
                    })
                }
                _ => None,
            }
        } else {
            match d.first()? {
                1 if d.len() >= 70 => Some(CatRec::Folder {
                    id: be32(d, 6),
                    created: be32(d, 10),
                    modified: be32(d, 14),
                    accessed: 0,
                    finder_flags: be16(d, 30),
                }),
                2 if d.len() >= 102 => {
                    let mut finfo = [0u8; 16];
                    finfo.copy_from_slice(&d[4..20]);
                    let mut fxinfo = [0u8; 16];
                    fxinfo.copy_from_slice(&d[56..72]);
                    Some(CatRec::File(Box::new(FileRec {
                        id: be32(d, 20),
                        locked: d[2] & 1 != 0,
                        file_type: d[4..8].try_into().unwrap(),
                        creator: d[8..12].try_into().unwrap(),
                        finder_flags: be16(d, 12),
                        finfo,
                        fxinfo,
                        created: be32(d, 44),
                        modified: be32(d, 48),
                        accessed: 0,
                        data: ForkDesc {
                            size: u64::from(be32(d, 26)),
                            extents: extents_of(&d[74..86], false),
                        },
                        rsrc: ForkDesc {
                            size: u64::from(be32(d, 36)),
                            extents: extents_of(&d[86..98], false),
                        },
                        link: None,
                    })))
                }
                3 | 4 if d.len() >= 15 => {
                    let n = usize::from(d[14]).min(d.len() - 15);
                    Some(CatRec::Thread {
                        name: names::hfs_display(
                            &d[15..15 + n]
                                .iter()
                                .map(|&b| u16::from(b))
                                .collect::<Vec<_>>(),
                        ),
                    })
                }
                _ => None,
            }
        }
    }

    /// Orders a catalog key against "(parent, empty name)", the first key of a folder.
    fn first_of(&self, parent: u32) -> impl Fn(&[u8]) -> Ordering + '_ {
        move |k: &[u8]| {
            let p = be32(k, 2);
            let empty = if self.plus() {
                be16(k, 6) == 0
            } else {
                k[6] == 0
            };
            p.cmp(&parent).then(if empty {
                Ordering::Equal
            } else {
                Ordering::Greater
            })
        }
    }

    /// The thread record of a catalog node ID (its parent and name).
    fn thread(&self, id: u32) -> Result<Option<CatRec>> {
        let recs = self.scan(&self.catalog.borrow(), &self.first_of(id), &|k| {
            be32(k, 2) == id && self.first_of(id)(k) == Ordering::Equal
        })?;
        Ok(recs.first().and_then(|(_, d)| self.cat_rec(d)))
    }

    /// Everything in a folder, in catalog order.
    pub(crate) fn children(&self, folder: u32) -> Result<Vec<Child>> {
        Ok(self
            .all_children(folder)?
            .into_iter()
            .filter(|c| !(folder == ROOT_FOLDER && self.plus() && is_private(&c.key)))
            .collect())
    }

    /// Everything in a folder, the HFS+ private folders included.
    fn all_children(&self, folder: u32) -> Result<Vec<Child>> {
        let recs = self.scan(&self.catalog.borrow(), &self.first_of(folder), &|k| {
            be32(k, 2) == folder
        })?;
        Ok(recs
            .iter()
            .filter_map(|(k, d)| {
                let (_, key) = self.cat_key(k);
                match self.cat_rec(d)? {
                    CatRec::Thread { .. } => None,
                    rec => Some(Child {
                        name: self.display(&key),
                        key,
                        rec,
                    }),
                }
            })
            .collect())
    }

    /// Resolve a path to (folder ID or file record). `None` when it does not exist.
    pub(crate) fn lookup(&self, path: &str) -> Result<Option<Child>> {
        let mut cur = Child {
            name: String::new(),
            key: Vec::new(),
            rec: CatRec::Folder {
                id: ROOT_FOLDER,
                created: 0,
                modified: 0,
                accessed: 0,
                finder_flags: 0,
            },
        };
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let CatRec::Folder { id, .. } = cur.rec else {
                return Ok(None);
            };
            let Ok(want) = self.disk_name(part) else {
                return Ok(None);
            };
            let order = self.order();
            match self
                .children(id)?
                .into_iter()
                .find(|c| names::cmp(order, &c.key, &want) == Ordering::Equal)
            {
                Some(c) => cur = c,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// A hard link's target file record (the "iNode<n>" file in the private folder).
    fn resolve(&self, f: &FileRec) -> Result<FileRec> {
        let (Some(n), Some(dir)) = (f.link, self.private_dir.get()) else {
            return Ok(f.clone());
        };
        let name = format!("iNode{n}");
        for c in self.children(dir)? {
            if let (true, CatRec::File(t)) = (c.name == name, &c.rec) {
                return Ok((**t).clone());
            }
        }
        Ok(f.clone())
    }

    /// All extents of a fork (fork_type 0 data, 0xFF resource).
    pub(crate) fn fork_extents(&self, f: &FileRec, fork_type: u8) -> Result<(u64, Vec<Extent>)> {
        let d = if fork_type == 0 { &f.data } else { &f.rsrc };
        let mut ext = d.extents.clone();
        let have: u64 = ext.iter().map(|e| u64::from(e.count)).sum();
        if have * self.block < d.size {
            ext.extend(self.overflow(f.id, fork_type, have as u32)?);
        }
        Ok((d.size, ext))
    }

    /// Copy a fork to `out`.
    pub(crate) fn copy_fork(&self, f: &FileRec, fork_type: u8, out: &mut dyn Write) -> Result<u64> {
        let f = self.resolve(f)?;
        let (size, ext) = self.fork_extents(&f, fork_type)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut done = 0u64;
        while done < size {
            let n = (size - done).min(buf.len() as u64) as usize;
            self.read_extents(&ext, done, &mut buf[..n])?;
            out.write_all(&buf[..n])?;
            done += n as u64;
        }
        Ok(done)
    }

    pub(crate) fn info(&self) -> HfsInfo {
        HfsInfo {
            kind: self.kind,
            label: self.label.borrow().clone(),
            block_size: self.block as u32,
            total_blocks: self.total_blocks,
            free_blocks: self.free_blocks.get(),
            uuid: self.uuid,
            journaled: self.journaled,
            wrapped: self.wrapped,
        }
    }

    pub(crate) fn label(&self) -> String {
        self.label.borrow().clone()
    }

    /// Re-read the volume header and B-tree headers after a write.
    pub(crate) fn reload(&self) -> Result<()> {
        let fresh = HfsFs::open(self.w.borrow().reopen())?;
        self.free_blocks.set(fresh.free_blocks.get());
        *self.label.borrow_mut() = fresh.label.into_inner();
        *self.extents.borrow_mut() = fresh.extents.into_inner();
        *self.catalog.borrow_mut() = fresh.catalog.into_inner();
        self.private_dir.set(fresh.private_dir.get());
        Ok(())
    }

    pub(crate) fn entry(&self, parent: &str, c: &Child) -> Result<Entry> {
        let utc = self.plus();
        let mut mac = None;
        let (is_dir, size, created, modified, accessed, flags, locked) = match &c.rec {
            CatRec::Folder {
                created,
                modified,
                accessed,
                finder_flags,
                ..
            } => (
                true,
                0,
                *created,
                *modified,
                *accessed,
                *finder_flags,
                false,
            ),
            CatRec::File(f) => {
                let t = self.resolve(f)?;
                mac = Some(MacInfo {
                    file_type: mac_roman_to_string(&t.file_type),
                    creator: mac_roman_to_string(&t.creator),
                    resource_size: t.rsrc.size,
                });
                (
                    false,
                    t.data.size,
                    f.created,
                    f.modified,
                    f.accessed,
                    f.finder_flags,
                    f.locked,
                )
            }
            CatRec::Thread { .. } => unreachable!("threads are not children"),
        };
        Ok(Entry {
            path: join(parent, &c.name),
            short_name: c.name.clone(),
            name: c.name.clone(),
            is_dir,
            size,
            modified: mac_date(modified, utc),
            created: mac_date(created, utc),
            accessed: mac_date(accessed, utc)
                .split(' ')
                .next()
                .unwrap_or("")
                .to_string(),
            attributes: Attributes {
                read_only: locked,
                hidden: flags & FINDER_INVISIBLE != 0,
                system: false,
                archive: false,
            },
            mac,
        })
    }

    pub(crate) fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let c = self
            .lookup(dir)?
            .ok_or_else(|| Error::NotFound(display(dir)))?;
        let CatRec::Folder { id, .. } = c.rec else {
            return Err(Error::Invalid(format!("not a folder: {}", display(dir))));
        };
        let mut out = self
            .children(id)?
            .iter()
            .map(|c| self.entry(dir, c))
            .collect::<Result<Vec<_>>>()?;
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
        let (parent, _) = split_parent(path);
        match self.lookup(path)? {
            Some(c) => Ok(Some(self.entry(parent, &c)?)),
            None => Ok(None),
        }
    }

    pub(crate) fn file(&self, path: &str) -> Result<FileRec> {
        match self.lookup(path)?.map(|c| c.rec) {
            Some(CatRec::File(f)) => Ok(*f),
            Some(_) => Err(Error::Invalid(format!("{} is a folder", display(path)))),
            None => Err(Error::NotFound(display(path))),
        }
    }

    pub(crate) fn copy_out(&self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let f = self.file(path)?;
        self.copy_fork(&f, 0, out)
    }

    pub(crate) fn extract(
        &self,
        src: &str,
        dest: &Path,
        overwrite: bool,
        forks: ForkMode,
    ) -> Result<Stats> {
        fs::create_dir_all(dest)?;
        let mut stats = Stats::default();
        let c = self
            .lookup(src)?
            .ok_or_else(|| Error::NotFound(display(src)))?;
        match &c.rec {
            CatRec::Folder { id, .. } if src.is_empty() || c.name.is_empty() => {
                self.extract_dir(*id, dest, overwrite, forks, &mut stats)?
            }
            _ => self.extract_one(&c, dest, overwrite, forks, &mut stats)?,
        }
        Ok(stats)
    }

    fn extract_dir(
        &self,
        id: u32,
        dest: &Path,
        overwrite: bool,
        forks: ForkMode,
        stats: &mut Stats,
    ) -> Result<()> {
        for c in self.children(id)? {
            self.extract_one(&c, dest, overwrite, forks, stats)?;
        }
        Ok(())
    }

    fn extract_one(
        &self,
        c: &Child,
        dest: &Path,
        overwrite: bool,
        forks: ForkMode,
        stats: &mut Stats,
    ) -> Result<()> {
        let name = host_safe_name(&c.name);
        match &c.rec {
            CatRec::Folder { id, .. } => {
                let target = dest.join(&name);
                fs::create_dir_all(&target)?;
                stats.dirs += 1;
                self.extract_dir(*id, &target, overwrite, forks, stats)
            }
            CatRec::File(f) => {
                let f = self.resolve(f)?;
                let create = |p: &Path| -> Result<File> {
                    if !overwrite && p.exists() {
                        return Err(Error::Exists(p.display().to_string()));
                    }
                    Ok(File::create(p)?)
                };
                let stamp = |out: &File| {
                    if let Some(t) = mac_unix(f.modified, self.plus()) {
                        let _ = out.set_modified(UNIX_EPOCH + Duration::from_secs(t));
                    }
                };
                if forks == ForkMode::MacBinary {
                    let mut out = create(&dest.join(format!("{name}.bin")))?;
                    stats.bytes += self.write_macbinary(&c.name, &f, &mut out)?;
                    stats.files += 1;
                    stamp(&out);
                    return Ok(());
                }
                let mut out = create(&dest.join(&name))?;
                stats.bytes += self.copy_fork(&f, 0, &mut out)?;
                stats.files += 1;
                stamp(&out);
                if forks == ForkMode::AppleDouble
                    && (f.rsrc.size > 0 || f.finfo.iter().chain(&f.fxinfo).any(|&b| b != 0))
                {
                    let mut out = create(&dest.join(format!("._{name}")))?;
                    stats.bytes += self.write_appledouble(&f, &mut out)?;
                    stamp(&out);
                }
                Ok(())
            }
            CatRec::Thread { .. } => Ok(()),
        }
    }

    /// An AppleDouble header file (Apple's AppleSingle/AppleDouble format, version 2):
    /// Finder Info (entry 9, 32 bytes) and the resource fork (entry 2), as macOS writes
    /// `._name` files on disks that cannot hold forks.
    fn write_appledouble(&self, f: &FileRec, out: &mut dyn Write) -> Result<u64> {
        let header = 26 + 2 * 12;
        let mut h = vec![0u8; header + 32];
        put_be32(&mut h, 0, 0x0005_1607);
        put_be32(&mut h, 4, 0x0002_0000);
        put_be16(&mut h, 24, 2);
        for (i, (id, off, len)) in [
            (9u32, header as u32, 32u32),
            (2, (header + 32) as u32, f.rsrc.size as u32),
        ]
        .into_iter()
        .enumerate()
        {
            put_be32(&mut h, 26 + i * 12, id);
            put_be32(&mut h, 30 + i * 12, off);
            put_be32(&mut h, 34 + i * 12, len);
        }
        h[header..header + 16].copy_from_slice(&f.finfo);
        h[header + 16..header + 32].copy_from_slice(&f.fxinfo);
        out.write_all(&h)?;
        Ok(h.len() as u64 + self.copy_fork(f, 0xFF, out)?)
    }

    /// A MacBinary III file: a 128-byte header with the name, Finder information and
    /// dates, then the data and resource forks, each padded to 128 bytes.
    fn write_macbinary(&self, name: &str, f: &FileRec, out: &mut dyn Write) -> Result<u64> {
        let mut h = [0u8; 128];
        let mac_name = name.replace(':', "/");
        let mut n = string_to_mac_roman(&mac_name).unwrap_or_else(|| {
            name.chars()
                .map(|c| if c.is_ascii() { c as u8 } else { b'_' })
                .collect()
        });
        n.truncate(63);
        h[1] = n.len() as u8;
        h[2..2 + n.len()].copy_from_slice(&n);
        // FInfo: type, creator, flags (high byte here, low byte at 101), location, folder.
        h[65..73].copy_from_slice(&f.finfo[..8]);
        h[73] = f.finfo[8];
        h[75..81].copy_from_slice(&f.finfo[10..16]);
        h[81] = u8::from(f.locked);
        put_be32(&mut h, 83, f.data.size as u32);
        put_be32(&mut h, 87, f.rsrc.size as u32);
        // MacBinary dates are local time; HFS+ keeps UTC.
        let local = |s: u32| match (self.plus(), mac_unix(s, true)) {
            (true, Some(u)) => mac_time(UNIX_EPOCH + Duration::from_secs(u), false),
            _ => s,
        };
        put_be32(&mut h, 91, local(f.created));
        put_be32(&mut h, 95, local(f.modified));
        h[101] = f.finfo[9];
        h[102..106].copy_from_slice(b"mBIN");
        h[106] = f.fxinfo[8];
        h[107] = f.fxinfo[9];
        h[122] = 130;
        h[123] = 129;
        let crc = crc16_xmodem(&h[..124]);
        put_be16(&mut h, 124, crc);
        out.write_all(&h)?;
        let mut total = 128;
        for fork in [0u8, 0xFF] {
            let n = self.copy_fork(f, fork, out)?;
            let pad = (128 - n % 128) % 128;
            out.write_all(&vec![0u8; pad as usize])?;
            total += n + pad;
        }
        Ok(total)
    }
}

/// CRC-16/XMODEM (polynomial 0x1021, initial 0), as MacBinary II and III use.
pub(crate) fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

// --- formatting -----------------------------------------------------------------------------

/// Seconds since 1904 for "now": local wall-clock time (HFS) or UTC (HFS+).
pub(crate) fn mac_now(utc: bool) -> u32 {
    mac_time(SystemTime::now(), utc)
}

/// Seconds since 1904 for a time: local wall-clock time (HFS) or UTC (HFS+).
pub(crate) fn mac_time(t: SystemTime, utc: bool) -> u32 {
    let unix = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let secs = if utc {
        unix
    } else {
        DateTime::from_timestamp(unix, 0)
            .map(|d| d.with_timezone(&Local).naive_local().and_utc().timestamp())
            .unwrap_or(unix)
    };
    (secs + MAC_EPOCH).clamp(0, i64::from(u32::MAX)) as u32
}

/// A B-tree header node (node 0): header record, 128-byte user data record and the map
/// record, with `used` nodes marked in the map.
fn header_node(node_size: usize, total_nodes: u32, used: u32, hdr: &BTreeHeader) -> Vec<u8> {
    let mut n = vec![0u8; node_size];
    n[8] = 1; // header node
    put_be16(&mut n, 10, 3);
    let h = 14;
    put_be16(&mut n, h, hdr.depth);
    put_be32(&mut n, h + 2, hdr.root);
    put_be32(&mut n, h + 6, hdr.leaf_records);
    put_be32(&mut n, h + 10, hdr.first_leaf);
    put_be32(&mut n, h + 14, hdr.last_leaf);
    put_be16(&mut n, h + 18, node_size as u16);
    put_be16(&mut n, h + 20, hdr.max_key);
    put_be32(&mut n, h + 22, total_nodes);
    put_be32(&mut n, h + 26, total_nodes - used);
    // HFS+ fields (reserved, zero, on HFS).
    put_be32(&mut n, h + 32, hdr.clump);
    n[h + 37] = hdr.compare;
    put_be32(&mut n, h + 38, hdr.attributes);
    let map = 248;
    for i in 0..used as usize {
        n[map + i / 8] |= 0x80 >> (i % 8);
    }
    let end = node_size;
    put_be16(&mut n, end - 2, 14);
    put_be16(&mut n, end - 4, 120);
    put_be16(&mut n, end - 6, map as u16);
    put_be16(&mut n, end - 8, (end - 8) as u16);
    n
}

struct BTreeHeader {
    depth: u16,
    root: u32,
    leaf_records: u32,
    first_leaf: u32,
    last_leaf: u32,
    max_key: u16,
    clump: u32,
    compare: u8,
    attributes: u32,
}

/// A leaf node holding `recs` (already ordered key+data byte strings).
fn leaf_node(node_size: usize, recs: &[Vec<u8>]) -> Vec<u8> {
    let mut n = vec![0u8; node_size];
    n[8] = 0xFF; // leaf
    n[9] = 1; // height
    put_be16(&mut n, 10, recs.len() as u16);
    let mut at = 14;
    for (i, r) in recs.iter().enumerate() {
        put_be16(&mut n, node_size - 2 * (i + 1), at as u16);
        n[at..at + r.len()].copy_from_slice(r);
        at += r.len();
    }
    put_be16(&mut n, node_size - 2 * (recs.len() + 1), at as u16);
    n
}

/// Map records hold the node bitmap for this many nodes without extra map nodes.
fn map_capacity(node_size: usize) -> u32 {
    ((node_size - 256) * 8) as u32
}

fn write_bitmap(w: &mut Window, at: u64, bytes: usize, used: u64) -> Result<()> {
    let mut map = vec![0u8; bytes];
    for i in 0..used as usize {
        map[i / 8] |= 0x80 >> (i % 8);
    }
    w.write_at(at, &map)?;
    Ok(())
}

/// Format an empty HFS (Mac OS Standard) volume: boot blocks, Master Directory Block and
/// its copy in the next-to-last sector, the volume bitmap from sector 3, then the extents
/// and catalog B-trees; the catalog holds the root folder and its thread.
pub(crate) fn format_hfs(mut w: Window, label: &str) -> Result<()> {
    let sectors = w.len() / 512;
    if sectors < 800 {
        return Err(Error::Invalid("HFS volumes need at least 400 KB".into()));
    }
    let name = string_to_mac_roman(label)
        .filter(|n| !n.is_empty() && n.len() <= 27 && !n.contains(&b':'))
        .ok_or_else(|| {
            Error::Invalid("an HFS volume name is 1-27 Mac Roman characters without ':'".into())
        })?;
    // Allocation blocks: a multiple of 512 bytes, at most 65535 of them.
    let usable0 = sectors - 3 - 2;
    let block_sectors = usable0.div_ceil(65535).max(1);
    let blocks_est = usable0 / block_sectors;
    let bitmap_sectors = blocks_est.div_ceil(4096);
    let al_start = 3 + bitmap_sectors;
    let blocks = ((sectors - al_start - 2) / block_sectors) as u32;
    let block = block_sectors * 512;
    // B-tree files: 1/128 of the volume each for small disks, within 4 nodes .. map limit.
    let tree_nodes = |div: u64| -> u32 {
        (sectors * 512 / div / 512).clamp(4, u64::from(map_capacity(512))) as u32
    };
    let xt_nodes = tree_nodes(256);
    let ct_nodes = tree_nodes(128);
    let xt_blocks = (u64::from(xt_nodes) * 512).div_ceil(block) as u32;
    let ct_blocks = (u64::from(ct_nodes) * 512).div_ceil(block) as u32;
    let (xt_nodes, ct_nodes) = (
        (u64::from(xt_blocks) * block / 512) as u32,
        (u64::from(ct_blocks) * block / 512) as u32,
    );
    if xt_blocks + ct_blocks + 4 > blocks {
        return Err(Error::Invalid("volume is too small for HFS".into()));
    }
    let now = mac_now(false);
    // Zero the system area and both trees.
    w.write_at(0, &vec![0u8; (al_start * 512) as usize])?;
    let base = al_start * 512;
    let zeros = vec![0u8; ((xt_blocks + ct_blocks) as u64 * block) as usize];
    w.write_at(base, &zeros)?;
    write_bitmap(
        &mut w,
        3 * 512,
        (bitmap_sectors * 512) as usize,
        u64::from(xt_blocks + ct_blocks),
    )?;

    let empty = BTreeHeader {
        depth: 0,
        root: 0,
        leaf_records: 0,
        first_leaf: 0,
        last_leaf: 0,
        max_key: 7,
        clump: 0,
        compare: 0,
        attributes: 0,
    };
    w.write_at(base, &header_node(512, xt_nodes, 1, &empty))?;
    let cat = base + u64::from(xt_blocks) * block;
    w.write_at(
        cat,
        &header_node(
            512,
            ct_nodes,
            2,
            &BTreeHeader {
                depth: 1,
                root: 1,
                leaf_records: 2,
                first_leaf: 1,
                last_leaf: 1,
                max_key: 37,
                ..empty
            },
        ),
    )?;
    // Root folder record, keyed by (1, volume name); its thread, keyed by (2, "").
    let mut k1 = vec![0u8; (7 + name.len()).next_multiple_of(2)];
    k1[0] = (6 + name.len()) as u8;
    put_be32(&mut k1, 2, ROOT_PARENT);
    k1[6] = name.len() as u8;
    k1[7..7 + name.len()].copy_from_slice(&name);
    let mut dir = vec![0u8; 70];
    dir[0] = 1;
    put_be32(&mut dir, 6, ROOT_FOLDER);
    put_be32(&mut dir, 10, now);
    put_be32(&mut dir, 14, now);
    k1.extend(dir);
    let mut k2 = vec![0u8; 8];
    k2[0] = 6;
    put_be32(&mut k2, 2, ROOT_FOLDER);
    let mut thd = vec![0u8; 46];
    thd[0] = 3;
    put_be32(&mut thd, 10, ROOT_PARENT);
    thd[14] = name.len() as u8;
    thd[15..15 + name.len()].copy_from_slice(&name);
    k2.extend(thd);
    w.write_at(cat + 512, &leaf_node(512, &[k1, k2]))?;

    let mut mdb = vec![0u8; 512];
    put_be16(&mut mdb, 0, HFS_SIG);
    put_be32(&mut mdb, 2, now);
    put_be32(&mut mdb, 6, now);
    put_be16(&mut mdb, 10, 1 << 8); // cleanly unmounted
    put_be16(&mut mdb, 14, 3);
    put_be16(&mut mdb, 16, (xt_blocks + ct_blocks) as u16);
    put_be16(&mut mdb, 18, blocks as u16);
    put_be32(&mut mdb, 20, block as u32);
    put_be32(&mut mdb, 24, (block * 4) as u32);
    put_be16(&mut mdb, 28, al_start as u16);
    put_be32(&mut mdb, 30, 16);
    put_be16(&mut mdb, 34, (blocks - xt_blocks - ct_blocks) as u16);
    mdb[36] = name.len() as u8;
    mdb[37..37 + name.len()].copy_from_slice(&name);
    put_be32(&mut mdb, 74, xt_blocks * block as u32);
    put_be32(&mut mdb, 78, ct_blocks * block as u32);
    put_be32(&mut mdb, 130, xt_blocks * block as u32);
    put_be16(&mut mdb, 134, 0);
    put_be16(&mut mdb, 136, xt_blocks as u16);
    put_be32(&mut mdb, 146, ct_blocks * block as u32);
    put_be16(&mut mdb, 150, xt_blocks as u16);
    put_be16(&mut mdb, 152, ct_blocks as u16);
    w.write_at(1024, &mdb)?;
    w.write_at((sectors - 2) * 512, &mdb)?;
    w.write_at((sectors - 1) * 512, &[0u8; 512])?;
    Ok(())
}

/// Format an empty HFS+ volume (no journal): volume header and its copy 1024 bytes before
/// the end, allocation bitmap file, extents and catalog B-trees with 4 KiB nodes.
pub(crate) fn format_hfsplus(mut w: Window, label: &str) -> Result<()> {
    let size = w.len();
    let block: u64 = if size >= 32 << 20 { 4096 } else { 512 };
    let total = (size / block) as u32;
    if total < 64 {
        return Err(Error::Invalid("volume is too small for HFS+".into()));
    }
    let name = names::plus_disk(label)
        .map_err(|e| Error::Invalid(e.replace("HFS+ name", "HFS+ volume name")))?;
    let node = 4096usize;
    let bitmap_bytes = u64::from(total).div_ceil(8);
    let bitmap_blocks = bitmap_bytes.div_ceil(block) as u32;
    let nodes_for = |div: u64| -> u32 {
        (size / div / node as u64).clamp(4, u64::from(map_capacity(node))) as u32
    };
    let xt_nodes = nodes_for(512);
    let ct_nodes = nodes_for(128);
    let blocks_for = |nodes: u32| (u64::from(nodes) * node as u64).div_ceil(block) as u32;
    let (xt_blocks, ct_blocks) = (blocks_for(xt_nodes), blocks_for(ct_nodes));
    // Block 0 (or the first 1.5 KiB) holds the boot blocks and the volume header.
    let first = (1536u64.div_ceil(block)) as u32;
    let bm_start = first;
    let xt_start = bm_start + bitmap_blocks;
    let ct_start = xt_start + xt_blocks;
    let used_front = ct_start + ct_blocks;
    // The last 1 KiB (alternate header and a reserved sector) is allocated too.
    let tail = (1024u64.div_ceil(block)) as u32;
    if used_front + tail + 8 > total {
        return Err(Error::Invalid("volume is too small for HFS+".into()));
    }
    let free = total - used_front - tail;
    let now_local = mac_now(false);
    let now = mac_now(true);

    w.write_at(0, &vec![0u8; (u64::from(used_front) * block) as usize])?;
    let mut bitmap = vec![0u8; (u64::from(bitmap_blocks) * block) as usize];
    for i in (0..used_front).chain(total - tail..total) {
        bitmap[i as usize / 8] |= 0x80 >> (i % 8);
    }
    w.write_at(u64::from(bm_start) * block, &bitmap)?;
    let xt_nodes = (u64::from(xt_blocks) * block / node as u64) as u32;
    let ct_nodes = (u64::from(ct_blocks) * block / node as u64) as u32;
    let empty = BTreeHeader {
        depth: 0,
        root: 0,
        leaf_records: 0,
        first_leaf: 0,
        last_leaf: 0,
        max_key: 10,
        clump: (u64::from(xt_blocks) * block) as u32,
        compare: 0,
        attributes: 2, // big keys
    };
    w.write_at(
        u64::from(xt_start) * block,
        &header_node(node, xt_nodes, 1, &empty),
    )?;
    let cat = u64::from(ct_start) * block;
    w.write_at(
        cat,
        &header_node(
            node,
            ct_nodes,
            2,
            &BTreeHeader {
                depth: 1,
                root: 1,
                leaf_records: 2,
                first_leaf: 1,
                last_leaf: 1,
                max_key: 516,
                clump: (u64::from(ct_blocks) * block) as u32,
                compare: 0,
                attributes: 2 | 4, // big keys, variable-length index keys
            },
        ),
    )?;
    let uni = |v: &mut Vec<u8>| {
        v.extend((name.len() as u16).to_be_bytes());
        for u in &name {
            v.extend(u.to_be_bytes());
        }
    };
    let mut k1 = Vec::new();
    k1.extend(((6 + 2 * name.len()) as u16).to_be_bytes());
    k1.extend(ROOT_PARENT.to_be_bytes());
    uni(&mut k1);
    let mut dir = vec![0u8; 88];
    put_be16(&mut dir, 0, 1);
    put_be32(&mut dir, 8, ROOT_FOLDER);
    for o in [12, 16, 20, 24] {
        put_be32(&mut dir, o, now);
    }
    put_be16(&mut dir, 42, 0o040755);
    k1.extend(dir);
    let mut k2 = Vec::new();
    k2.extend(6u16.to_be_bytes());
    k2.extend(ROOT_FOLDER.to_be_bytes());
    k2.extend(0u16.to_be_bytes());
    let mut thd = vec![0u8; 8];
    put_be16(&mut thd, 0, 3);
    put_be32(&mut thd, 4, ROOT_PARENT);
    thd.truncate(8);
    uni(&mut thd);
    k2.extend(thd);
    w.write_at(cat + node as u64, &leaf_node(node, &[k1, k2]))?;

    let mut vh = vec![0u8; 512];
    put_be16(&mut vh, 0, HFSPLUS_SIG);
    put_be16(&mut vh, 2, 4);
    put_be32(&mut vh, 4, 1 << 8); // cleanly unmounted
    vh[8..12].copy_from_slice(b"10.0");
    put_be32(&mut vh, 16, now_local);
    put_be32(&mut vh, 20, now);
    put_be32(&mut vh, 28, now);
    put_be32(&mut vh, 40, block as u32);
    put_be32(&mut vh, 44, total);
    put_be32(&mut vh, 48, free);
    put_be32(&mut vh, 52, used_front);
    put_be32(&mut vh, 56, 65536);
    put_be32(&mut vh, 60, 65536);
    put_be32(&mut vh, 64, 16);
    put_be64(&mut vh, 72, 1); // Mac Roman
    let uuid = crate::util::new_guid();
    vh[104..112].copy_from_slice(&uuid[..8]);
    let fork = |vh: &mut Vec<u8>, at: usize, start: u32, blocks: u32| {
        put_be64(vh, at, u64::from(blocks) * block);
        put_be32(vh, at + 8, (u64::from(blocks) * block) as u32);
        put_be32(vh, at + 12, blocks);
        put_be32(vh, at + 16, start);
        put_be32(vh, at + 20, blocks);
    };
    fork(&mut vh, 112, bm_start, bitmap_blocks);
    fork(&mut vh, 192, xt_start, xt_blocks);
    fork(&mut vh, 272, ct_start, ct_blocks);
    w.write_at(1024, &vh)?;
    w.write_at(size - 1024, &vh)?;
    w.write_at(size - 512, &[0u8; 512])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_roman_round_trip() {
        let all: Vec<u8> = (0x20..=0xFF).filter(|&b| b != 0x7F).collect();
        let s = mac_roman_to_string(&all);
        assert_eq!(string_to_mac_roman(&s).unwrap(), all);
        assert_eq!(mac_roman_to_string(b"caf\x8E"), "café");
        assert_eq!(string_to_mac_roman("日本"), None);
    }

    #[test]
    fn macbinary_crc() {
        // CRC-16/XMODEM check value.
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
    }

    #[test]
    fn names_and_dates() {
        // HFS dates are local wall-clock time, shown as stored.
        let secs = (978_494_706 + MAC_EPOCH) as u32;
        assert_eq!(mac_date(secs, false), "2001-01-03 04:05:06");
    }
}
