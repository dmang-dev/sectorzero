//! Writing to HFS and HFS+ volumes.
//!
//! A change loads the catalog and extents overflow B-trees into memory, edits them there
//! and writes each tree back packed: header node, map nodes, leaves, then the index levels.
//! File data goes straight into newly allocated blocks; blocks freed by the change are
//! reused only when the volume would otherwise be full, so an interrupted change leaves the
//! old files intact. The allocation bitmap and both copies of the volume header are written
//! last. When files with extended attributes go away, the HFS+ attributes tree is rewritten
//! without their records.
//!
//! Journaled HFS+ and HFS+ inside an HFS wrapper are not written. New files and folders
//! get thread records (as the Linux driver and Mac OS X create them) and no BSD
//! permissions (as Mac OS 9 creates them: systems then apply their defaults).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;

use super::names::{self, Order};
use super::*;
use crate::fatraw::EntryEdit;

const ALLOCATION_FILE: u32 = 6;
const STARTUP_FILE: u32 = 7;
const ATTRIBUTES_FILE: u32 = 8;

const FOLDER: u8 = 1;
const FILE: u8 = 2;
const FOLDER_THREAD: u8 = 3;
const FILE_THREAD: u8 = 4;

/// HFS file flags: record in use, thread record exists (as the Linux driver writes them).
const HFS_FILE_FLAGS: u8 = 0x82;
/// HFS+ file flags: thread record exists.
const PLUS_THREAD_EXISTS: u16 = 0x0002;
const LOCKED: u16 = 0x0001;
/// HFS+ folders that also count their subfolders (at byte 84).
const HAS_FOLDER_COUNT: u16 = 0x0010;
/// BSD owner flag "immutable", which Mac OS X shows as locked.
const UF_IMMUTABLE: u8 = 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ExtKey {
    file: u32,
    fork: u8,
    start: u32,
}

#[derive(Debug, Clone)]
struct CatVal {
    /// The stored name (Mac Roman bytes as u16 on HFS).
    name: Vec<u16>,
    data: Vec<u8>,
}

/// A catalog key in catalog order: parent ID, then the name's sort key.
type CatKey = (u32, Vec<u16>);

/// A B-tree file and the header fields that are kept when it is rewritten.
struct Tree {
    node_size: usize,
    /// All extents of the tree file.
    fork: Vec<Extent>,
    max_key: u16,
    /// Header record clump size (HFS+; reserved on HFS).
    hdr_clump: u32,
    /// How much to grow the file by at least.
    grow: u32,
    btype: u8,
    compare: u8,
    attributes: u32,
    /// The 128-byte user data record.
    user: Vec<u8>,
    /// Nodes 0..old_used may hold data from before the change.
    old_used: u32,
}

impl Tree {
    fn blocks(&self) -> u32 {
        self.fork.iter().map(|e| e.count).sum()
    }

    fn total_nodes(&self, block: u64) -> u32 {
        (u64::from(self.blocks()) * block / self.node_size as u64) as u32
    }

    fn big_keys(&self) -> bool {
        self.attributes & 2 != 0
    }

    /// Index keys: `None` when variable-length, else their fixed size.
    fn index_key_len(&self) -> Option<usize> {
        if self.attributes & 4 != 0 {
            None
        } else if self.big_keys() {
            Some(usize::from(self.max_key) + 2)
        } else {
            Some(usize::from(self.max_key) + 1)
        }
    }

    fn map_nodes(&self, total: u32) -> u32 {
        let header = ((self.node_size - 256) * 8) as u32;
        let per_node = ((self.node_size - 20) * 8) as u32;
        if total <= header {
            0
        } else {
            (total - header).div_ceil(per_node)
        }
    }
}

/// A leaf record: key followed by data.
struct Rec {
    bytes: Vec<u8>,
    key_len: usize,
}

/// A B-tree's shape: which records go in which leaf, and the index levels above them.
struct Plan {
    leaves: Vec<Range<usize>>,
    /// Bottom-up; each level groups the nodes of the level below.
    index: Vec<Vec<Range<usize>>>,
    map_nodes: u32,
}

impl Plan {
    fn nodes(&self) -> u32 {
        1 + self.map_nodes
            + self.leaves.len() as u32
            + self.index.iter().map(|l| l.len() as u32).sum::<u32>()
    }
}

/// Group consecutive records into nodes, each as full as it can be.
fn pack(node_size: usize, sizes: impl Iterator<Item = usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut used, mut n, mut i) = (0, 0, 0, 0);
    for s in sizes {
        // Descriptor, records, and one offset per record plus the free-space offset.
        if n > 0 && 14 + used + s + 2 * (n + 2) > node_size {
            out.push(start..i);
            start = i;
            used = 0;
            n = 0;
        }
        used += s;
        n += 1;
        i += 1;
    }
    if n > 0 {
        out.push(start..i);
    }
    out
}

/// A node holding `recs`.
fn make_node(
    node_size: usize,
    flink: u32,
    blink: u32,
    kind: u8,
    height: u8,
    recs: &[Vec<u8>],
) -> Vec<u8> {
    let mut n = vec![0u8; node_size];
    put_be32(&mut n, 0, flink);
    put_be32(&mut n, 4, blink);
    n[8] = kind;
    n[9] = height;
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

/// Join extents that continue one another.
fn merge(ext: &[Extent], max_count: u32) -> Vec<Extent> {
    let mut out: Vec<Extent> = Vec::with_capacity(ext.len());
    for &e in ext.iter().filter(|e| e.count > 0) {
        match out.last_mut() {
            Some(l)
                if l.start + l.count == e.start
                    && l.count.checked_add(e.count).is_some_and(|n| n <= max_count) =>
            {
                l.count += e.count
            }
            _ => out.push(e),
        }
    }
    out
}

fn kind_of(plus: bool, d: &[u8]) -> u8 {
    if plus { be16(d, 0) as u8 } else { d[0] }
}

fn id_of(plus: bool, d: &[u8]) -> u32 {
    match (plus, kind_of(plus, d)) {
        (true, _) => be32(d, 8),
        (false, FOLDER) => be32(d, 6),
        (false, _) => be32(d, 20),
    }
}

/// Leaf records as (key, data).
type Leaves = Vec<(Vec<u8>, Vec<u8>)>;

/// Read a tree's leaf records in order, checking the chain and the record count.
fn load_tree(fs: &HfsFs, bt: &BTree) -> Result<(Tree, Leaves)> {
    let head = fs.node(bt, 0)?;
    let ns = bt.node_size;
    let h = &head[14..];
    let total = be32(h, 22);
    let have =
        (bt.fork.iter().map(|e| u64::from(e.count)).sum::<u64>() * fs.block / ns as u64) as u32;
    if total > have {
        return Err(invalid("a B-tree file is shorter than its header says"));
    }
    let mut recs = Vec::new();
    let (mut n, mut prev, mut hops) = (be32(h, 10), 0u32, 0u32);
    while n != 0 {
        if n >= total || hops >= total {
            return Err(invalid("broken B-tree leaf chain"));
        }
        let node = fs.node(bt, n)?;
        if node[8] != 0xFF || be32(&node, 4) != prev {
            return Err(invalid("broken B-tree leaf chain"));
        }
        let rs = fs.records(&node);
        if rs.len() != usize::from(be16(&node, 10)) {
            return Err(invalid("damaged B-tree node"));
        }
        recs.extend(rs.into_iter().map(|(k, d)| (k.to_vec(), d.to_vec())));
        prev = n;
        n = be32(&node, 0);
        hops += 1;
    }
    if recs.len() as u64 != u64::from(be32(h, 6)) {
        return Err(invalid("B-tree record count does not match its header"));
    }
    let old_used = used_nodes(fs, bt, &head, total)?;
    Ok((
        Tree {
            node_size: ns,
            fork: bt.fork.clone(),
            max_key: be16(h, 20),
            hdr_clump: be32(h, 32),
            grow: be32(h, 32),
            btype: h[36],
            compare: h[37],
            attributes: be32(h, 38),
            user: head[120..248].to_vec(),
            old_used,
        },
        recs,
    ))
}

/// One past the highest node the tree's map marks used.
fn used_nodes(fs: &HfsFs, bt: &BTree, head: &[u8], total: u32) -> Result<u32> {
    let ns = bt.node_size;
    let (mut high, mut base) = (0u32, 0u32);
    let mut node = head.to_vec();
    let mut rec = 2;
    for _ in 0..=total {
        let start = usize::from(be16(&node, ns - 2 * (rec + 1)));
        let end = usize::from(be16(&node, ns - 2 * (rec + 2)));
        if start > end || end > ns {
            break;
        }
        for (i, &b) in node[start..end].iter().enumerate() {
            if b != 0 {
                high = base + (i as u32) * 8 + (8 - b.trailing_zeros());
            }
        }
        base += ((end - start) * 8) as u32;
        let next = be32(&node, 0);
        if next == 0 || next >= total {
            break;
        }
        node = fs.node(bt, next)?;
        rec = 0;
    }
    Ok(high.min(total))
}

/// A file to create.
pub(super) struct NewFile<'r> {
    pub data: &'r mut dyn Read,
    pub data_len: u64,
    pub rsrc: Vec<u8>,
    pub finfo: [u8; 16],
    pub fxinfo: [u8; 16],
    pub created: u32,
    pub modified: u32,
    pub accessed: u32,
    pub locked: bool,
}

/// A parsed `EntryEdit` (dates in the volume's convention).
pub(super) struct Ed {
    read_only: Option<bool>,
    hidden: Option<bool>,
    created: Option<u32>,
    modified: Option<u32>,
    accessed: Option<u32>,
}

#[derive(Clone, Copy)]
enum Which {
    Cat,
    Ext,
    Attr,
}

