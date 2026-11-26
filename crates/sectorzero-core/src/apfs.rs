//! APFS (Apple File System), read-only, written from Apple's "Apple File System Reference":
//! the container superblock and its checkpoint area, object maps, B-trees, volume superblocks
//! and the file-system records of one volume (inodes, folder entries, file extents, extended
//! attributes). Files that macOS compressed (decmpfs: zlib, LZVN or LZFSE, kept in the
//! attribute or in the resource fork) are decompressed. Encrypted and sealed volumes are
//! refused rather than read wrongly.
//!
//! A container can hold several volumes; the first is opened unless another is picked by
//! number or name. Every metadata block's Fletcher-64 checksum is checked. Names are shown as
//! stored (APFS keeps the normalization they were created with).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result, display};
use crate::util::{be32, le16, le32, le64, put_be16, put_be32};
use crate::volume::{
    Attributes, Entry, ForkMode, MacInfo, Stats, host_safe_name, join, split_parent,
};
use crate::window::Window;

// Object types (the low 16 bits of o_type).
const OBJ_TYPE_MASK: u32 = 0xFFFF;
const T_NX_SUPERBLOCK: u32 = 0x01;
const T_BTREE: u32 = 0x02;
const T_BTREE_NODE: u32 = 0x03;
const T_SPACEMAN: u32 = 0x05;
const T_OMAP: u32 = 0x0B;
const T_CHECKPOINT_MAP: u32 = 0x0C;
const T_FS: u32 = 0x0D;

// B-tree node flags.
const BTNODE_ROOT: u16 = 0x1;
const BTNODE_LEAF: u16 = 0x2;
const BTNODE_FIXED_KV_SIZE: u16 = 0x4;
/// Size of the btree_info_t at the end of a root node.
const BTREE_INFO: usize = 40;
/// Offset of btn_data in a B-tree node.
const BTN_DATA: usize = 56;

// Object map value flags.
const OMAP_VAL_DELETED: u32 = 0x1;
const OMAP_VAL_ENCRYPTED: u32 = 0x4;

// File-system record types (the top 4 bits of a key's obj_id_and_type).
const OBJ_ID_MASK: u64 = 0x0FFF_FFFF_FFFF_FFFF;
const TYPE_INODE: u8 = 3;
const TYPE_XATTR: u8 = 4;
const TYPE_FILE_EXTENT: u8 = 8;
const TYPE_DIR_REC: u8 = 9;

const ROOT_DIR: u64 = 2;

// Volume flags and features.
const FS_UNENCRYPTED: u64 = 0x1;
const INCOMPAT_CASE_INSENSITIVE: u64 = 0x1;
const INCOMPAT_SEALED_VOLUME: u64 = 0x20;

// Inodes.
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;
const S_IFLNK: u16 = 0o120000;
const UF_IMMUTABLE: u32 = 0x2;
const UF_COMPRESSED: u32 = 0x20;
const UF_HIDDEN: u32 = 0x8000;
const SF_IMMUTABLE: u32 = 0x20000;
const INO_EXT_TYPE_DSTREAM: u8 = 8;
const FINDER_INVISIBLE: u16 = 0x4000;

// Extended attributes.
const XATTR_DATA_STREAM: u16 = 0x1;
const XATTR_DATA_EMBEDDED: u16 = 0x2;
const DECMPFS: &str = "com.apple.decmpfs";
const RESOURCE_FORK: &str = "com.apple.ResourceFork";
const FINDER_INFO: &str = "com.apple.FinderInfo";
const SYMLINK: &str = "com.apple.fs.symlink";

/// decmpfs data in a resource fork is split into chunks of this many bytes.
const DECMPFS_CHUNK: u64 = 65536;
/// Blocks kept in the metadata cache before it is emptied.
const CACHE_BLOCKS: usize = 4096;
const MAX_DEPTH: usize = 16;

fn damaged() -> Error {
    Error::Invalid("damaged APFS volume".into())
}

/// The file system name for a volume whose first 2 KiB are `head`, if it is an APFS container.
pub(crate) fn fs_name(head: &[u8]) -> Option<&'static str> {
    (head.len() >= 40 && &head[32..36] == b"NXSB" && le32(head, 24) & OBJ_TYPE_MASK == 1)
        .then_some("APFS")
}