pub(super) struct Writer<'a> {
    fs: &'a HfsFs,
    plus: bool,
    order: Order,
    block: u64,
    total: u32,
    /// Where the volume header (or MDB) starts, minus 1024.
    at: u64,
    vh: Vec<u8>,
    cat: Tree,
    ext: Tree,
    attr: Option<Tree>,
    catalog: BTreeMap<CatKey, CatVal>,
    extents: BTreeMap<ExtKey, Vec<Extent>>,
    bitmap: Vec<u8>,
    bitmap_fork: Vec<Extent>,
    free: u32,
    /// Freed by this change; reused only when nothing else is left.
    pending: Vec<Extent>,
    hint: u32,
    next_id: u32,
    now: u32,
    /// Folders whose contents changed (their modification dates are updated).
    touched: BTreeSet<u32>,
    /// Catalog node IDs removed (their extended attributes go too).
    gone: BTreeSet<u32>,
    new_label: Option<Vec<u16>>,
    dirty: bool,
}

impl<'a> Writer<'a> {
    pub(super) fn load(fs: &'a HfsFs) -> Result<Writer<'a>> {
        let plus = fs.plus();
        let at = if plus { fs.base } else { 0 };
        let mut vh = vec![0u8; 512];
        fs.w.borrow_mut().read_at(at + 1024, &mut vh)?;
        let (mut cat, cat_recs) = load_tree(fs, &fs.catalog.borrow())?;
        let (mut ext, ext_recs) = load_tree(fs, &fs.extents.borrow())?;
        if !plus {
            ext.grow = be32(&vh, 74);
            cat.grow = be32(&vh, 78);
        }
        let order = fs.order();
        let mut catalog = BTreeMap::new();
        let mut prev: Option<CatKey> = None;
        for (k, d) in cat_recs {
            let (parent, name) = fs.cat_key(&k);
            let key = (parent, names::sort_key(order, &name));
            let min = match (plus, d.len() >= 2, d.first().map(|_| kind_of(plus, &d))) {
                (_, false, _) => usize::MAX,
                (true, _, Some(FOLDER)) => 88,
                (true, _, Some(FILE)) => 248,
                (true, _, Some(FOLDER_THREAD | FILE_THREAD)) => 10,
                (false, _, Some(FOLDER)) => 70,
                (false, _, Some(FILE)) => 102,
                (false, _, Some(FOLDER_THREAD | FILE_THREAD)) => 46,
                _ => usize::MAX,
            };
            if d.len() < min {
                return Err(invalid("damaged catalog record"));
            }
            if prev.as_ref().is_some_and(|p| *p >= key) {
                return Err(Error::Unsupported(
                    "writing to this volume: its catalog does not keep names in the order SectorZero compares them in"
                        .into(),
                ));
            }
            prev = Some(key.clone());
            catalog.insert(key, CatVal { name, data: d });
        }
        let mut extents = BTreeMap::new();
        let mut prev: Option<ExtKey> = None;
        for (k, d) in ext_recs {
            let key = if plus {
                ExtKey {
                    file: be32(&k, 4),
                    fork: k[2],
                    start: be32(&k, 8),
                }
            } else {
                ExtKey {
                    file: be32(&k, 2),
                    fork: k[1],
                    start: u32::from(be16(&k, 6)),
                }
            };
            if prev.is_some_and(|p| p >= key) {
                return Err(invalid("extents overflow records out of order"));
            }
            prev = Some(key);
            extents.insert(key, extents_of(&d, plus));
        }
        let total = fs.total_blocks;
        let bytes = (total as usize).div_ceil(8);
        let mut bitmap = vec![0u8; bytes];
        let mut bitmap_fork = Vec::new();
        if plus {
            let f = plus_fork(&vh[112..192]);
            let mut e = f.extents;
            let have: u64 = e.iter().map(|x| u64::from(x.count)).sum();
            if have * fs.block < bytes as u64 {
                e.extend(fs.overflow(ALLOCATION_FILE, 0, have as u32)?);
            }
            fs.read_extents(&e, 0, &mut bitmap)?;
            bitmap_fork = e;
        } else {
            fs.w.borrow_mut()
                .read_at(u64::from(be16(&vh, 14)) * 512, &mut bitmap)?;
        }
        let mut w = Writer {
            fs,
            plus,
            order,
            block: fs.block,
            total,
            at,
            cat,
            ext,
            attr: None,
            catalog,
            extents,
            bitmap,
            bitmap_fork,
            free: 0,
            pending: Vec::new(),
            hint: if plus {
                be32(&vh, 52)
            } else {
                u32::from(be16(&vh, 16))
            },
            next_id: if plus { be32(&vh, 64) } else { be32(&vh, 30) },
            now: mac_now(plus),
            vh,
            touched: BTreeSet::new(),
            gone: BTreeSet::new(),
            new_label: None,
            dirty: false,
        };
        if w.hint >= total {
            w.hint = 0;
        }
        w.free = total - (0..total).filter(|&b| w.bit(b)).count() as u32;
        // Never hand out an ID in use, whatever the header says (the Linux hfs driver can
        // leave its next catalog ID behind the IDs it has given out).
        let in_use = w
            .catalog
            .iter()
            .filter(|(k, _)| !k.1.is_empty())
            .map(|(_, v)| w.id(&v.data))
            .max()
            .unwrap_or(0);
        w.next_id = w.next_id.max(in_use.saturating_add(1)).max(16);
        if w.thread_of(ROOT_FOLDER).is_none() {
            return Err(invalid("the root folder has no thread record"));
        }
        Ok(w)
    }

    // --- catalog helpers ------------------------------------------------------------

    fn kind(&self, d: &[u8]) -> u8 {
        kind_of(self.plus, d)
    }

    fn id(&self, d: &[u8]) -> u32 {
        id_of(self.plus, d)
    }

    fn key(&self, parent: u32, name: &[u16]) -> CatKey {
        (parent, names::sort_key(self.order, name))
    }

    /// The parent and name a thread record gives for `id`.
    fn thread_of(&self, id: u32) -> Option<(u32, Vec<u16>)> {
        let d = &self.catalog.get(&(id, Vec::new()))?.data;
        if self.plus {
            let n = usize::from(be16(d, 8)).min(d.len().saturating_sub(10) / 2);
            Some((be32(d, 4), (0..n).map(|i| be16(d, 10 + 2 * i)).collect()))
        } else {
            let n = usize::from(d[14]).min(31).min(d.len().saturating_sub(15));
            Some((
                be32(d, 10),
                d[15..15 + n].iter().map(|&b| u16::from(b)).collect(),
            ))
        }
    }

    fn thread_data(&self, kind: u8, parent: u32, name: &[u16]) -> Vec<u8> {
        if self.plus {
            let mut d = vec![0u8; 10 + 2 * name.len()];
            put_be16(&mut d, 0, u16::from(kind));
            put_be32(&mut d, 4, parent);
            put_be16(&mut d, 8, name.len() as u16);
            for (i, &u) in name.iter().enumerate() {
                put_be16(&mut d, 10 + 2 * i, u);
            }
            d
        } else {
            let mut d = vec![0u8; 46];
            d[0] = kind;
            put_be32(&mut d, 10, parent);
            d[14] = name.len() as u8;
            for (i, &u) in name.iter().enumerate() {
                d[15 + i] = u as u8;
            }
            d
        }
    }

    /// The stored form of a new name, or why it cannot be stored.
    fn name_of(&self, name: &str) -> Result<Vec<u16>> {
        let u = self.fs.disk_name(name).map_err(Error::Invalid)?;
        if u.contains(&0) || names::sort_key(self.order, &u).is_empty() {
            return Err(Error::Invalid(format!(
                "{name:?} cannot be stored as an HFS name"
            )));
        }
        Ok(u)
    }

    fn shown(&self, name: &[u16]) -> String {
        self.fs.display(name)
    }

    fn root_key(&self) -> CatKey {
        let (p, n) = self
            .thread_of(ROOT_FOLDER)
            .unwrap_or((ROOT_PARENT, Vec::new()));
        self.key(p, &n)
    }

    /// The catalog key of `path` ("" is the root folder).
    fn find(&self, path: &str) -> Option<CatKey> {
        let mut key = self.root_key();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let d = &self.catalog.get(&key)?.data;
            if self.kind(d) != FOLDER {
                return None;
            }
            let id = self.id(d);
            let name = self.fs.disk_name(part).ok()?;
            if id == ROOT_FOLDER && self.plus && is_private(&name) {
                return None;
            }
            key = self.key(id, &name);
        }
        self.catalog.contains_key(&key).then_some(key)
    }

    fn folder_id(&self, path: &str) -> Result<u32> {
        match self.find(path).map(|k| &self.catalog[&k].data) {
            Some(d) if self.kind(d) == FOLDER => Ok(self.id(d)),
            Some(_) => Err(Error::Invalid(format!("not a folder: {}", display(path)))),
            None => Err(Error::NotFound(display(path))),
        }
    }

    fn children(&self, id: u32) -> Vec<CatKey> {
        self.catalog
            .range((id, Vec::new())..(id + 1, Vec::new()))
            .filter(|(k, _)| !k.1.is_empty())
            .map(|(k, _)| k.clone())
            .collect()
    }

    fn take_id(&mut self) -> Result<u32> {
        let id = self.next_id.max(16);
        if id == u32::MAX {
            return Err(Error::Invalid(
                "the volume has run out of catalog node IDs".into(),
            ));
        }
        self.next_id = id + 1;
        Ok(id)
    }

    fn insert(&mut self, parent: u32, name: Vec<u16>, data: Vec<u8>) {
        let key = self.key(parent, &name);
        self.catalog.insert(key, CatVal { name, data });
        self.touched.insert(parent);
        self.dirty = true;
    }

    fn insert_thread(&mut self, id: u32, kind: u8, parent: u32, name: &[u16]) {
        let data = self.thread_data(kind, parent, name);
        self.catalog.insert(
            (id, Vec::new()),
            CatVal {
                name: Vec::new(),
                data,
            },
        );
    }

    // --- allocation -----------------------------------------------------------------

    fn bit(&self, b: u32) -> bool {
        self.bitmap[(b / 8) as usize] & (0x80 >> (b % 8)) != 0
    }

    fn mark(&mut self, e: Extent, used: bool) {
        for b in e.start..e.start + e.count {
            let (i, m) = ((b / 8) as usize, 0x80u8 >> (b % 8));
            if (self.bitmap[i] & m != 0) != used {
                self.bitmap[i] ^= m;
                if used {
                    self.free -= 1;
                } else {
                    self.free += 1;
                }
            }
        }
        self.dirty = true;
    }

    fn commit_frees(&mut self) {
        for e in std::mem::take(&mut self.pending) {
            self.mark(e, false);
        }
    }

    fn release(&mut self, ext: &[Extent]) {
        self.pending.extend(ext.iter().filter(|e| e.count > 0));
        self.dirty = true;
    }

    fn pending_blocks(&self) -> u64 {
        self.pending.iter().map(|e| u64::from(e.count)).sum()
    }

    fn no_space(&self) -> Error {
        Error::Invalid("not enough free space in the image".into())
    }

    /// `n` blocks: one run if there is one (searching from the allocation hint), else
    /// the free runs in order.
    fn alloc(&mut self, n: u32) -> Result<Vec<Extent>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        if n > self.free {
            self.commit_frees();
        }
        if n > self.free {
            return Err(self.no_space());
        }
        let mut runs: Vec<Extent> = Vec::new();
        let mut found = None;
        'scan: for (lo, hi) in [(self.hint, self.total), (0, self.hint)] {
            let mut b = lo;
            while b < hi {
                if b % 8 == 0 && b + 8 <= hi && self.bitmap[(b / 8) as usize] == 0xFF {
                    b += 8;
                    continue;
                }
                if self.bit(b) {
                    b += 1;
                    continue;
                }
                let s = b;
                while b < hi && !self.bit(b) {
                    b += 1;
                }
                if b - s >= n {
                    found = Some(Extent { start: s, count: n });
                    break 'scan;
                }
                runs.push(Extent {
                    start: s,
                    count: b - s,
                });
            }
        }
        let out = match found {
            Some(e) => vec![e],
            None => {
                let mut left = n;
                let mut out = Vec::new();
                for r in runs {
                    let take = r.count.min(left);
                    out.push(Extent {
                        start: r.start,
                        count: take,
                    });
                    left -= take;
                    if left == 0 {
                        break;
                    }
                }
                if left > 0 {
                    return Err(self.no_space());
                }
                out
            }
        };
        for &e in &out {
            self.mark(e, true);
        }
        let last = out[out.len() - 1];
        self.hint = (last.start + last.count) % self.total;
        Ok(out)
    }

    /// `n` blocks, right after block `after` when they are free there.
    fn alloc_after(&mut self, after: u32, n: u32) -> Result<Vec<Extent>> {
        if after.checked_add(n).is_some_and(|end| end <= self.total)
            && (after..after + n).all(|b| !self.bit(b))
        {
            let e = Extent {
                start: after,
                count: n,
            };
            self.mark(e, true);
            return Ok(vec![e]);
        }
        self.alloc(n)
    }

    fn write_extents(&self, ext: &[Extent], mut offset: u64, mut data: &[u8]) -> Result<()> {
        let mut w = self.fs.w.borrow_mut();
        for e in ext {
            let len = u64::from(e.count) * self.block;
            if offset >= len {
                offset -= len;
                continue;
            }
            let n = (len - offset).min(data.len() as u64) as usize;
            w.write_at(
                self.fs.base + u64::from(e.start) * self.block + offset,
                &data[..n],
            )?;
            data = &data[n..];
            offset = 0;
            if data.is_empty() {
                return Ok(());
            }
        }
        if data.is_empty() {
            Ok(())
        } else {
            Err(invalid("write past the end of a fork"))
        }
    }

    /// Copy `len` bytes from `src` into `ext`, zero-filling the rest of the last block.
    fn fill(&self, ext: &[Extent], src: &mut dyn Read, len: u64) -> Result<()> {
        let mut buf = vec![0u8; 1 << 20];
        let mut w = self.fs.w.borrow_mut();
        let mut left = len;
        for e in ext {
            let mut at = self.fs.base + u64::from(e.start) * self.block;
            let mut room = u64::from(e.count) * self.block;
            while room > 0 {
                let n = room.min(buf.len() as u64) as usize;
                let take = left.min(n as u64) as usize;
                src.read_exact(&mut buf[..take])?;
                buf[take..n].fill(0);
                w.write_at(at, &buf[..n])?;
                left -= take as u64;
                at += n as u64;
                room -= n as u64;
            }
        }
        Ok(())
    }

    fn max_extent(&self) -> u32 {
        if self.plus { u32::MAX } else { 0xFFFF }
    }

    /// Remove a fork's overflow extent records, returning their extents.
    fn drop_overflow(&mut self, file: u32, fork: u8) -> Vec<Extent> {
        let keys: Vec<ExtKey> = self
            .extents
            .range(
                ExtKey {
                    file,
                    fork,
                    start: 0,
                }..=ExtKey {
                    file,
                    fork,
                    start: u32::MAX,
                },
            )
            .map(|(k, _)| *k)
            .collect();
        if !keys.is_empty() {
            self.dirty = true;
        }
        keys.iter()
            .flat_map(|k| self.extents.remove(k).unwrap_or_default())
            .collect()
    }

    /// Lay out a fork: its first 3 (HFS) or 8 (HFS+) extents are returned for the record,
    /// the rest go into overflow records.
    fn place(&mut self, file: u32, fork: u8, ext: &[Extent]) -> Vec<Extent> {
        self.drop_overflow(file, fork);
        let n = if self.plus { 8 } else { 3 };
        let ext = merge(ext, self.max_extent());
        let head = ext[..n.min(ext.len())].to_vec();
        let mut start: u32 = head.iter().map(|e| e.count).sum();
        for chunk in ext[head.len()..].chunks(n) {
            self.extents
                .insert(ExtKey { file, fork, start }, chunk.to_vec());
            start += chunk.iter().map(|e| e.count).sum::<u32>();
        }
        self.dirty = true;
        head
    }

    /// The first extents a file record holds, data fork then resource fork.
    fn record_forks(&self, d: &[u8]) -> [(u8, u64, Vec<Extent>); 2] {
        if self.plus {
            [
                (0, be64(d, 88), extents_of(&d[104..168], true)),
                (0xFF, be64(d, 168), extents_of(&d[184..248], true)),
            ]
        } else {
            [
                (0, u64::from(be32(d, 26)), extents_of(&d[74..86], false)),
                (0xFF, u64::from(be32(d, 36)), extents_of(&d[86..98], false)),
            ]
        }
    }

    fn free_forks(&mut self, id: u32, d: &[u8]) {
        for (fork, _, first) in self.record_forks(d) {
            let more = self.drop_overflow(id, fork);
            self.release(&first);
            self.release(&more);
        }
    }

    // --- records --------------------------------------------------------------------

    fn folder_data(&self, id: u32) -> Vec<u8> {
        let now = self.now;
        if self.plus {
            let mut d = vec![0u8; 88];
            put_be16(&mut d, 0, u16::from(FOLDER));
            put_be32(&mut d, 8, id);
            for o in [12, 16, 20, 24] {
                put_be32(&mut d, o, now);
            }
            d
        } else {
            let mut d = vec![0u8; 70];
            d[0] = FOLDER;
            put_be32(&mut d, 6, id);
            put_be32(&mut d, 10, now);
            put_be32(&mut d, 14, now);
            d
        }
    }

    fn file_data(&self, id: u32, f: &NewFile, forks: [(u64, u32, &[Extent]); 2]) -> Vec<u8> {
        if self.plus {
            let mut d = vec![0u8; 248];
            put_be16(&mut d, 0, u16::from(FILE));
            put_be16(
                &mut d,
                2,
                PLUS_THREAD_EXISTS | if f.locked { LOCKED } else { 0 },
            );
            put_be32(&mut d, 8, id);
            put_be32(&mut d, 12, f.created);
            put_be32(&mut d, 16, f.modified);
            put_be32(&mut d, 20, self.now);
            put_be32(&mut d, 24, f.accessed);
            if f.locked {
                d[41] |= UF_IMMUTABLE;
            }
            d[48..64].copy_from_slice(&f.finfo);
            d[64..80].copy_from_slice(&f.fxinfo);
            for ((len, blocks, ext), at) in forks.into_iter().zip([88, 168]) {
                put_be64(&mut d, at, len);
                put_be32(&mut d, at + 12, blocks);
                for (i, e) in ext.iter().enumerate() {
                    put_be32(&mut d, at + 16 + 8 * i, e.start);
                    put_be32(&mut d, at + 20 + 8 * i, e.count);
                }
            }
            d
        } else {
            let mut d = vec![0u8; 102];
            d[0] = FILE;
            d[2] = HFS_FILE_FLAGS | u8::from(f.locked);
            d[4..20].copy_from_slice(&f.finfo);
            put_be32(&mut d, 20, id);
            for ((len, blocks, ext), (at, ext_at)) in forks.into_iter().zip([(24, 74), (34, 86)]) {
                put_be16(&mut d, at, ext.first().map_or(0, |e| e.start as u16));
                put_be32(&mut d, at + 2, len as u32);
                put_be32(&mut d, at + 6, (u64::from(blocks) * self.block) as u32);
                for (i, e) in ext.iter().enumerate() {
                    put_be16(&mut d, ext_at + 4 * i, e.start as u16);
                    put_be16(&mut d, ext_at + 2 + 4 * i, e.count as u16);
                }
            }
            put_be32(&mut d, 44, f.created);
            put_be32(&mut d, 48, f.modified);
            d[56..72].copy_from_slice(&f.fxinfo);
            d
        }
    }

    fn new_folder(&mut self, parent: u32, name: Vec<u16>) -> Result<u32> {
        let id = self.take_id()?;
        let data = self.folder_data(id);
        self.insert_thread(id, FOLDER_THREAD, parent, &name);
        self.insert(parent, name, data);
        Ok(id)
    }

    fn new_file(&mut self, parent: u32, name: Vec<u16>, f: NewFile) -> Result<u32> {
        let blocks = |len: u64| len.div_ceil(self.block);
        let (db, rb) = (blocks(f.data_len), blocks(f.rsrc.len() as u64));
        let limit = if self.plus {
            u64::MAX
        } else {
            u64::from(u32::MAX)
        };
        if db.max(rb).saturating_mul(self.block) > limit {
            return Err(Error::Invalid(
                "files on HFS volumes are smaller than 4 GiB".into(),
            ));
        }
        if db + rb > u64::from(self.free) + self.pending_blocks() {
            return Err(self.no_space());
        }
        let id = self.take_id()?;
        let dext = self.alloc(db as u32)?;
        if let Err(e) = self.fill(&dext, &mut *f.data, f.data_len) {
            self.release(&dext);
            return Err(e);
        }
        let rext = self.alloc(rb as u32)?;
        self.fill(&rext, &mut &f.rsrc[..], f.rsrc.len() as u64)?;
        let dhead = self.place(id, 0, &dext);
        let rhead = self.place(id, 0xFF, &rext);
        let data = self.file_data(
            id,
            &f,
            [
                (f.data_len, db as u32, &dhead[..]),
                (f.rsrc.len() as u64, rb as u32, &rhead[..]),
            ],
        );
        self.insert_thread(id, FILE_THREAD, parent, &name);
        self.insert(parent, name, data);
        Ok(id)
    }

    /// The hard link target number of an HFS+ link file.
    fn link_of(&self, d: &[u8]) -> Option<u32> {
        (self.plus && &d[48..52] == b"hlnk" && &d[52..56] == b"hfs+").then(|| be32(d, 44))
    }

    /// One link to iNode `n` fewer; the iNode file goes with its last link.
    fn unlink(&mut self, n: u32) {
        let private: Vec<u16> = PRIVATE_DATA.encode_utf16().collect();
        let Some(dir) = self
            .catalog
            .get(&self.key(ROOT_FOLDER, &private))
            .map(|v| self.id(&v.data))
        else {
            return;
        };
        let inode: Vec<u16> = format!("iNode{n}").encode_utf16().collect();
        let key = self.key(dir, &inode);
        let Some(v) = self.catalog.get_mut(&key) else {
            return;
        };
        let links = be32(&v.data, 44);
        if links > 1 {
            put_be32(&mut v.data, 44, links - 1);
            self.dirty = true;
            return;
        }
        let v = self.catalog.remove(&key).expect("present");
        let id = self.id(&v.data);
        self.free_forks(id, &v.data);
        self.catalog.remove(&(id, Vec::new()));
        self.gone.insert(id);
        self.touched.insert(dir);
    }

    fn delete(
        &mut self,
        key: &CatKey,
        recursive: bool,
        path: &str,
        stats: &mut Stats,
    ) -> Result<()> {
        let v = self.catalog[key].clone();
        let id = self.id(&v.data);
        match self.kind(&v.data) {
            FOLDER => {
                let kids = self.children(id);
                if !kids.is_empty() && !recursive {
                    return Err(Error::Invalid(format!(
                        "folder is not empty: {}",
                        display(path)
                    )));
                }
                for k in kids {
                    let name = self.shown(&self.catalog[&k].name);
                    self.delete(&k, true, &join(path, &name), stats)?;
                }
                stats.dirs += 1;
            }
            FILE => {
                stats.files += 1;
                stats.bytes += self.record_forks(&v.data)[0].1;
                self.free_forks(id, &v.data);
                if let Some(n) = self.link_of(&v.data) {
                    self.unlink(n);
                }
            }
            _ => return Err(invalid("a catalog entry that is neither file nor folder")),
        }
        self.catalog.remove(key);
        self.catalog.remove(&(id, Vec::new()));
        self.gone.insert(id);
        self.touched.insert(key.0);
        self.dirty = true;
        Ok(())
    }

    // --- operations -----------------------------------------------------------------

    pub(super) fn ensure_dir(&mut self, path: &str) -> Result<u32> {
        let mut id = ROOT_FOLDER;
        let mut done = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            done = join(&done, part);
            let name = self.name_of(part)?;
            if id == ROOT_FOLDER && self.plus && is_private(&name) {
                return Err(Error::Exists(display(&done)));
            }
            id = match self.catalog.get(&self.key(id, &name)) {
                Some(v) if self.kind(&v.data) == FOLDER => self.id(&v.data),
                Some(_) => return Err(Error::Exists(format!("{} (a file)", display(&done)))),
                None => self.new_folder(id, name)?,
            };
        }
        Ok(id)
    }

    pub(super) fn remove(&mut self, path: &str, recursive: bool) -> Result<Stats> {
        if path.is_empty() {
            return Err(Error::InvalidPath("cannot remove the root folder".into()));
        }
        let key = self
            .find(path)
            .ok_or_else(|| Error::NotFound(display(path)))?;
        let mut stats = Stats::default();
        self.delete(&key, recursive, path, &mut stats)?;
        Ok(stats)
    }

    pub(super) fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        if from.is_empty() {
            return Err(Error::InvalidPath("cannot move the root folder".into()));
        }
        let src = self
            .find(from)
            .ok_or_else(|| Error::NotFound(display(from)))?;
        let v = self.catalog[&src].clone();
        let (parent, name) = match self.find(to) {
            // The same entry under another case: a case-only rename.
            Some(k) if k == src => (src.0, self.name_of(split_parent(to).1)?),
            Some(k) if self.kind(&self.catalog[&k].data) == FOLDER => {
                (self.id(&self.catalog[&k].data), v.name.clone())
            }
            Some(_) => return Err(Error::Exists(display(to))),
            None => {
                let (p, n) = split_parent(to);
                (self.folder_id(p)?, self.name_of(n)?)
            }
        };
        if parent == ROOT_FOLDER && self.plus && is_private(&name) {
            return Err(Error::Exists(display(to)));
        }
        let id = self.id(&v.data);
        let kind = self.kind(&v.data);
        if kind == FOLDER {
            let (mut p, mut hops) = (parent, 0);
            while p != ROOT_PARENT && p != 0 && hops < 1 << 16 {
                if p == id {
                    return Err(Error::Invalid(format!(
                        "cannot move {} into itself",
                        display(from)
                    )));
                }
                p = self.thread_of(p).map_or(0, |t| t.0);
                hops += 1;
            }
        }
        let new_key = self.key(parent, &name);
        if new_key != src && self.catalog.contains_key(&new_key) {
            return Err(Error::Exists(display(to)));
        }
        if new_key == src && v.name == name {
            return Ok(());
        }
        self.catalog.remove(&src);
        self.touched.insert(src.0);
        self.insert(parent, name.clone(), v.data);
        if self.catalog.contains_key(&(id, Vec::new())) {
            let t = if kind == FOLDER {
                FOLDER_THREAD
            } else {
                FILE_THREAD
            };
            self.insert_thread(id, t, parent, &name);
        }
        Ok(())
    }

    pub(super) fn set_label(&mut self, label: &str) -> Result<()> {
        let label = label.trim();
        if label.is_empty() {
            return Err(Error::Invalid("an HFS volume always has a name".into()));
        }
        let name = if self.plus {
            names::plus_disk(label)
        } else {
            names::hfs_disk(label, 27)
        }
        .map_err(|e| {
            Error::Invalid(
                e.replace("HFS+ name", "HFS+ volume name")
                    .replace("HFS name", "HFS volume name"),
            )
        })?;
        let (rp, _) = self.thread_of(ROOT_FOLDER).expect("checked at load");
        let old = self.root_key();
        let v = self
            .catalog
            .remove(&old)
            .ok_or_else(|| invalid("the root folder record is missing"))?;
        self.catalog.insert(
            self.key(rp, &name),
            CatVal {
                name: name.clone(),
                data: v.data,
            },
        );
        self.insert_thread(ROOT_FOLDER, FOLDER_THREAD, rp, &name);
        if !self.plus {
            self.new_label = Some(name);
        }
        self.dirty = true;
        Ok(())
    }

    pub(super) fn edit(&mut self, path: &str, ed: &Ed, recursive: bool) -> Result<u64> {
        let key = self
            .find(path)
            .ok_or_else(|| Error::NotFound(display(path)))?;
        let mut n = 0;
        self.edit_key(&key, ed, recursive, &mut n);
        Ok(n)
    }

    fn edit_key(&mut self, key: &CatKey, ed: &Ed, recursive: bool, count: &mut u64) {
        let (plus, now) = (self.plus, self.now);
        let v = self.catalog.get_mut(key).expect("present");
        let d = &mut v.data;
        let kind = kind_of(plus, d);
        let file = kind == FILE;
        if let (Some(ro), true) = (ed.read_only, file) {
            if plus {
                let f = be16(d, 2) & !LOCKED | if ro { LOCKED } else { 0 };
                put_be16(d, 2, f);
                d[41] = d[41] & !UF_IMMUTABLE | if ro { UF_IMMUTABLE } else { 0 };
            } else {
                d[2] = d[2] & !1 | u8::from(ro);
            }
        }
        if let Some(h) = ed.hidden {
            let at = match (plus, file) {
                (true, _) => 56,
                (false, true) => 12,
                (false, false) => 30,
            };
            let f = be16(d, at) & !FINDER_INVISIBLE | if h { FINDER_INVISIBLE } else { 0 };
            put_be16(d, at, f);
        }
        let (c, m) = match (plus, file) {
            (true, _) => (12, 16),
            (false, true) => (44, 48),
            (false, false) => (10, 14),
        };
        if let Some(t) = ed.created {
            put_be32(d, c, t);
        }
        if let Some(t) = ed.modified {
            put_be32(d, m, t);
        }
        if plus {
            if let Some(t) = ed.accessed {
                put_be32(d, 24, t);
            }
            put_be32(d, 20, now);
        }
        let id = id_of(plus, d);
        *count += 1;
        self.dirty = true;
        if recursive && kind == FOLDER {
            for k in self.children(id) {
                self.edit_key(&k, ed, true, count);
            }
        }
    }

    pub(super) fn inject(
        &mut self,
        host: &Path,
        dest_dir: &str,
        overwrite: bool,
        forks: ForkMode,
        stats: &mut Stats,
    ) -> Result<()> {
        let name = host
            .file_name()
            .ok_or_else(|| Error::InvalidPath(host.display().to_string()))?
            .to_string_lossy()
            .into_owned();
        let meta = fs::metadata(host)?;
        if meta.is_dir() {
            let path = join(dest_dir, &name);
            self.ensure_dir(&path)?;
            stats.dirs += 1;
            let mut kids: Vec<_> = fs::read_dir(host)?.collect::<io::Result<_>>()?;
            kids.sort_by_key(|c| c.file_name());
            for c in kids {
                let cn = c.file_name().to_string_lossy().into_owned();
                // AppleDouble header files go in with the file they belong to.
                if forks == ForkMode::AppleDouble
                    && cn.len() > 2
                    && cn.starts_with("._")
                    && host.join(&cn[2..]).exists()
                {
                    continue;
                }
                self.inject(&c.path(), &path, overwrite, forks, stats)?;
            }
            return Ok(());
        }
        let parent = self.ensure_dir(dest_dir)?;
        let mut inc = Incoming::open(host, &name, &meta, forks, self.plus)?;
        let disk = self.name_of(&inc.name)?;
        let img_path = join(dest_dir, &self.shown(&disk));
        let key = self.key(parent, &disk);
        if let Some(v) = self.catalog.get(&key) {
            if self.kind(&v.data) == FOLDER {
                return Err(Error::Exists(format!("{} (a folder)", display(&img_path))));
            }
            if !overwrite {
                return Err(Error::Exists(display(&img_path)));
            }
            self.delete(&key, false, &img_path, &mut Stats::default())?;
        }
        let now = self.now;
        let bytes = inc.data_len + inc.rsrc.len() as u64;
        self.new_file(
            parent,
            disk,
            NewFile {
                data: &mut inc.data,
                data_len: inc.data_len,
                rsrc: std::mem::take(&mut inc.rsrc),
                finfo: inc.finfo,
                fxinfo: inc.fxinfo,
                created: inc.created.unwrap_or(now),
                modified: inc.modified.unwrap_or(now),
                accessed: inc.accessed.unwrap_or(now),
                locked: inc.locked,
            },
        )
        .map_err(|e| match e {
            Error::Invalid(m) if m.starts_with("not enough free space") => Error::Invalid(format!(
                "not enough free space in the image for {}",
                display(&img_path)
            )),
            other => other,
        })?;
        stats.files += 1;
        stats.bytes += bytes;
        Ok(())
    }

    // --- writing back ---------------------------------------------------------------

    fn tree(&self, w: Which) -> &Tree {
        match w {
            Which::Cat => &self.cat,
            Which::Ext => &self.ext,
            Which::Attr => self.attr.as_ref().expect("loaded"),
        }
    }

    fn tree_mut(&mut self, w: Which) -> &mut Tree {
        match w {
            Which::Cat => &mut self.cat,
            Which::Ext => &mut self.ext,
            Which::Attr => self.attr.as_mut().expect("loaded"),
        }
    }

    fn cat_records(&self) -> Vec<Rec> {
        self.catalog
            .iter()
            .map(|((parent, _), v)| {
                let mut b = Vec::with_capacity(16 + 2 * v.name.len() + v.data.len());
                if self.plus {
                    b.extend(((6 + 2 * v.name.len()) as u16).to_be_bytes());
                    b.extend(parent.to_be_bytes());
                    b.extend((v.name.len() as u16).to_be_bytes());
                    for u in &v.name {
                        b.extend(u.to_be_bytes());
                    }
                } else {
                    b.push((6 + v.name.len()) as u8);
                    b.push(0);
                    b.extend(parent.to_be_bytes());
                    b.push(v.name.len() as u8);
                    b.extend(v.name.iter().map(|&u| u as u8));
                    if b.len() % 2 == 1 {
                        b.push(0);
                    }
                }
                let key_len = b.len();
                b.extend_from_slice(&v.data);
                Rec { bytes: b, key_len }
            })
            .collect()
    }

    fn ext_records(&self) -> Vec<Rec> {
        self.extents
            .iter()
            .map(|(k, e)| {
                let mut b = Vec::with_capacity(76);
                if self.plus {
                    b.extend(10u16.to_be_bytes());
                    b.push(k.fork);
                    b.push(0);
                    b.extend(k.file.to_be_bytes());
                    b.extend(k.start.to_be_bytes());
                    for i in 0..8 {
                        let x = e.get(i).copied().unwrap_or(Extent { start: 0, count: 0 });
                        b.extend(x.start.to_be_bytes());
                        b.extend(x.count.to_be_bytes());
                    }
                    Rec {
                        bytes: b,
                        key_len: 12,
                    }
                } else {
                    b.push(7);
                    b.push(k.fork);
                    b.extend(k.file.to_be_bytes());
                    b.extend((k.start as u16).to_be_bytes());
                    for i in 0..3 {
                        let x = e.get(i).copied().unwrap_or(Extent { start: 0, count: 0 });
                        b.extend((x.start as u16).to_be_bytes());
                        b.extend((x.count as u16).to_be_bytes());
                    }
                    Rec {
                        bytes: b,
                        key_len: 8,
                    }
                }
            })
            .collect()
    }

    fn plan(t: &Tree, recs: &[Rec], total: u32) -> Plan {
        let ns = t.node_size;
        let leaves = pack(ns, recs.iter().map(|r| r.bytes.len()));
        let idx = t.index_key_len();
        let mut index = Vec::new();
        let mut firsts: Vec<usize> = leaves.iter().map(|r| r.start).collect();
        while firsts.len() > 1 {
            let groups = pack(
                ns,
                firsts.iter().map(|&i| idx.unwrap_or(recs[i].key_len) + 4),
            );
            firsts = groups.iter().map(|g| firsts[g.start]).collect();
            index.push(groups);
        }
        Plan {
            leaves,
            index,
            map_nodes: t.map_nodes(total),
        }
    }

    /// The tree's shape, growing its file until the shape fits.
    fn plan_room(&mut self, which: Which, recs: &[Rec]) -> Result<Plan> {
        loop {
            let t = self.tree(which);
            let total = t.total_nodes(self.block);
            let plan = Self::plan(t, recs, total);
            if plan.nodes() <= total {
                return Ok(plan);
            }
            let ns = t.node_size as u64;
            let unit = ns.max(self.block);
            let bytes = (u64::from(plan.nodes() - total) * ns)
                .max(u64::from(t.grow))
                .next_multiple_of(unit);
            let after = t.fork.last().map_or(0, |e| e.start + e.count);
            let n = u32::try_from(bytes / self.block).map_err(|_| self.no_space())?;
            let more = self.alloc_after(after, n).map_err(|_| {
                Error::Invalid("not enough free space in the image for the catalog".into())
            })?;
            for e in &more {
                let zero = vec![0u8; (u64::from(e.count) * self.block) as usize];
                self.write_extents(std::slice::from_ref(e), 0, &zero)?;
            }
            let max = self.max_extent();
            let t = self.tree_mut(which);
            t.fork.extend(more);
            t.fork = merge(&t.fork, max);
        }
    }

    fn index_key(t: &Tree, key: &[u8]) -> Vec<u8> {
        match t.index_key_len() {
            None => key.to_vec(),
            Some(len) => {
                let mut k = vec![0u8; len];
                let n = key.len().min(len);
                k[..n].copy_from_slice(&key[..n]);
                if t.big_keys() {
                    put_be16(&mut k, 0, t.max_key);
                } else {
                    k[0] = t.max_key as u8;
                }
                k
            }
        }
    }

    fn write_tree(&self, t: &Tree, plan: &Plan, recs: &[Rec]) -> Result<()> {
        let ns = t.node_size;
        let total = t.total_nodes(self.block);
        let used = plan.nodes();
        let m = plan.map_nodes;
        let first_leaf = 1 + m;
        let nl = plan.leaves.len() as u32;
        let mut out = Vec::with_capacity(used as usize * ns);
        // Header node, then map nodes.
        let header_bits = (ns - 256) * 8;
        let per_map = (ns - 20) * 8;
        let mut levels_base = Vec::new();
        let mut next = first_leaf + nl;
        for l in &plan.index {
            levels_base.push(next);
            next += l.len() as u32;
        }
        let (depth, root) = match (nl, plan.index.last()) {
            (0, _) => (0, 0),
            (_, None) => (1, first_leaf),
            (_, Some(_)) => (plan.index.len() as u16 + 1, used - 1),
        };
        let mut head = vec![0u8; ns];
        put_be32(&mut head, 0, if m > 0 { 1 } else { 0 });
        head[8] = 1;
        put_be16(&mut head, 10, 3);
        let h = 14;
        put_be16(&mut head, h, depth);
        put_be32(&mut head, h + 2, root);
        put_be32(&mut head, h + 6, recs.len() as u32);
        put_be32(&mut head, h + 10, if nl > 0 { first_leaf } else { 0 });
        put_be32(
            &mut head,
            h + 14,
            if nl > 0 { first_leaf + nl - 1 } else { 0 },
        );
        put_be16(&mut head, h + 18, ns as u16);
        put_be16(&mut head, h + 20, t.max_key);
        put_be32(&mut head, h + 22, total);
        put_be32(&mut head, h + 26, total - used);
        put_be32(&mut head, h + 32, t.hdr_clump);
        head[h + 36] = t.btype;
        head[h + 37] = t.compare;
        put_be32(&mut head, h + 38, t.attributes);
        head[120..248].copy_from_slice(&t.user);
        let set_bits = |node: &mut [u8], at: usize, from: usize, count: usize| {
            for i in 0..count {
                if from + i < used as usize {
                    node[at + i / 8] |= 0x80 >> (i % 8);
                }
            }
        };
        set_bits(&mut head, 248, 0, header_bits);
        put_be16(&mut head, ns - 2, 14);
        put_be16(&mut head, ns - 4, 120);
        put_be16(&mut head, ns - 6, 248);
        put_be16(&mut head, ns - 8, (ns - 8) as u16);
        out.extend(head);
        for i in 0..m {
            let mut n = vec![0u8; ns];
            put_be32(&mut n, 0, if i + 1 < m { i + 2 } else { 0 });
            n[8] = 2;
            put_be16(&mut n, 10, 1);
            set_bits(&mut n, 14, header_bits + i as usize * per_map, per_map);
            put_be16(&mut n, ns - 2, 14);
            put_be16(&mut n, ns - 4, (ns - 6) as u16);
            out.extend(n);
        }
        // Leaves.
        for (i, r) in plan.leaves.iter().enumerate() {
            let i = i as u32;
            let records: Vec<Vec<u8>> = recs[r.clone()].iter().map(|r| r.bytes.clone()).collect();
            out.extend(make_node(
                ns,
                if i + 1 < nl { first_leaf + i + 1 } else { 0 },
                if i > 0 { first_leaf + i - 1 } else { 0 },
                0xFF,
                1,
                &records,
            ));
        }
        // Index levels: each record is the first key of a child node and its number.
        let mut firsts: Vec<usize> = plan.leaves.iter().map(|r| r.start).collect();
        let mut child_base = first_leaf;
        for (l, groups) in plan.index.iter().enumerate() {
            let base = levels_base[l];
            let n = groups.len() as u32;
            for (j, g) in groups.iter().enumerate() {
                let j = j as u32;
                let records: Vec<Vec<u8>> = g
                    .clone()
                    .map(|c| {
                        let r = &recs[firsts[c]];
                        let mut k = Self::index_key(t, &r.bytes[..r.key_len]);
                        k.extend((child_base + c as u32).to_be_bytes());
                        k
                    })
                    .collect();
                out.extend(make_node(
                    ns,
                    if j + 1 < n { base + j + 1 } else { 0 },
                    if j > 0 { base + j - 1 } else { 0 },
                    0,
                    l as u8 + 2,
                    &records,
                ));
            }
            firsts = groups.iter().map(|g| firsts[g.start]).collect();
            child_base = base;
        }
        debug_assert_eq!(out.len(), used as usize * ns);
        self.write_extents(&t.fork, 0, &out)?;
        // Clear nodes that held data before and are free now.
        let stale = t.old_used.min(total);
        if stale > used {
            let zero = vec![0u8; (stale - used) as usize * ns];
            self.write_extents(&t.fork, u64::from(used) * ns as u64, &zero)?;
        }
        Ok(())
    }

    /// The HFS+ attributes tree without the records of removed files, or `None` when
    /// nothing changes.
    fn attr_without_gone(&mut self) -> Result<Option<Vec<Rec>>> {
        if !self.plus || self.gone.is_empty() {
            return Ok(None);
        }
        let f = plus_fork(&self.vh[352..432]);
        if f.size == 0 || f.extents.is_empty() {
            return Ok(None);
        }
        let mut ext = f.extents;
        let have: u64 = ext.iter().map(|e| u64::from(e.count)).sum();
        if have * self.block < f.size {
            ext.extend(
                self.extents
                    .range(
                        ExtKey {
                            file: ATTRIBUTES_FILE,
                            fork: 0,
                            start: 0,
                        }..=ExtKey {
                            file: ATTRIBUTES_FILE,
                            fork: 0,
                            start: u32::MAX,
                        },
                    )
                    .flat_map(|(_, e)| e.clone()),
            );
        }
        let bt = self.fs.tree_header(ext)?;
        let (tree, recs) = load_tree(self.fs, &bt)?;
        let before = recs.len();
        let mut kept = Vec::with_capacity(before);
        for (k, d) in recs {
            if k.len() >= 8 && self.gone.contains(&be32(&k, 4)) {
                // Fork and extents attribute records own blocks.
                match (d.len(), be32(&d, 0)) {
                    (88.., 0x20) => self.release(&extents_of(&d[24..88], true)),
                    (72.., 0x30) => self.release(&extents_of(&d[8..72], true)),
                    _ => {}
                }
                continue;
            }
            let key_len = k.len();
            let mut bytes = k;
            bytes.extend(d);
            kept.push(Rec { bytes, key_len });
        }
        if kept.len() == before {
            return Ok(None);
        }
        self.attr = Some(tree);
        Ok(Some(kept))
    }

    /// Valences (and HFS+ subfolder counts) from the catalog; modification dates of the
    /// folders whose contents changed.
    fn fix_folders(&mut self) -> Result<()> {
        let plus = self.plus;
        let mut counts: HashMap<u32, (u32, u32)> = HashMap::new();
        for ((parent, sk), v) in &self.catalog {
            if sk.is_empty() {
                continue;
            }
            let c = counts.entry(*parent).or_default();
            match kind_of(plus, &v.data) {
                FILE => c.0 += 1,
                FOLDER => c.1 += 1,
                _ => {}
            }
        }
        let now = self.now;
        for v in self.catalog.values_mut() {
            let d = &mut v.data;
            if kind_of(plus, d) != FOLDER {
                continue;
            }
            let id = id_of(plus, d);
            let (files, dirs) = counts.get(&id).copied().unwrap_or_default();
            if plus {
                put_be32(d, 4, files + dirs);
                if be16(d, 2) & HAS_FOLDER_COUNT != 0 {
                    put_be32(d, 84, dirs);
                }
            } else {
                let n = u16::try_from(files + dirs).map_err(|_| {
                    Error::Invalid("an HFS folder holds at most 65535 items".into())
                })?;
                put_be16(d, 4, n);
            }
            if self.touched.contains(&id) {
                if plus {
                    put_be32(d, 16, now);
                    put_be32(d, 20, now);
                } else {
                    put_be32(d, 14, now);
                }
            }
        }
        Ok(())
    }

    /// (files, folders without the root, files in the root, folders in the root).
    fn counts(&self) -> (u32, u32, u32, u32) {
        let (mut files, mut dirs, mut rf, mut rd) = (0u32, 0u32, 0u32, 0u32);
        for ((parent, sk), v) in &self.catalog {
            if sk.is_empty() {
                continue;
            }
            let k = self.kind(&v.data);
            let root = *parent == ROOT_FOLDER;
            match k {
                FILE => {
                    files += 1;
                    rf += u32::from(root);
                }
                FOLDER => {
                    dirs += 1;
                    rd += u32::from(root);
                }
                _ => {}
            }
        }
        (files, dirs.saturating_sub(1), rf, rd)
    }

    fn put_plus_fork(&mut self, at: usize, head: &[Extent], blocks: u32) {
        let vh = &mut self.vh;
        put_be64(vh, at, u64::from(blocks) * self.block);
        put_be32(vh, at + 12, blocks);
        vh[at + 16..at + 80].fill(0);
        for (i, e) in head.iter().enumerate() {
            put_be32(vh, at + 16 + 8 * i, e.start);
            put_be32(vh, at + 20 + 8 * i, e.count);
        }
    }

    fn put_hfs_fork(&mut self, at: usize, ext_at: usize, head: &[Extent], blocks: u32) {
        let vh = &mut self.vh;
        put_be32(vh, at, (u64::from(blocks) * self.block) as u32);
        vh[ext_at..ext_at + 12].fill(0);
        for (i, e) in head.iter().enumerate() {
            put_be16(vh, ext_at + 4 * i, e.start as u16);
            put_be16(vh, ext_at + 2 + 4 * i, e.count as u16);
        }
    }

    pub(super) fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        // Room first: everything that can fail for lack of space happens before writing.
        let attr_recs = self.attr_without_gone()?;
        self.commit_frees();
        self.fix_folders()?;
        let cat_recs = self.cat_records();
        let cat_plan = self.plan_room(Which::Cat, &cat_recs)?;
        let cat_fork = self.cat.fork.clone();
        let cat_head = self.place(CATALOG_FILE, 0, &cat_fork);
        let attr = match attr_recs {
            Some(recs) => {
                let plan = self.plan_room(Which::Attr, &recs)?;
                let fork = self.tree(Which::Attr).fork.clone();
                let head = self.place(ATTRIBUTES_FILE, 0, &fork);
                Some((recs, plan, head, fork.iter().map(|e| e.count).sum::<u32>()))
            }
            None => None,
        };
        let ext_recs = self.ext_records();
        let ext_plan = self.plan_room(Which::Ext, &ext_recs)?;
        let n = if self.plus { 8 } else { 3 };
        if self.ext.fork.len() > n {
            return Err(Error::Invalid(
                "the extents overflow file is full (the volume is too fragmented)".into(),
            ));
        }
        // Then write.
        self.write_tree(&self.cat, &cat_plan, &cat_recs)?;
        if let Some((recs, plan, _, _)) = &attr {
            self.write_tree(self.tree(Which::Attr), plan, recs)?;
        }
        self.write_tree(&self.ext, &ext_plan, &ext_recs)?;
        if self.plus {
            self.write_extents(&self.bitmap_fork, 0, &self.bitmap)?;
        } else {
            let at = u64::from(be16(&self.vh, 14)) * 512;
            self.fs.w.borrow_mut().write_at(at, &self.bitmap)?;
        }
        let (files, dirs, root_files, root_dirs) = self.counts();
        let (cat_blocks, ext_fork) = (self.cat.blocks(), self.ext.fork.clone());
        let ext_blocks = self.ext.blocks();
        let now = self.now;
        if self.plus {
            put_be32(&mut self.vh, 20, now);
            put_be32(&mut self.vh, 32, files);
            put_be32(&mut self.vh, 36, dirs);
            put_be32(&mut self.vh, 48, self.free);
            put_be32(&mut self.vh, 52, self.hint);
            put_be32(&mut self.vh, 64, self.next_id);
            let wc = be32(&self.vh, 68).wrapping_add(1);
            put_be32(&mut self.vh, 68, wc);
            self.put_plus_fork(192, &ext_fork, ext_blocks);
            self.put_plus_fork(272, &cat_head, cat_blocks);
            if let Some((_, _, head, blocks)) = &attr {
                self.put_plus_fork(352, head, *blocks);
            }
        } else {
            let vh = &mut self.vh;
            put_be32(vh, 6, now);
            put_be16(vh, 12, root_files.min(0xFFFF) as u16);
            put_be16(vh, 16, self.hint as u16);
            put_be32(vh, 30, self.next_id);
            put_be16(vh, 34, self.free as u16);
            let wc = be32(vh, 70).wrapping_add(1);
            put_be32(vh, 70, wc);
            put_be16(vh, 82, root_dirs.min(0xFFFF) as u16);
            put_be32(vh, 84, files);
            put_be32(vh, 88, dirs);
            if let Some(l) = &self.new_label {
                vh[36] = l.len() as u8;
                vh[37..64].fill(0);
                for (i, &u) in l.iter().enumerate() {
                    vh[37 + i] = u as u8;
                }
            }
            self.put_hfs_fork(130, 134, &ext_fork, ext_blocks);
            self.put_hfs_fork(146, 150, &cat_head, cat_blocks);
        }
        let mut w = self.fs.w.borrow_mut();
        let len = w.len();
        w.write_at(self.at + 1024, &self.vh)?;
        let alt = if self.plus {
            self.at + len - 1024
        } else {
            (len / 512 - 2) * 512
        };
        w.write_at(alt, &self.vh)?;
        self.dirty = false;
        Ok(())
    }

    // --- checking -------------------------------------------------------------------

    /// Problems found: catalog threads, valences and counts, and block ownership against
    /// the allocation bitmap.
    pub(super) fn check(&self) -> Vec<String> {
        let mut p = Vec::new();
        let plus = self.plus;
        let mut owner: Vec<u32> = vec![0; self.total as usize];
        let mut claim = |p: &mut Vec<String>, who: u32, ext: &[Extent]| {
            for e in ext {
                for b in e.start..e.start.saturating_add(e.count) {
                    match owner.get_mut(b as usize) {
                        None => p.push(format!(
                            "CNID {who}: block {b} is past the end of the volume"
                        )),
                        Some(o) if *o != 0 => {
                            p.push(format!("block {b} belongs to both CNID {} and {who}", *o))
                        }
                        Some(o) => *o = who,
                    }
                }
            }
        };
        let overflow = |file: u32, fork: u8| -> Vec<(u32, Vec<Extent>)> {
            self.extents
                .range(
                    ExtKey {
                        file,
                        fork,
                        start: 0,
                    }..=ExtKey {
                        file,
                        fork,
                        start: u32::MAX,
                    },
                )
                .map(|(k, e)| (k.start, e.clone()))
                .collect()
        };
        let full_fork =
            |p: &mut Vec<String>, who: u32, fork: u8, first: &[Extent]| -> Vec<Extent> {
                let mut all = first.to_vec();
                for (start, e) in overflow(who, fork) {
                    let have: u32 = all.iter().map(|x| x.count).sum();
                    if start != have {
                        p.push(format!(
                            "CNID {who}: overflow extents start at {start}, expected {have}"
                        ));
                    }
                    all.extend(e);
                }
                all
            };
        // Volume structures.
        let block = self.block;
        if plus {
            let front = 1536u64.div_ceil(block) as u32;
            let tail = 1024u64.div_ceil(block) as u32;
            claim(
                &mut p,
                1,
                &[Extent {
                    start: 0,
                    count: front,
                }],
            );
            claim(
                &mut p,
                1,
                &[Extent {
                    start: self.total - tail,
                    count: tail,
                }],
            );
            claim(&mut p, ALLOCATION_FILE, &self.bitmap_fork);
            for (id, at) in [(ATTRIBUTES_FILE, 352), (STARTUP_FILE, 432)] {
                let f = plus_fork(&self.vh[at..at + 80]);
                let all = full_fork(&mut p, id, 0, &f.extents);
                claim(&mut p, id, &all);
            }
        }
        claim(&mut p, EXTENTS_FILE, &self.ext.fork);
        claim(&mut p, CATALOG_FILE, &self.cat.fork);
        // Catalog.
        let mut counts: HashMap<u32, u32> = HashMap::new();
        let mut max_id = 0;
        let mut ids: BTreeMap<u32, String> = BTreeMap::new();
        for ((parent, sk), v) in &self.catalog {
            let d = &v.data;
            let k = self.kind(d);
            if sk.is_empty() {
                let Some((tp, tn)) = self.thread_of(*parent) else {
                    continue;
                };
                match self.catalog.get(&self.key(tp, &tn)) {
                    Some(t) if self.id(&t.data) == *parent => {
                        if (k == FOLDER_THREAD) != (self.kind(&t.data) == FOLDER) {
                            p.push(format!("thread of CNID {parent} has the wrong type"));
                        }
                    }
                    _ => p.push(format!("thread of CNID {parent} points to nothing")),
                }
                continue;
            }
            *counts.entry(*parent).or_default() += 1;
            let id = self.id(d);
            max_id = max_id.max(id);
            let here = format!("{} in folder {parent}", self.shown(&v.name));
            if let Some(first) = ids.insert(id, here.clone()) {
                p.push(format!("CNID {id} is used twice: {first}, {here}"));
            }
            let needs_thread = k == FOLDER || plus || (k == FILE && d[2] & 0x02 != 0);
            match self.thread_of(id) {
                Some((tp, tn)) if tp == *parent && names::sort_key(self.order, &tn) == *sk => {}
                Some(_) => p.push(format!("CNID {id}: thread record does not match")),
                None if needs_thread => p.push(format!("CNID {id}: no thread record")),
                None => {}
            }
            if k == FILE {
                for (fork, len, first) in self.record_forks(d) {
                    let all = full_fork(&mut p, id, fork, &first);
                    let blocks: u64 = all.iter().map(|e| u64::from(e.count)).sum();
                    if blocks * block < len {
                        p.push(format!(
                            "CNID {id}: fork {fork:#x} is longer than its blocks"
                        ));
                    }
                    if !plus {
                        let phys = u64::from(be32(d, if fork == 0 { 30 } else { 40 }));
                        if phys != blocks * block {
                            p.push(format!(
                                "CNID {id}: physical length {phys} for {blocks} blocks"
                            ));
                        }
                    }
                    claim(&mut p, id, &all);
                }
            }
        }
        for v in self.catalog.values() {
            if self.kind(&v.data) == FOLDER {
                let id = self.id(&v.data);
                let val = if plus {
                    be32(&v.data, 4)
                } else {
                    u32::from(be16(&v.data, 4))
                };
                let n = counts.get(&id).copied().unwrap_or(0);
                if val != n {
                    p.push(format!("folder CNID {id}: valence {val}, holds {n}"));
                }
            }
        }
        let next = if plus {
            be32(&self.vh, 64)
        } else {
            be32(&self.vh, 30)
        };
        if next <= max_id {
            p.push(format!(
                "the header's next CNID {next} is not above {max_id}"
            ));
        }
        for k in self.extents.keys() {
            if !ids.contains_key(&k.file) && !(3..=8).contains(&k.file) {
                p.push(format!("overflow extents for missing CNID {}", k.file));
            }
        }
        let (files, dirs, rf, rd) = self.counts();
        let vh = &self.vh;
        let got = if plus {
            (be32(vh, 32), be32(vh, 36), rf, rd, be32(vh, 48))
        } else {
            (
                be32(vh, 84),
                be32(vh, 88),
                u32::from(be16(vh, 12)),
                u32::from(be16(vh, 82)),
                u32::from(be16(vh, 34)),
            )
        };
        if got != (files, dirs, rf, rd, self.free) {
            p.push(format!(
                "volume header counts (files, folders, root files, root folders, free) {got:?}, expected {:?}",
                (files, dirs, rf, rd, self.free)
            ));
        }
        // Bitmap.
        let mut wrong = 0;
        for b in 0..self.total {
            if self.bit(b) != (owner[b as usize] != 0) {
                if wrong < 10 {
                    p.push(format!(
                        "block {b}: bitmap says {}, {}",
                        if self.bit(b) { "used" } else { "free" },
                        if owner[b as usize] != 0 {
                            format!("CNID {} owns it", owner[b as usize])
                        } else {
                            "nothing owns it".into()
                        }
                    ));
                }
                wrong += 1;
            }
        }
        if wrong >= 10 {
            p.push(format!("{wrong} blocks differ from the bitmap in all"));
        }
        p
    }
}