/// Fletcher-64 over a block, skipping the checksum field itself (Apple's object checksum).
fn fletcher64(b: &[u8]) -> u64 {
    const M: u64 = 0xFFFF_FFFF;
    let (mut s1, mut s2) = (0u64, 0u64);
    // Sums stay far below 2^64 between reductions for blocks up to 64 KiB.
    for chunk in b[8..].chunks(4096) {
        for w in chunk.chunks_exact(4) {
            s1 += u64::from(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
            s2 += s1;
        }
        s1 %= M;
        s2 %= M;
    }
    let c1 = M - ((s1 + s2) % M);
    let c2 = M - ((s1 + c1) % M);
    (c2 << 32) | c1
}

fn checksum_ok(b: &[u8]) -> bool {
    le64(b, 0) == fletcher64(b)
}

fn obj_type(b: &[u8]) -> u32 {
    le32(b, 24) & OBJ_TYPE_MASK
}

/// One volume of the container, as `info` lists it.
#[derive(Debug, Clone, Serialize)]
pub struct ApfsVolume {
    /// 1-based, in the container's order.
    pub index: usize,
    pub name: String,
    /// System, Data, Preboot, Recovery, VM, … or empty.
    pub role: String,
    pub encrypted: bool,
    pub case_sensitive: bool,
    pub files: u64,
    pub folders: u64,
    pub symlinks: u64,
    /// Bytes the volume has allocated.
    pub used_bytes: u64,
    /// The volume opened.
    pub open: bool,
}

/// The container around the opened volume.
#[derive(Debug, Clone, Serialize)]
pub struct ApfsInfo {
    pub block_size: u32,
    pub blocks: u64,
    pub free_blocks: Option<u64>,
    pub volumes: Vec<ApfsVolume>,
}

pub(crate) struct ApfsVolInfo {
    pub label: String,
    pub uuid: String,
    pub formatted_by: String,
    pub case_sensitive: bool,
    pub apfs: ApfsInfo,
}

/// A parsed B-tree node.
struct Node {
    b: Rc<Vec<u8>>,
    leaf: bool,
    fixed: bool,
    count: usize,
    toc: usize,
    keys: usize,
    vals_end: usize,
}

impl Node {
    fn parse(b: Rc<Vec<u8>>) -> Result<Node> {
        if !matches!(obj_type(&b), T_BTREE | T_BTREE_NODE) || b.len() < BTN_DATA + BTREE_INFO {
            return Err(damaged());
        }
        let flags = le16(&b, 32);
        let count = le32(&b, 36) as usize;
        let toc = BTN_DATA + usize::from(le16(&b, 40));
        let keys = toc + usize::from(le16(&b, 42));
        let vals_end = if flags & BTNODE_ROOT != 0 {
            b.len() - BTREE_INFO
        } else {
            b.len()
        };
        let fixed = flags & BTNODE_FIXED_KV_SIZE != 0;
        let entry = if fixed { 4 } else { 8 };
        if keys > vals_end || toc + count * entry > keys {
            return Err(damaged());
        }
        Ok(Node {
            b,
            leaf: flags & BTNODE_LEAF != 0,
            fixed,
            count,
            toc,
            keys,
            vals_end,
        })
    }

    /// Key and value of entry `i`; `kv` is the key and value size of fixed-size nodes.
    fn entry(&self, i: usize, kv: (usize, usize)) -> Result<(&[u8], &[u8])> {
        let b = &self.b;
        let (ko, kl, vo, vl) = if self.fixed {
            let e = self.toc + i * 4;
            let vl = if self.leaf { kv.1 } else { 8 };
            (le16(b, e), kv.0, le16(b, e + 2), vl)
        } else {
            let e = self.toc + i * 8;
            (
                le16(b, e),
                usize::from(le16(b, e + 2)),
                le16(b, e + 4),
                usize::from(le16(b, e + 6)),
            )
        };
        let k = self.keys + usize::from(ko);
        if k + kl > self.vals_end {
            return Err(damaged());
        }
        let key = &b[k..k + kl];
        if vo == 0xFFFF {
            return Ok((key, &[]));
        }
        let v = self
            .vals_end
            .checked_sub(usize::from(vo))
            .filter(|&v| v >= self.keys && v + vl <= self.vals_end)
            .ok_or_else(damaged)?;
        Ok((key, &b[v..v + vl]))
    }

    fn child(&self, i: usize, kv: (usize, usize)) -> Result<u64> {
        let (_, v) = self.entry(i, kv)?;
        if v.len() < 8 {
            return Err(damaged());
        }
        Ok(le64(v, 0))
    }
}

/// (object ID, record type) of a file-system key.
fn ident(k: &[u8]) -> (u64, u8) {
    if k.len() < 8 {
        return (0, 0);
    }
    let h = le64(k, 0);
    (h & OBJ_ID_MASK, (h >> 60) as u8)
}

#[derive(Clone)]
struct Child {
    name: String,
    id: u64,
}

struct Inode {
    private_id: u64,
    created: u64,
    modified: u64,
    accessed: u64,
    bsd_flags: u32,
    mode: u16,
    /// Size of the data stream, if the inode has one.
    size: Option<u64>,
}

impl Inode {
    fn kind(&self) -> u16 {
        self.mode & S_IFMT
    }
}

struct Xattr {
    name: String,
    flags: u16,
    data: Vec<u8>,
}

/// An inode and its extended attributes.
struct Object {
    ino: Inode,
    xattrs: Vec<Xattr>,
}

impl Object {
    fn xattr(&self, name: &str) -> Option<&Xattr> {
        self.xattrs.iter().find(|x| x.name == name)
    }

    fn compressed(&self) -> bool {
        self.ino.bsd_flags & UF_COMPRESSED != 0 && self.xattr(DECMPFS).is_some()
    }
}

pub(crate) struct ApfsFs {
    w: RefCell<Window>,
    bs: u64,
    blocks: u64,
    xid: u64,
    /// Root of the volume's object map B-tree (physical).
    omap: u64,
    /// The file-system tree's root (virtual).
    root: u64,
    /// Whether folder entries use hashed keys (case- or normalization-insensitive volumes).
    vol: ApfsVolInfo,
    cache: RefCell<HashMap<u64, Rc<Vec<u8>>>>,
    virt: RefCell<HashMap<u64, u64>>,
}

/// A volume superblock found through the container's object map.
struct VolRec {
    b: Rc<Vec<u8>>,
}

impl VolRec {
    fn name(&self) -> String {
        let n = &self.b[704..960];
        let end = n.iter().position(|&c| c == 0).unwrap_or(n.len());
        String::from_utf8_lossy(&n[..end]).into_owned()
    }

    fn flags(&self) -> u64 {
        le64(&self.b, 264)
    }

    fn incompat(&self) -> u64 {
        le64(&self.b, 56)
    }

    fn role(&self) -> String {
        match le16(&self.b, 964) {
            0x0001 => "System",
            0x0002 => "User",
            0x0004 => "Recovery",
            0x0008 => "VM",
            0x0010 => "Preboot",
            0x0020 => "Installer",
            0x0040 => "Data",
            0x0080 => "Baseband",
            0x00C0 => "Update",
            0x0180 => "Backup",
            _ => "",
        }
        .to_string()
    }

    fn case_sensitive(&self) -> bool {
        self.incompat() & INCOMPAT_CASE_INSENSITIVE == 0
    }
}

fn uuid_text(b: &[u8]) -> String {
    let h: Vec<String> = b.iter().map(|x| format!("{x:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

impl ApfsFs {
    /// Open volume `pick` (a 1-based number or a name; the first when `None`).
    pub(crate) fn open(w: Window, pick: Option<&str>) -> Result<ApfsFs> {
        let mut fs = ApfsFs {
            w: RefCell::new(w),
            bs: 4096,
            blocks: 0,
            xid: 0,
            omap: 0,
            root: 0,
            vol: ApfsVolInfo {
                label: String::new(),
                uuid: String::new(),
                formatted_by: String::new(),
                case_sensitive: false,
                apfs: ApfsInfo {
                    block_size: 0,
                    blocks: 0,
                    free_blocks: None,
                    volumes: Vec::new(),
                },
            },
            cache: RefCell::new(HashMap::new()),
            virt: RefCell::new(HashMap::new()),
        };
        let mut b0 = vec![0u8; 4096];
        fs.w.borrow_mut().read_at(0, &mut b0)?;
        if fs_name(&b0).is_none() {
            return Err(damaged());
        }
        let bs = le32(&b0, 36);
        if !(4096..=65536).contains(&bs) || !bs.is_power_of_two() {
            return Err(damaged());
        }
        fs.bs = u64::from(bs);
        if fs.bs != 4096 {
            b0 = vec![0u8; bs as usize];
            fs.w.borrow_mut().read_at(0, &mut b0)?;
        }
        let nx = fs.newest_superblock(b0)?;
        fs.blocks = le64(&nx, 40);
        fs.xid = le64(&nx, 16);
        let container_omap = fs.omap_root(le64(&nx, 160))?;

        // The volumes, in the container's order.
        let max = (le32(&nx, 180) as usize).clamp(1, 100);
        let mut vols = Vec::new();
        for i in 0..max {
            let oid = le64(&nx, 184 + i * 8);
            if oid == 0 {
                continue;
            }
            let Some(addr) = fs.omap_lookup(container_omap, oid)? else {
                continue;
            };
            let b = fs.block(addr)?;
            if obj_type(&b) != T_FS || &b[32..36] != b"APSB" {
                return Err(damaged());
            }
            vols.push(VolRec { b });
        }
        if vols.is_empty() {
            return Err(Error::Invalid("the APFS container has no volumes".into()));
        }
        let chosen = match pick {
            None => 0,
            Some(p) => {
                let by_number = p
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=vols.len()).contains(n))
                    .map(|n| n - 1);
                let by_name = || {
                    vols.iter().position(|v| v.name() == p).or_else(|| {
                        vols.iter()
                            .position(|v| v.name().to_lowercase() == p.to_lowercase())
                    })
                };
                match by_number.or_else(by_name) {
                    Some(i) => i,
                    None => {
                        let list: Vec<String> = vols
                            .iter()
                            .enumerate()
                            .map(|(i, v)| format!("{} {}", i + 1, v.name()))
                            .collect();
                        return Err(Error::NotFound(format!(
                            "APFS volume {p} (the container has {})",
                            list.join(", ")
                        )));
                    }
                }
            }
        };
        let free_blocks = fs.free_blocks(&nx).ok().flatten();
        let volumes = vols
            .iter()
            .enumerate()
            .map(|(i, v)| ApfsVolume {
                index: i + 1,
                name: v.name(),
                role: v.role(),
                encrypted: v.flags() & FS_UNENCRYPTED == 0,
                case_sensitive: v.case_sensitive(),
                files: le64(&v.b, 184),
                folders: le64(&v.b, 192),
                symlinks: le64(&v.b, 200),
                used_bytes: le64(&v.b, 88).saturating_mul(fs.bs),
                open: i == chosen,
            })
            .collect();
        let v = &vols[chosen];
        let formatted = &v.b[272..304];
        let end = formatted.iter().position(|&c| c == 0).unwrap_or(32);
        fs.vol = ApfsVolInfo {
            label: v.name(),
            uuid: uuid_text(&v.b[240..256]),
            formatted_by: String::from_utf8_lossy(&formatted[..end])
                .trim()
                .to_string(),
            case_sensitive: v.case_sensitive(),
            apfs: ApfsInfo {
                block_size: bs,
                blocks: fs.blocks,
                free_blocks,
                volumes,
            },
        };
        if v.flags() & FS_UNENCRYPTED == 0 {
            return Err(Error::Unsupported(
                "encrypted APFS volumes (another volume can be opened with --volume)".into(),
            ));
        }
        if v.incompat() & INCOMPAT_SEALED_VOLUME != 0 {
            return Err(Error::Unsupported("sealed APFS system volumes".into()));
        }
        fs.omap = fs.omap_root(le64(&v.b, 128))?;
        let root_oid = le64(&v.b, 136);
        fs.root = root_oid;
        // The root tree must resolve now, or the volume is unreadable.
        fs.virtual_block(root_oid)?;
        Ok(fs)
    }

    /// The newest valid container superblock in the checkpoint descriptor area, or block 0's.
    fn newest_superblock(&self, b0: Vec<u8>) -> Result<Rc<Vec<u8>>> {
        let b0 = Rc::new(b0);
        let desc_blocks = le32(&b0, 104);
        let base = le64(&b0, 112);
        let mut best: Option<Rc<Vec<u8>>> = checksum_ok(&b0).then(|| b0.clone());
        // A descriptor area kept in a B-tree (top bit set) is not scanned.
        if desc_blocks & 0x8000_0000 == 0 {
            for i in 0..u64::from(desc_blocks.min(65536)) {
                let Ok(b) = self.read_block(base + i) else {
                    continue;
                };
                if obj_type(&b) != T_NX_SUPERBLOCK || &b[32..36] != b"NXSB" || !checksum_ok(&b) {
                    continue;
                }
                if best.as_ref().is_none_or(|x| le64(&b, 16) > le64(x, 16)) {
                    best = Some(Rc::new(b));
                }
            }
        }
        best.ok_or_else(damaged)
    }

    /// Free blocks, from the space manager found through the checkpoint map.
    fn free_blocks(&self, nx: &[u8]) -> Result<Option<u64>> {
        let sm_oid = le64(nx, 152);
        let desc_blocks = u64::from(le32(nx, 104) & 0x7FFF_FFFF);
        let base = le64(nx, 112);
        let (index, len) = (u64::from(le32(nx, 136)), u64::from(le32(nx, 140)));
        if desc_blocks == 0 || le32(nx, 104) & 0x8000_0000 != 0 {
            return Ok(None);
        }
        for k in 0..len.min(desc_blocks) {
            let b = self.read_block(base + (index + k) % desc_blocks)?;
            if obj_type(&b) != T_CHECKPOINT_MAP || !checksum_ok(&b) {
                continue;
            }
            let count = (le32(&b, 36) as usize).min((b.len() - 40) / 40);
            for e in 0..count {
                let at = 40 + e * 40;
                if le64(&b, at + 24) == sm_oid {
                    let sm = self.read_block(le64(&b, at + 32))?;
                    if obj_type(&sm) != T_SPACEMAN || !checksum_ok(&sm) {
                        return Ok(None);
                    }
                    return Ok(Some(le64(&sm, 72)));
                }
            }
        }
        Ok(None)
    }

    fn read_block(&self, addr: u64) -> Result<Vec<u8>> {
        let w = &mut *self.w.borrow_mut();
        let at = addr.checked_mul(self.bs).ok_or_else(damaged)?;
        if (self.blocks != 0 && addr >= self.blocks) || at + self.bs > w.len() {
            return Err(damaged());
        }
        let mut b = vec![0u8; self.bs as usize];
        w.read_at(at, &mut b)?;
        Ok(b)
    }

    /// A metadata block, checksum checked, through the cache.
    fn block(&self, addr: u64) -> Result<Rc<Vec<u8>>> {
        if let Some(b) = self.cache.borrow().get(&addr) {
            return Ok(b.clone());
        }
        let b = self.read_block(addr)?;
        if !checksum_ok(&b) {
            return Err(damaged());
        }
        let b = Rc::new(b);
        let mut c = self.cache.borrow_mut();
        if c.len() >= CACHE_BLOCKS {
            c.clear();
        }
        c.insert(addr, b.clone());
        Ok(b)
    }

    /// The B-tree root of the object map at `addr`.
    fn omap_root(&self, addr: u64) -> Result<u64> {
        let b = self.block(addr)?;
        if obj_type(&b) != T_OMAP {
            return Err(damaged());
        }
        Ok(le64(&b, 48))
    }

    /// The physical address of virtual object `oid` in the newest version up to our
    /// transaction, from the object map B-tree at `root`.
    fn omap_lookup(&self, root: u64, oid: u64) -> Result<Option<u64>> {
        let want = (oid, self.xid);
        let mut addr = root;
        for _ in 0..MAX_DEPTH {
            let node = Node::parse(self.block(addr)?)?;
            let (mut lo, mut hi) = (0, node.count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let (k, _) = node.entry(mid, (16, 16))?;
                if (le64(k, 0), le64(k, 8)) <= want {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if lo == 0 {
                return Ok(None);
            }
            if !node.leaf {
                addr = node.child(lo - 1, (16, 16))?;
                continue;
            }
            let (k, v) = node.entry(lo - 1, (16, 16))?;
            if le64(k, 0) != oid || v.len() < 16 {
                return Ok(None);
            }
            let flags = le32(v, 0);
            if flags & OMAP_VAL_DELETED != 0 {
                return Ok(None);
            }
            if flags & OMAP_VAL_ENCRYPTED != 0 {
                return Err(Error::Unsupported("encrypted APFS volumes".into()));
            }
            return Ok(Some(le64(v, 8)));
        }
        Err(damaged())
    }

    /// A node of the file-system tree by its virtual object ID.
    fn virtual_block(&self, oid: u64) -> Result<Rc<Vec<u8>>> {
        let cached = self.virt.borrow().get(&oid).copied();
        let addr = match cached {
            Some(a) => a,
            None => {
                let a = self.omap_lookup(self.omap, oid)?.ok_or_else(damaged)?;
                self.virt.borrow_mut().insert(oid, a);
                a
            }
        };
        self.block(addr)
    }

    /// File-system records whose (object ID, type) lies in `lo..=hi`, in key order.
    fn scan(&self, lo: (u64, u8), hi: (u64, u8)) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        self.scan_node(self.root, lo, hi, &mut out, 0)?;
        Ok(out)
    }

    fn scan_node(
        &self,
        oid: u64,
        lo: (u64, u8),
        hi: (u64, u8),
        out: &mut Vec<(Vec<u8>, Vec<u8>)>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(damaged());
        }
        let node = Node::parse(self.virtual_block(oid)?)?;
        // The first entry whose key is at or past `lo`.
        let (mut a, mut z) = (0, node.count);
        while a < z {
            let mid = (a + z) / 2;
            if ident(node.entry(mid, (0, 0))?.0) < lo {
                a = mid + 1;
            } else {
                z = mid;
            }
        }
        if node.leaf {
            for i in a..node.count {
                let (k, v) = node.entry(i, (0, 0))?;
                if ident(k) > hi {
                    break;
                }
                out.push((k.to_vec(), v.to_vec()));
            }
            return Ok(());
        }
        // A child holds keys from its own key up to the next child's, so records equal to
        // `lo` may start in the child before the first one whose key reaches `lo`.
        for i in a.saturating_sub(1)..node.count {
            let (k, _) = node.entry(i, (0, 0))?;
            if ident(k) > hi {
                break;
            }
            let child = node.child(i, (0, 0))?;
            self.scan_node(child, lo, hi, out, depth + 1)?;
        }
        Ok(())
    }

    fn children(&self, dir: u64) -> Result<Vec<Child>> {
        let mut out = Vec::new();
        for (k, v) in self.scan((dir, TYPE_DIR_REC), (dir, TYPE_DIR_REC))? {
            if v.len() < 18 {
                return Err(damaged());
            }
            // Hashed keys (length and hash in 32 bits) or plain ones (16-bit length).
            let name = if k.len() >= 12 && 12 + (le32(&k, 8) & 0x3FF) as usize == k.len() {
                &k[12..]
            } else if k.len() >= 10 && 10 + usize::from(le16(&k, 8)) == k.len() {
                &k[10..]
            } else {
                return Err(damaged());
            };
            let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            out.push(Child {
                name: String::from_utf8_lossy(&name[..end]).into_owned(),
                id: le64(&v, 0),
            });
        }
        Ok(out)
    }

    fn object(&self, id: u64) -> Result<Object> {
        let mut ino = None;
        let mut xattrs = Vec::new();
        for (k, v) in self.scan((id, TYPE_INODE), (id, TYPE_XATTR))? {
            match ident(&k).1 {
                TYPE_INODE => ino = Some(parse_inode(&v)?),
                TYPE_XATTR => {
                    if k.len() < 10 || v.len() < 4 {
                        return Err(damaged());
                    }
                    let n = &k[10..(10 + usize::from(le16(&k, 8))).min(k.len())];
                    let end = n.iter().position(|&c| c == 0).unwrap_or(n.len());
                    let len = usize::from(le16(&v, 2)).min(v.len() - 4);
                    xattrs.push(Xattr {
                        name: String::from_utf8_lossy(&n[..end]).into_owned(),
                        flags: le16(&v, 0),
                        data: v[4..4 + len].to_vec(),
                    });
                }
                _ => {}
            }
        }
        Ok(Object {
            ino: ino.ok_or_else(damaged)?,
            xattrs,
        })
    }

    /// The contents of an extended attribute (kept in the record or in a data stream).
    fn xattr_data(&self, x: &Xattr) -> Result<Vec<u8>> {
        if x.flags & XATTR_DATA_EMBEDDED != 0 || x.flags & XATTR_DATA_STREAM == 0 {
            return Ok(x.data.clone());
        }
        if x.data.len() < 16 {
            return Err(damaged());
        }
        let mut out = Vec::new();
        self.read_stream(le64(&x.data, 0), le64(&x.data, 8), &mut out)?;
        Ok(out)
    }

    fn xattr_size(x: &Xattr) -> u64 {
        if x.flags & XATTR_DATA_STREAM != 0 && x.data.len() >= 16 {
            le64(&x.data, 8)
        } else {
            x.data.len() as u64
        }
    }

    /// Copy `size` bytes of data stream `id` (its file extents; holes read as zeros).
    fn read_stream(&self, id: u64, size: u64, out: &mut dyn Write) -> Result<u64> {
        let mut pos = 0u64;
        for (k, v) in self.scan((id, TYPE_FILE_EXTENT), (id, TYPE_FILE_EXTENT))? {
            if k.len() < 16 || v.len() < 16 {
                return Err(damaged());
            }
            let logical = le64(&k, 8);
            let len = le64(&v, 0) & 0x00FF_FFFF_FFFF_FFFF;
            let phys = le64(&v, 8);
            if logical >= size {
                break;
            }
            if logical < pos {
                return Err(damaged());
            }
            write_zeros(out, logical - pos)?;
            let take = len.min(size - logical);
            if phys == 0 {
                write_zeros(out, take)?;
            } else {
                self.copy_blocks(phys, take, out)?;
            }
            pos = logical + take;
        }
        write_zeros(out, size - pos)?;
        Ok(size)
    }

    fn copy_blocks(&self, phys: u64, len: u64, out: &mut dyn Write) -> Result<()> {
        let start = phys.checked_mul(self.bs).ok_or_else(damaged)?;
        let w = &mut *self.w.borrow_mut();
        if start.checked_add(len).is_none_or(|e| e > w.len()) {
            return Err(damaged());
        }
        let mut buf = vec![0u8; len.min(1 << 20) as usize];
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(buf.len() as u64) as usize;
            w.read_at(start + done, &mut buf[..n])?;
            out.write_all(&buf[..n])?;
            done += n as u64;
        }
        Ok(())
    }

    fn file_size(&self, o: &Object) -> u64 {
        if o.compressed() {
            let h = &o.xattr(DECMPFS).map(|x| x.data.as_slice()).unwrap_or(&[]);
            if h.len() >= 16 {
                return le64(h, 8);
            }
        }
        if o.ino.kind() == S_IFLNK {
            return o
                .xattr(SYMLINK)
                .map_or(0, |x| Self::xattr_size(x).saturating_sub(1));
        }
        o.ino.size.unwrap_or(0)
    }

    /// The data fork, decompressed if macOS compressed it.
    fn copy_data(&self, o: &Object, out: &mut dyn Write) -> Result<u64> {
        if o.compressed() {
            let header = self.xattr_data(o.xattr(DECMPFS).ok_or_else(damaged)?)?;
            return self.decompress(o, &header, out);
        }
        if o.ino.kind() == S_IFLNK {
            let t = self.link_target(o)?;
            out.write_all(&t)?;
            return Ok(t.len() as u64);
        }
        match o.ino.size {
            Some(s) => self.read_stream(o.ino.private_id, s, out),
            None => Ok(0),
        }
    }

    fn link_target(&self, o: &Object) -> Result<Vec<u8>> {
        let mut t = self.xattr_data(o.xattr(SYMLINK).ok_or_else(damaged)?)?;
        while t.last() == Some(&0) {
            t.pop();
        }
        Ok(t)
    }

    /// A decmpfs-compressed file: the header in `com.apple.decmpfs`, the data after it or in
    /// the resource fork.
    fn decompress(&self, o: &Object, h: &[u8], out: &mut dyn Write) -> Result<u64> {
        if h.len() < 16 || &h[0..4] != b"fpmc" {
            return Err(damaged());
        }
        let kind = le32(h, 4);
        let size = le64(h, 8);
        let inline = &h[16..];
        let data = match kind {
            1 | 9 => inline.to_vec(),
            3 => zlib_chunk(inline, size)?,
            7 => lzvn_chunk(inline, size)?,
            11 => lzfse_chunk(inline, size)?,
            4 | 8 | 10 | 12 => {
                let rsrc = self.xattr_data(o.xattr(RESOURCE_FORK).ok_or_else(damaged)?)?;
                return write_rsrc_chunks(kind, &rsrc, size, out);
            }
            5 => {
                return Err(Error::Unsupported(
                    "dataless APFS files (their data is in the cloud)".into(),
                ));
            }
            13 | 14 => return Err(Error::Unsupported("LZBITMAP-compressed APFS files".into())),
            _ => {
                return Err(Error::Unsupported(format!(
                    "APFS files compressed with method {kind}"
                )));
            }
        };
        if data.len() as u64 != size {
            return Err(damaged());
        }
        out.write_all(&data)?;
        Ok(size)
    }

    /// Finder info (32 bytes, zeros when there is none) and the resource fork size a user
    /// sees (not the one holding compressed data).
    fn mac_parts<'o>(&self, o: &'o Object) -> ([u8; 32], Option<&'o Xattr>) {
        let mut finder = [0u8; 32];
        if let Some(x) = o.xattr(FINDER_INFO) {
            let n = x.data.len().min(32);
            finder[..n].copy_from_slice(&x.data[..n]);
        }
        let rsrc = o.xattr(RESOURCE_FORK).filter(|_| !o.compressed());
        (finder, rsrc)
    }

    pub(crate) fn info(&self) -> &ApfsVolInfo {
        &self.vol
    }

    pub(crate) fn label(&self) -> String {
        self.vol.label.clone()
    }

    /// The entry at `path` ("" is the root).
    fn lookup(&self, path: &str) -> Result<Option<Child>> {
        let mut cur = Child {
            name: String::new(),
            id: ROOT_DIR,
        };
        if path.is_empty() {
            return Ok(Some(cur));
        }
        let parts: Vec<&str> = path.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            let kids = self.children(cur.id)?;
            let want: String = part.nfc().collect();
            let found = kids
                .iter()
                .find(|c| c.name == *part)
                .or_else(|| kids.iter().find(|c| c.name.nfc().eq(want.chars())))
                .or_else(|| {
                    let lower = want.to_lowercase();
                    kids.iter()
                        .find(|c| c.name.nfc().collect::<String>().to_lowercase() == lower)
                });
            let Some(c) = found else {
                return Ok(None);
            };
            if i + 1 < parts.len() && self.object(c.id)?.ino.kind() != S_IFDIR {
                return Ok(None);
            }
            cur = c.clone();
        }
        Ok(Some(cur))
    }

    fn entry(&self, parent: &str, c: &Child) -> Result<Entry> {
        let o = self.object(c.id)?;
        let is_dir = o.ino.kind() == S_IFDIR;
        let (finder, rsrc) = self.mac_parts(&o);
        let resource_size = rsrc.map_or(0, Self::xattr_size);
        let mac =
            (!is_dir && (resource_size > 0 || finder.iter().any(|&b| b != 0))).then(|| MacInfo {
                file_type: crate::hfs::mac_roman_to_string(&finder[0..4]),
                creator: crate::hfs::mac_roman_to_string(&finder[4..8]),
                resource_size,
            });
        let flags = o.ino.bsd_flags;
        Ok(Entry {
            path: join(parent, &c.name),
            short_name: c.name.clone(),
            name: c.name.clone(),
            is_dir,
            size: if is_dir { 0 } else { self.file_size(&o) },
            modified: format_ns(o.ino.modified, true),
            created: format_ns(o.ino.created, true),
            accessed: format_ns(o.ino.accessed, false),
            attributes: Attributes {
                read_only: flags & (UF_IMMUTABLE | SF_IMMUTABLE) != 0,
                hidden: flags & UF_HIDDEN != 0
                    || u16::from_be_bytes([finder[8], finder[9]]) & FINDER_INVISIBLE != 0,
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
        if self.object(c.id)?.ino.kind() != S_IFDIR {
            return Err(Error::Invalid(format!("not a folder: {}", display(dir))));
        }
        let mut out = self
            .children(c.id)?
            .iter()
            .map(|k| self.entry(dir, k))
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

    pub(crate) fn copy_out(&self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let c = self
            .lookup(path)?
            .ok_or_else(|| Error::NotFound(display(path)))?;
        let o = self.object(c.id)?;
        if o.ino.kind() == S_IFDIR {
            return Err(Error::Invalid(format!("{} is a folder", display(path))));
        }
        self.copy_data(&o, out)
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
        if src.is_empty() {
            self.extract_dir(c.id, dest, overwrite, forks, &mut stats)?;
        } else {
            self.extract_one(&c, dest, overwrite, forks, &mut stats)?;
        }
        Ok(stats)
    }

    fn extract_dir(
        &self,
        id: u64,
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
        let target = dest.join(&name);
        let o = self.object(c.id)?;
        let create = |p: &Path| -> Result<File> {
            if !overwrite && p.symlink_metadata().is_ok() {
                return Err(Error::Exists(p.display().to_string()));
            }
            Ok(File::create(p)?)
        };
        let stamp = |f: &File| {
            let _ = f.set_modified(UNIX_EPOCH + Duration::from_nanos(o.ino.modified));
        };
        match o.ino.kind() {
            S_IFDIR => {
                fs::create_dir_all(&target)?;
                stats.dirs += 1;
                self.extract_dir(c.id, &target, overwrite, forks, stats)
            }
            S_IFREG => {
                let (finder, rsrc) = self.mac_parts(&o);
                if forks == ForkMode::MacBinary {
                    let rsrc = match rsrc {
                        Some(x) => self.xattr_data(x)?,
                        None => Vec::new(),
                    };
                    let mut out = create(&dest.join(format!("{name}.bin")))?;
                    stats.bytes += self.write_macbinary(&c.name, &o, &finder, &rsrc, &mut out)?;
                    stats.files += 1;
                    stamp(&out);
                    return Ok(());
                }
                let mut out = create(&target)?;
                stats.bytes += self.copy_data(&o, &mut out)?;
                stats.files += 1;
                stamp(&out);
                if forks == ForkMode::AppleDouble
                    && (rsrc.is_some() || finder.iter().any(|&b| b != 0))
                {
                    let rsrc = match rsrc {
                        Some(x) => self.xattr_data(x)?,
                        None => Vec::new(),
                    };
                    let mut out = create(&dest.join(format!("._{name}")))?;
                    let h = appledouble(&finder, &rsrc);
                    out.write_all(&h)?;
                    stats.bytes += h.len() as u64;
                    stamp(&out);
                }
                Ok(())
            }
            S_IFLNK => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    let t = self.link_target(&o)?;
                    if target.symlink_metadata().is_ok() {
                        if !overwrite {
                            return Err(Error::Exists(target.display().to_string()));
                        }
                        let _ = fs::remove_file(&target);
                    }
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&t), &target)?;
                    stats.files += 1;
                }
                Ok(())
            }
            // Device nodes, FIFOs and sockets have no contents to copy.
            _ => Ok(()),
        }
    }

    /// A MacBinary III file: a 128-byte header with the name, Finder information and dates,
    /// then the data and resource forks, each padded to 128 bytes.
    fn write_macbinary(
        &self,
        name: &str,
        o: &Object,
        finder: &[u8; 32],
        rsrc: &[u8],
        out: &mut dyn Write,
    ) -> Result<u64> {
        let mut data = Vec::new();
        self.copy_data(o, &mut data)?;
        let mut h = [0u8; 128];
        let mac_name = name.replace(':', "/");
        let mut n = crate::hfs::string_to_mac_roman(&mac_name).unwrap_or_else(|| {
            name.chars()
                .map(|c| if c.is_ascii() { c as u8 } else { b'_' })
                .collect()
        });
        n.truncate(63);
        h[1] = n.len() as u8;
        h[2..2 + n.len()].copy_from_slice(&n);
        h[65..73].copy_from_slice(&finder[..8]);
        h[73] = finder[8];
        h[75..81].copy_from_slice(&finder[10..16]);
        h[81] = u8::from(o.ino.bsd_flags & (UF_IMMUTABLE | SF_IMMUTABLE) != 0);
        put_be32(&mut h, 83, data.len() as u32);
        put_be32(&mut h, 87, rsrc.len() as u32);
        let local = |ns: u64| crate::hfs::mac_time(UNIX_EPOCH + Duration::from_nanos(ns), false);
        put_be32(&mut h, 91, local(o.ino.created));
        put_be32(&mut h, 95, local(o.ino.modified));
        h[101] = finder[9];
        h[102..106].copy_from_slice(b"mBIN");
        h[106] = finder[24];
        h[107] = finder[25];
        h[122] = 130;
        h[123] = 129;
        let crc = crate::hfs::crc16_xmodem(&h[..124]);
        put_be16(&mut h, 124, crc);
        out.write_all(&h)?;
        let mut total = 128u64;
        for fork in [&data[..], rsrc] {
            out.write_all(fork)?;
            let pad = (128 - fork.len() % 128) % 128;
            out.write_all(&vec![0u8; pad])?;
            total += (fork.len() + pad) as u64;
        }
        Ok(total)
    }
}