/// A host file on its way in: name, forks and Finder information.
struct Incoming {
    name: String,
    data: Box<dyn Read>,
    data_len: u64,
    rsrc: Vec<u8>,
    finfo: [u8; 16],
    fxinfo: [u8; 16],
    created: Option<u32>,
    modified: Option<u32>,
    accessed: Option<u32>,
    locked: bool,
}

impl Incoming {
    fn open(
        host: &Path,
        name: &str,
        meta: &fs::Metadata,
        forks: ForkMode,
        plus: bool,
    ) -> Result<Incoming> {
        let stamp = |t: io::Result<std::time::SystemTime>| t.ok().map(|t| mac_time(t, plus));
        let mut inc = Incoming {
            name: name.to_string(),
            data: Box::new(File::open(host)?),
            data_len: meta.len(),
            rsrc: Vec::new(),
            finfo: [0; 16],
            fxinfo: [0; 16],
            created: stamp(meta.created()).or(stamp(meta.modified())),
            modified: stamp(meta.modified()),
            accessed: stamp(meta.accessed()),
            locked: false,
        };
        match forks {
            ForkMode::Data => {}
            ForkMode::AppleDouble => {
                if let Some(base) = name.strip_prefix("._").filter(|b| !b.is_empty()) {
                    // A header file whose data fork is not here: a file with only a
                    // resource fork and Finder information.
                    if let Some((finder, rsrc)) = parse_appledouble(&fs::read(host)?) {
                        inc.name = base.to_string();
                        inc.data = Box::new(io::empty());
                        inc.data_len = 0;
                        inc.set_finder(&finder, rsrc);
                    }
                } else {
                    let side = host.with_file_name(format!("._{name}"));
                    if side.is_file()
                        && let Some((finder, rsrc)) = parse_appledouble(&fs::read(&side)?)
                    {
                        inc.set_finder(&finder, rsrc);
                    }
                }
            }
            ForkMode::MacBinary => {
                let mut f = File::open(host)?;
                let mut h = [0u8; 128];
                if meta.len() >= 128
                    && f.read_exact(&mut h).is_ok()
                    && let Some(mb) = parse_macbinary(&h, meta.len())
                {
                    f.seek(SeekFrom::Start(mb.rsrc_at))?;
                    let mut rsrc = vec![0u8; mb.rsrc_len as usize];
                    f.read_exact(&mut rsrc)?;
                    f.seek(SeekFrom::Start(mb.data_at))?;
                    inc.name = mb.name;
                    inc.data = Box::new(f.take(mb.data_len));
                    inc.data_len = mb.data_len;
                    inc.rsrc = rsrc;
                    inc.finfo = mb.finfo;
                    inc.fxinfo = mb.fxinfo;
                    inc.locked = mb.locked;
                    // MacBinary dates are local time.
                    let conv = |s: u32| -> Option<u32> {
                        if plus {
                            mac_unix(s, false)
                                .map(|u| (u as i64 + MAC_EPOCH).clamp(0, u32::MAX.into()) as u32)
                        } else {
                            (s != 0).then_some(s)
                        }
                    };
                    inc.created = conv(mb.created).or(inc.created);
                    inc.modified = conv(mb.modified).or(inc.modified);
                }
            }
        }
        Ok(inc)
    }

    fn set_finder(&mut self, finder: &[u8; 32], rsrc: Vec<u8>) {
        self.finfo.copy_from_slice(&finder[..16]);
        self.fxinfo.copy_from_slice(&finder[16..]);
        self.rsrc = rsrc;
    }
}

/// Finder information (32 bytes) and resource fork of an AppleDouble (or AppleSingle) file.
fn parse_appledouble(b: &[u8]) -> Option<([u8; 32], Vec<u8>)> {
    if b.len() < 26 || !matches!(be32(b, 0), 0x0005_1607 | 0x0005_1600) {
        return None;
    }
    let mut finder = [0u8; 32];
    let mut rsrc = Vec::new();
    for i in 0..usize::from(be16(b, 24)) {
        let at = 26 + 12 * i;
        if at + 12 > b.len() {
            return None;
        }
        let (id, off, len) = (
            be32(b, at),
            be32(b, at + 4) as usize,
            be32(b, at + 8) as usize,
        );
        let data = b.get(off..off.checked_add(len)?)?;
        match id {
            9 => {
                let n = data.len().min(32);
                finder[..n].copy_from_slice(&data[..n]);
            }
            2 => rsrc = data.to_vec(),
            _ => {}
        }
    }
    Some((finder, rsrc))
}

struct MacBin {
    name: String,
    finfo: [u8; 16],
    fxinfo: [u8; 16],
    locked: bool,
    created: u32,
    modified: u32,
    data_at: u64,
    data_len: u64,
    rsrc_at: u64,
    rsrc_len: u64,
}