fn parse_inode(v: &[u8]) -> Result<Inode> {
    if v.len() < 92 {
        return Err(damaged());
    }
    let mut size = None;
    if v.len() >= 96 {
        let x = &v[92..];
        let n = usize::from(le16(x, 0));
        let mut data = 4 + n * 4;
        for i in 0..n {
            let at = 4 + i * 4;
            if at + 4 > x.len() {
                break;
            }
            let (t, len) = (x[at], usize::from(le16(x, at + 2)));
            if data + len > x.len() {
                break;
            }
            if t == INO_EXT_TYPE_DSTREAM && len >= 8 {
                size = Some(le64(x, data));
            }
            data += (len + 7) & !7;
        }
    }
    Ok(Inode {
        private_id: le64(v, 8),
        created: le64(v, 16),
        modified: le64(v, 24),
        accessed: le64(v, 40),
        bsd_flags: le32(v, 68),
        mode: le16(v, 80),
        size,
    })
}

fn write_zeros(out: &mut dyn Write, mut n: u64) -> Result<()> {
    const Z: [u8; 65536] = [0u8; 65536];
    while n > 0 {
        let k = n.min(Z.len() as u64) as usize;
        out.write_all(&Z[..k])?;
        n -= k as u64;
    }
    Ok(())
}

/// One zlib chunk of decmpfs data (a first byte with the low nibble 0xF marks stored data).
fn zlib_chunk(c: &[u8], want: u64) -> Result<Vec<u8>> {
    if c.first().is_some_and(|&b| b & 0x0F == 0x0F) {
        return Ok(c[1..].to_vec());
    }
    let mut out = Vec::with_capacity(want as usize);
    flate2::read::ZlibDecoder::new(c)
        .take(want + 1)
        .read_to_end(&mut out)
        .map_err(|_| damaged())?;
    Ok(out)
}

/// One LZVN chunk (a first byte of 0x06, LZVN's end mark, means stored data). The raw LZVN
/// stream is decoded as the LZVN block of an LZFSE stream.
fn lzvn_chunk(c: &[u8], want: u64) -> Result<Vec<u8>> {
    if c.first() == Some(&0x06) {
        return Ok(c[1..].to_vec());
    }
    let mut s = Vec::with_capacity(c.len() + 16);
    s.extend_from_slice(b"bvxn");
    s.extend_from_slice(&(want as u32).to_le_bytes());
    s.extend_from_slice(&(c.len() as u32).to_le_bytes());
    s.extend_from_slice(c);
    s.extend_from_slice(b"bvx$");
    let mut out = Vec::with_capacity(want as usize);
    lzfse_rust::decode_bytes(&s, &mut out).map_err(|_| damaged())?;
    Ok(out)
}