/// A MacBinary II or III header (checked by its CRC or the "mBIN" signature).
fn parse_macbinary(h: &[u8; 128], file_len: u64) -> Option<MacBin> {
    let n = usize::from(h[1]);
    if h[0] != 0 || h[74] != 0 || h[82] != 0 || !(1..=63).contains(&n) {
        return None;
    }
    if be16(h, 124) != crc16_xmodem(&h[..124]) && &h[102..106] != b"mBIN" {
        return None;
    }
    let (data_len, rsrc_len) = (u64::from(be32(h, 83)), u64::from(be32(h, 87)));
    if data_len > 0x7FFF_FFFF || rsrc_len > 0x7FFF_FFFF {
        return None;
    }
    let data_at = 128 + u64::from(be16(h, 120)).next_multiple_of(128);
    let rsrc_at = data_at + data_len.next_multiple_of(128);
    if rsrc_at + rsrc_len > file_len.next_multiple_of(128) || data_at + data_len > file_len {
        return None;
    }
    if rsrc_len > 0 && rsrc_at + rsrc_len > file_len {
        return None;
    }
    let mut finfo = [0u8; 16];
    finfo[..8].copy_from_slice(&h[65..73]);
    finfo[8] = h[73];
    finfo[9] = h[101];
    finfo[10..16].copy_from_slice(&h[75..81]);
    let mut fxinfo = [0u8; 16];
    fxinfo[8] = h[106];
    fxinfo[9] = h[107];
    let name = names::hfs_display(
        &h[2..2 + n]
            .iter()
            .map(|&b| u16::from(b))
            .collect::<Vec<_>>(),
    );
    Some(MacBin {
        name,
        finfo,
        fxinfo,
        locked: h[81] & 1 != 0,
        created: be32(h, 91),
        modified: be32(h, 95),
        data_at,
        data_len,
        rsrc_at,
        rsrc_len,
    })
}

// --- the volume's face -------------------------------------------------------------------

impl HfsFs {
    /// Why this volume cannot be written, if it cannot.
    pub(crate) fn write_blocker(&self) -> Option<String> {
        if self.wrapped {
            Some("HFS+ volumes inside an HFS wrapper are read-only so far".into())
        } else if self.journaled {
            Some("journaled HFS+ volumes are read-only (turn journaling off on a Mac first)".into())
        } else {
            None
        }
    }

    fn change<T>(&self, f: impl FnOnce(&mut Writer) -> Result<T>) -> Result<T> {
        if let Some(b) = self.write_blocker() {
            return Err(Error::Invalid(b));
        }
        let mut w = Writer::load(self)?;
        let r = f(&mut w);
        let flushed = w.flush();
        drop(w);
        self.reload()?;
        flushed?;
        r
    }

    pub(crate) fn inject(
        &self,
        host: &Path,
        dest_dir: &str,
        overwrite: bool,
        forks: ForkMode,
    ) -> Result<Stats> {
        self.change(|w| {
            let mut stats = Stats::default();
            w.inject(host, dest_dir, overwrite, forks, &mut stats)?;
            Ok(stats)
        })
    }

    pub(crate) fn mkdir(&self, path: &str) -> Result<()> {
        self.change(|w| w.ensure_dir(path).map(|_| ()))
    }