/// One LZFSE chunk (a whole LZFSE stream; anything else is stored data after a marker byte).
fn lzfse_chunk(c: &[u8], want: u64) -> Result<Vec<u8>> {
    if !c.starts_with(b"bvx") {
        return Ok(c.get(1..).unwrap_or(&[]).to_vec());
    }
    let mut out = Vec::with_capacity(want as usize);
    lzfse_rust::decode_bytes(c, &mut out).map_err(|_| damaged())?;
    Ok(out)
}

/// decmpfs data kept in the resource fork, in 64 KiB chunks: zlib (type 4, in a classic
/// resource fork with a table of chunks) or LZVN, stored or LZFSE (types 8, 10 and 12, after a
/// table of chunk offsets).
fn write_rsrc_chunks(kind: u32, r: &[u8], size: u64, out: &mut dyn Write) -> Result<u64> {
    let chunks = size.div_ceil(DECMPFS_CHUNK) as usize;
    let mut spans = Vec::with_capacity(chunks);
    if kind == 4 {
        if r.len() < 0x10 {
            return Err(damaged());
        }
        let data = be32(r, 0) as usize;
        let table = data + 4;
        if table + 4 > r.len() {
            return Err(damaged());
        }
        let n = le32(r, table) as usize;
        if n < chunks || table + 4 + n * 8 > r.len() {
            return Err(damaged());
        }
        for i in 0..chunks {
            let off = table + le32(r, table + 4 + i * 8) as usize;
            let len = le32(r, table + 8 + i * 8) as usize;
            spans.push((off, off + len));
        }
    } else {
        if r.len() < 4 || (le32(r, 0) as usize) < (chunks + 1) * 4 {
            return Err(damaged());
        }
        for i in 0..chunks {
            spans.push((le32(r, i * 4) as usize, le32(r, i * 4 + 4) as usize));
        }
    }
    for (i, (a, z)) in spans.into_iter().enumerate() {
        if a > z || z > r.len() {
            return Err(damaged());
        }
        let c = &r[a..z];
        let want = (size - i as u64 * DECMPFS_CHUNK).min(DECMPFS_CHUNK);
        let data = match kind {
            4 => zlib_chunk(c, want)?,
            8 => lzvn_chunk(c, want)?,
            12 => lzfse_chunk(c, want)?,
            _ => c.get(1..).unwrap_or(&[]).to_vec(),
        };
        if data.len() as u64 != want {
            return Err(damaged());
        }
        out.write_all(&data)?;
    }
    Ok(size)
}