    pub(crate) fn remove(&self, path: &str, recursive: bool) -> Result<Stats> {
        self.change(|w| w.remove(path, recursive))
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.change(|w| w.rename(from, to))
    }

    pub(crate) fn set_label(&self, label: Option<&str>) -> Result<()> {
        self.change(|w| w.set_label(label.unwrap_or("")))
    }

    /// Locked (files only), invisible and dates. HFS has no system or archive attribute,
    /// and keeps no access dates.
    pub(crate) fn edit(&self, path: &str, edit: &EntryEdit, recursive: bool) -> Result<u64> {
        let mut foreign: Vec<&str> = [("system", edit.system), ("archive", edit.archive)]
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(n, _)| *n)
            .collect();
        if !self.plus() && edit.accessed.is_some() {
            foreign.push("access date");
        }
        if !foreign.is_empty() {
            return Err(Error::Unsupported(format!(
                "the {} on HFS volumes (they have locked, invisible, and creation and modification dates)",
                foreign.join(" / ")
            )));
        }
        let plus = self.plus();
        let date = |s: &Option<String>| -> Result<Option<u32>> {
            s.as_deref()
                .map(|t| Ok(mac_time(crate::ext::parse_local_time(t)?, plus)))
                .transpose()
        };
        let ed = Ed {
            read_only: edit.read_only,
            hidden: edit.hidden,
            created: date(&edit.created)?,
            modified: date(&edit.modified)?,
            accessed: date(&edit.accessed)?,
        };
        self.change(|w| w.edit(path, &ed, recursive))
    }

    /// Consistency problems (empty when clean). A test aid.
    pub(crate) fn check(&self) -> Result<Vec<String>> {
        Ok(Writer::load(self)?.check())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_respects_node_size() {
        // 3 records of 100 bytes fit a 512-byte node (14 + 300 + 8), 5 do not.
        assert_eq!(pack(512, [100; 3].into_iter()), vec![0..3]);
        assert_eq!(pack(512, [100; 5].into_iter()), vec![0..4, 4..5]);
        assert!(pack(512, std::iter::empty()).is_empty());
    }

    #[test]
    fn extents_merge_up_to_the_limit() {
        let e = |start, count| Extent { start, count };
        let m = merge(&[e(1, 2), e(3, 4), e(10, 1)], 0xFFFF);
        assert_eq!(
            m.iter().map(|x| (x.start, x.count)).collect::<Vec<_>>(),
            vec![(1, 6), (10, 1)]
        );
        let m = merge(&[e(0, 0xFFF0), e(0xFFF0, 0x20)], 0xFFFF);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn macbinary_headers_are_recognised() {
        let mut h = [0u8; 128];
        h[1] = 3;
        h[2..5].copy_from_slice(b"a/b");
        h[65..69].copy_from_slice(b"TEXT");
        put_be32(&mut h, 83, 5);
        h[102..106].copy_from_slice(b"mBIN");
        let mb = parse_macbinary(&h, 256).unwrap();
        assert_eq!(mb.name, "a:b");
        assert_eq!(&mb.finfo[..4], b"TEXT");
        assert_eq!((mb.data_at, mb.data_len, mb.rsrc_at), (128, 5, 256));
        h[102] = 0;
        assert!(parse_macbinary(&h, 256).is_none());
        let crc = crc16_xmodem(&h[..124]);
        put_be16(&mut h, 124, crc);
        assert!(parse_macbinary(&h, 256).is_some());
    }
}