/// An AppleDouble header file (version 2) with Finder info (entry 9) and the resource fork
/// (entry 2), as macOS writes `._name` files on disks that cannot hold them.
fn appledouble(finder: &[u8; 32], rsrc: &[u8]) -> Vec<u8> {
    let header = 26 + 2 * 12;
    let mut h = vec![0u8; header + 32];
    put_be32(&mut h, 0, 0x0005_1607);
    put_be32(&mut h, 4, 0x0002_0000);
    put_be16(&mut h, 24, 2);
    for (i, (id, off, len)) in [
        (9u32, header as u32, 32u32),
        (2, (header + 32) as u32, rsrc.len() as u32),
    ]
    .into_iter()
    .enumerate()
    {
        put_be32(&mut h, 26 + i * 12, id);
        put_be32(&mut h, 30 + i * 12, off);
        put_be32(&mut h, 34 + i * 12, len);
    }
    h[header..header + 32].copy_from_slice(finder);
    h.extend_from_slice(rsrc);
    h
}

/// `YYYY-MM-DD HH:MM:SS` (or the date only) in local time, from nanoseconds since 1970.
fn format_ns(ns: u64, with_time: bool) -> String {
    if ns == 0 {
        return String::new();
    }
    let secs = (ns / 1_000_000_000) as i64;
    match Local.timestamp_opt(secs, 0).single() {
        Some(t) if with_time => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        Some(t) => t.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fletcher_matches_a_stored_checksum() {
        let mut b = vec![0u8; 4096];
        for (i, x) in b.iter_mut().enumerate().skip(8) {
            *x = (i * 7 + 3) as u8;
        }
        let c = fletcher64(&b);
        b[..8].copy_from_slice(&c.to_le_bytes());
        assert!(checksum_ok(&b));
        b[100] ^= 1;
        assert!(!checksum_ok(&b));
    }

    #[test]
    fn lzvn_chunks_decode_through_lzfse() {
        // lzfse_rust writes LZVN blocks for small inputs: take the payload out of one.
        let text = b"compressible compressible compressible text, compressible text".repeat(20);
        let mut enc = Vec::new();
        lzfse_rust::encode_bytes(&text, &mut enc).unwrap();
        assert_eq!(&enc[..4], b"bvxn", "expected an LZVN block");
        let payload = le32(&enc, 8) as usize;
        let lzvn = &enc[12..12 + payload];
        assert_eq!(lzvn_chunk(lzvn, text.len() as u64).unwrap(), text);
        let mut stored = vec![0x06];
        stored.extend_from_slice(b"plain");
        assert_eq!(lzvn_chunk(&stored, 5).unwrap(), b"plain");
    }

    #[test]
    fn resource_fork_chunks() {
        // Type 8 layout with two stored-LZVN chunks of a 65536 + 3 byte file.
        let a = vec![7u8; 65536];
        let mut r = Vec::new();
        let first = 12u32;
        let second = first + 1 + 65536;
        let end = second + 1 + 3;
        for v in [first, second, end] {
            r.extend_from_slice(&v.to_le_bytes());
        }
        r.push(0x06);
        r.extend_from_slice(&a);
        r.push(0x06);
        r.extend_from_slice(b"xyz");
        let mut out = Vec::new();
        write_rsrc_chunks(8, &r, 65539, &mut out).unwrap();
        assert_eq!(out.len(), 65539);
        assert_eq!(&out[65536..], b"xyz");
    }
}
