//! ext2, ext3 and ext4 file systems.
//!
//! Reading covers all three: classic block maps and ext4 extent trees, linear and hashed
//! (htree) directories, 32- and 64-bit group descriptors, meta_bg and flex_bg layouts.
//!
//! Writing covers ext2, ext3 and the ext4 feature set mke2fs uses by default: extents
//! (new files get minimal-depth extent trees), 64-bit descriptors, flex_bg, uninitialized
//! groups (uninit_bg or metadata_csum), metadata checksums on everything this module
//! writes, huge_file and dir_nlink. Journals are handled the way offline tools handle
//! them: the journal must be clean (as after a normal unmount) and is left clean. A hashed
//! directory that gets modified becomes a plain linear directory (with metadata_csum its
//! index blocks are rewritten as empty, checksummed leaves), which Linux and e2fsck accept
//! (`e2fsck -D` rebuilds the index). Images using other features (inline data,
//! encryption, bigalloc, quotas, casefolding, meta_bg, …) open read-only.
//!
//! Formatting makes ext2, ext3 or ext4 (extents and metadata_csum, without flex_bg or
//! 64bit, which only matter for very large volumes).
//!
//! Written from the published on-disk format description (the Linux kernel's
//! Documentation/filesystems/ext4); no code from e2fsprogs or Linux.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use serde::Serialize;

use crate::error::{Error, Result, display};
use crate::fatraw::EntryEdit;
use crate::util::*;
use crate::volume::{Attributes, Entry, Stats, host_safe_name, join, split_parent};
use crate::window::Window;

const MAGIC: u16 = 0xEF53;
const ROOT: u32 = 2;
const JOURNAL_INO: u32 = 8;
const SB_OFFSET: u64 = 1024;

const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_EXT_ATTR: u32 = 0x8;
const COMPAT_RESIZE_INODE: u32 = 0x10;
const COMPAT_DIR_INDEX: u32 = 0x20;
const COMPAT_SPARSE_SUPER2: u32 = 0x200;

const INCOMPAT_COMPRESSION: u32 = 0x1;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_MMP: u32 = 0x100;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_EA_INODE: u32 = 0x400;
const INCOMPAT_DIRDATA: u32 = 0x1000;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const INCOMPAT_ENCRYPT: u32 = 0x10000;
const INCOMPAT_CASEFOLD: u32 = 0x20000;

const RO_SPARSE_SUPER: u32 = 0x1;
const RO_LARGE_FILE: u32 = 0x2;
const RO_BTREE_DIR: u32 = 0x4;
const RO_HUGE_FILE: u32 = 0x8;
const RO_GDT_CSUM: u32 = 0x10;
const RO_DIR_NLINK: u32 = 0x20;
const RO_EXTRA_ISIZE: u32 = 0x40;
const RO_METADATA_CSUM: u32 = 0x400;

/// Incompatible features this module can read.
const READ_INCOMPAT: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_META_BG
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_MMP
    | INCOMPAT_FLEX_BG
    | INCOMPAT_EA_INODE
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR
    | INCOMPAT_INLINE_DATA
    | INCOMPAT_ENCRYPT
    | INCOMPAT_CASEFOLD;
/// Features this module keeps consistent when writing: everything ext2/ext3 use, and the
/// ext4 set mke2fs enables by default (extents, 64-bit, flex_bg, metadata checksums,
/// uninitialized groups).
const WRITE_INCOMPAT: u32 =
    INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_CSUM_SEED;
const WRITE_RO: u32 = RO_SPARSE_SUPER
    | RO_LARGE_FILE
    | RO_BTREE_DIR
    | RO_HUGE_FILE
    | RO_GDT_CSUM
    | RO_DIR_NLINK
    | RO_EXTRA_ISIZE
    | RO_METADATA_CSUM;

const BG_INODE_UNINIT: u16 = 0x1;
const BG_BLOCK_UNINIT: u16 = 0x2;

const EXT_MAGIC: u16 = 0xF30A;
const EXT_MAX_LEN: u64 = 32768;

// --- checksums ----------------------------------------------------------------------------

/// CRC-32C (Castagnoli) without the usual pre/post inversion, as ext4 chains it: start from
/// `crc`, feed `data`, keep the result as the next seed.
fn crc32c(crc: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ 0x82F6_3B78
                } else {
                    c >> 1
                };
            }
            *v = c;
        }
        t
    });
    data.iter().fold(crc, |c, &b| {
        (c >> 8) ^ t[((c ^ u32::from(b)) & 0xFF) as usize]
    })
}

/// CRC-16 (polynomial 0x8005, reflected, no inversion): uninit_bg descriptor checksums.
fn crc16(crc: u16, data: &[u8]) -> u16 {
    data.iter().fold(crc, |mut c, &b| {
        c ^= u16::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xA001
            } else {
                c >> 1
            };
        }
        c
    })
}

const S_IFMT: u16 = 0xF000;
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;
const S_IFLNK: u16 = 0xA000;

const FL_INDEX: u32 = 0x1000;
const FL_EXTENTS: u32 = 0x8_0000;
const FL_ENCRYPT: u32 = 0x800;
const FL_INLINE_DATA: u32 = 0x1000_0000;

const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;

fn feature_names(compat: u32, incompat: u32, ro: u32) -> Vec<&'static str> {
    let table: [(u32, u32, &str); 27] = [
        (0, COMPAT_HAS_JOURNAL, "has_journal"),
        (0, COMPAT_EXT_ATTR, "ext_attr"),
        (0, COMPAT_RESIZE_INODE, "resize_inode"),
        (0, COMPAT_DIR_INDEX, "dir_index"),
        (0, COMPAT_SPARSE_SUPER2, "sparse_super2"),
        (1, INCOMPAT_COMPRESSION, "compression"),
        (1, INCOMPAT_FILETYPE, "filetype"),
        (1, INCOMPAT_RECOVER, "needs_recovery"),
        (1, INCOMPAT_JOURNAL_DEV, "journal_dev"),
        (1, INCOMPAT_META_BG, "meta_bg"),
        (1, INCOMPAT_EXTENTS, "extent"),
        (1, INCOMPAT_64BIT, "64bit"),
        (1, INCOMPAT_MMP, "mmp"),
        (1, INCOMPAT_FLEX_BG, "flex_bg"),
        (1, INCOMPAT_EA_INODE, "ea_inode"),
        (1, INCOMPAT_DIRDATA, "dirdata"),
        (1, INCOMPAT_CSUM_SEED, "metadata_csum_seed"),
        (1, INCOMPAT_LARGEDIR, "large_dir"),
        (1, INCOMPAT_INLINE_DATA, "inline_data"),
        (1, INCOMPAT_ENCRYPT, "encrypt"),
        (1, INCOMPAT_CASEFOLD, "casefold"),
        (2, RO_SPARSE_SUPER, "sparse_super"),
        (2, RO_LARGE_FILE, "large_file"),
        (2, RO_HUGE_FILE, "huge_file"),
        (2, RO_GDT_CSUM, "uninit_bg"),
        (2, RO_DIR_NLINK, "dir_nlink"),
        (2, RO_METADATA_CSUM, "metadata_csum"),
    ];
    let mut out: Vec<&str> = table
        .iter()
        .filter(|(set, bit, _)| [compat, incompat, ro][*set as usize] & bit != 0)
        .map(|t| t.2)
        .collect();
    let known = |set: u32| {
        table
            .iter()
            .filter(|t| t.0 == set)
            .fold(0u32, |a, t| a | t.1)
    };
    if ro & !(known(2) | RO_BTREE_DIR | RO_EXTRA_ISIZE) != 0 {
        out.push("other ro_compat features");
    }
    if incompat & !known(1) != 0 {
        out.push("unknown incompat features");
    }
    out
}

// --- superblock and group descriptors -------------------------------------------------

#[derive(Clone)]
struct Sb {
    raw: Vec<u8>,
    bs: u64,
    blocks: u64,
    first_data: u64,
    bpg: u64,
    ipg: u64,
    inodes: u64,
    isz: u64,
    first_ino: u32,
    compat: u32,
    incompat: u32,
    ro: u32,
    desc: u64,
    groups: u64,
}

impl Sb {
    fn parse(raw: &[u8]) -> Result<Sb> {
        if raw.len() < 1024 || le16(raw, 56) != MAGIC {
            return Err(Error::Invalid("no ext2/3/4 superblock".into()));
        }
        let log = le32(raw, 24);
        if log > 6 {
            return Err(Error::Invalid(
                "ext superblock has an invalid block size".into(),
            ));
        }
        let bs = 1024u64 << log;
        let rev = le32(raw, 76);
        let (incompat, compat, ro) = if rev >= 1 {
            (le32(raw, 96), le32(raw, 92), le32(raw, 100))
        } else {
            (0, 0, 0)
        };
        let mut blocks = u64::from(le32(raw, 4));
        if incompat & INCOMPAT_64BIT != 0 {
            blocks |= u64::from(le32(raw, 336)) << 32;
        }
        let first_data = u64::from(le32(raw, 20));
        let bpg = u64::from(le32(raw, 32));
        let ipg = u64::from(le32(raw, 40));
        let isz = if rev >= 1 {
            u64::from(le16(raw, 88))
        } else {
            128
        };
        let first_ino = if rev >= 1 { le32(raw, 84) } else { 11 };
        let desc = if incompat & INCOMPAT_64BIT != 0 {
            u64::from(le16(raw, 254)).max(32)
        } else {
            32
        };
        if bpg == 0
            || ipg == 0
            || bpg > bs * 8
            || ipg > bs * 8
            || !(128..=bs).contains(&isz)
            || !isz.is_power_of_two()
            || first_data > 1
            || blocks <= first_data
            || !(32..=bs).contains(&desc)
        {
            return Err(Error::Invalid(
                "ext superblock has inconsistent sizes".into(),
            ));
        }
        let groups = (blocks - first_data).div_ceil(bpg);
        Ok(Sb {
            raw: raw[..1024].to_vec(),
            bs,
            blocks,
            first_data,
            bpg,
            ipg,
            inodes: u64::from(le32(raw, 0)),
            isz,
            first_ino,
            compat,
            incompat,
            ro,
            desc,
            groups,
        })
    }

    fn has_super(&self, g: u64) -> bool {
        if g == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return g == u64::from(le32(&self.raw, 0x24C))
                || g == u64::from(le32(&self.raw, 0x250));
        }
        if self.ro & RO_SPARSE_SUPER == 0 {
            return true;
        }
        g == 1 || [3u64, 5, 7].iter().any(|&b| is_power_of(g, b))
    }

    fn group_start(&self, g: u64) -> u64 {
        self.first_data + g * self.bpg
    }

    fn group_blocks(&self, g: u64) -> u64 {
        (self.blocks - self.group_start(g)).min(self.bpg)
    }

    /// Block holding group descriptor block number `db`.
    fn desc_block(&self, db: u64) -> u64 {
        let per = self.bs / self.desc;
        let first_meta = u64::from(le32(&self.raw, 260));
        if self.incompat & INCOMPAT_META_BG != 0 && db >= first_meta {
            let g = db * per;
            self.group_start(g) + u64::from(self.has_super(g))
        } else {
            self.first_data + 1 + db
        }
    }

    fn gdt_blocks(&self) -> u64 {
        (self.groups * self.desc).div_ceil(self.bs)
    }

    fn label(&self) -> String {
        let l = &self.raw[120..136];
        let end = l.iter().position(|&b| b == 0).unwrap_or(16);
        String::from_utf8_lossy(&l[..end]).into_owned()
    }

    fn uuid(&self) -> String {
        guid_string_be(&self.raw[104..120])
    }

    fn kind(&self) -> &'static str {
        kind_of(self.compat, self.incompat, self.ro)
    }

    /// Why this file system cannot be modified here, or `None` when it can.
    fn write_blocker(&self) -> Option<String> {
        if self.incompat & INCOMPAT_RECOVER != 0 {
            return Some(
                "the ext3/4 journal needs recovery (the file system was not unmounted cleanly); \
                 mount it once in Linux or run e2fsck, then try again"
                    .into(),
            );
        }
        let state = le16(&self.raw, 58);
        if state & 1 == 0 || state & 2 != 0 {
            return Some(
                "the file system was not unmounted cleanly or has errors; check it with e2fsck first"
                    .into(),
            );
        }
        let bad_in = self.incompat & !WRITE_INCOMPAT;
        let bad_ro = self.ro & !WRITE_RO;
        if bad_in != 0 || bad_ro != 0 {
            return Some(format!(
                "writing {} file systems with {}",
                self.kind(),
                feature_names(0, bad_in, bad_ro).join(", ")
            ));
        }
        if self.ro & RO_METADATA_CSUM != 0 && self.raw[0x175] != 1 {
            return Some("unknown metadata checksum type".into());
        }
        if self.inodes != self.ipg * self.groups {
            return Some("the inode count does not match the group layout".into());
        }
        None
    }

    fn csum(&self) -> bool {
        self.ro & RO_METADATA_CSUM != 0
    }

    /// Whether group descriptors carry a checksum and the uninit flags mean something.
    fn gd_csum(&self) -> bool {
        self.ro & (RO_METADATA_CSUM | RO_GDT_CSUM) != 0
    }

    fn extents(&self) -> bool {
        self.incompat & INCOMPAT_EXTENTS != 0
    }

    /// Seed of every metadata_csum checksum.
    fn csum_seed(&self) -> u32 {
        if self.incompat & INCOMPAT_CSUM_SEED != 0 {
            le32(&self.raw, 0x270)
        } else {
            crc32c(!0, &self.raw[104..120])
        }
    }

    /// Superblock with its checksum (when the file system has metadata_csum) filled in.
    fn sealed(&self) -> Vec<u8> {
        let mut raw = self.raw.clone();
        if self.csum() {
            let c = crc32c(!0, &raw[..0x3FC]);
            put_le32(&mut raw, 0x3FC, c);
        }
        raw
    }
}

fn is_power_of(mut g: u64, b: u64) -> bool {
    while g > 1 && g.is_multiple_of(b) {
        g /= b;
    }
    g == 1
}

fn kind_of(compat: u32, incompat: u32, ro: u32) -> &'static str {
    let ext4_in = INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_INLINE_DATA;
    let ext4_ro = RO_HUGE_FILE | RO_GDT_CSUM | RO_METADATA_CSUM | RO_DIR_NLINK | RO_EXTRA_ISIZE;
    if incompat & ext4_in != 0 || ro & ext4_ro != 0 {
        "ext4"
    } else if compat & COMPAT_HAS_JOURNAL != 0 {
        "ext3"
    } else {
        "ext2"
    }
}

/// UUIDs in the superblock are stored as plain big-endian bytes.
fn guid_string_be(b: &[u8]) -> String {
    let bytes: [u8; 16] = b[..16].try_into().unwrap();
    uuid::Uuid::from_bytes(bytes).hyphenated().to_string()
}

/// "ext2", "ext3" or "ext4" if `buf` (the first 2 KiB of a volume) holds a plausible
/// ext superblock.
pub(crate) fn fs_name(buf: &[u8]) -> Option<&'static str> {
    if buf.len() < 2048 {
        return None;
    }
    let sb = Sb::parse(&buf[1024..2048]).ok()?;
    let rev = le32(&sb.raw, 76);
    let groups_ok = sb.inodes == sb.ipg * sb.groups || sb.incompat & INCOMPAT_64BIT != 0;
    (rev <= 1 && groups_ok).then(|| sb.kind())
}

#[derive(Clone, Default)]
struct Gd {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
    free_blocks: u64,
    free_inodes: u64,
    used_dirs: u64,
    flags: u16,
    itable_unused: u64,
    /// The descriptor as read (`desc` bytes); fields above are written back into it.
    raw: Vec<u8>,
}

impl Gd {
    fn parse(b: &[u8], wide: bool) -> Gd {
        let mut g = Gd::parse_fields(b, wide);
        g.flags = le16(b, 0x12);
        g.itable_unused = u64::from(le16(b, 0x1C))
            | if wide {
                u64::from(le16(b, 0x32)) << 16
            } else {
                0
            };
        g.raw = b.to_vec();
        g
    }

    /// The descriptor bytes with the current fields and, when the file system has them,
    /// its checksum (`group` is its number).
    fn encode(&self, sb: &Sb, group: u64) -> Vec<u8> {
        let mut b = self.raw.clone();
        b.resize(sb.desc as usize, 0);
        self.store(&mut b);
        put_le16(&mut b, 0x12, self.flags);
        put_le16(&mut b, 0x1C, self.itable_unused as u16);
        if b.len() >= 64 {
            put_le32(&mut b, 0x20, (self.block_bitmap >> 32) as u32);
            put_le32(&mut b, 0x24, (self.inode_bitmap >> 32) as u32);
            put_le32(&mut b, 0x28, (self.inode_table >> 32) as u32);
            put_le16(&mut b, 0x2C, (self.free_blocks >> 16) as u16);
            put_le16(&mut b, 0x2E, (self.free_inodes >> 16) as u16);
            put_le16(&mut b, 0x30, (self.used_dirs >> 16) as u16);
            put_le16(&mut b, 0x32, (self.itable_unused >> 16) as u16);
        }
        let g = (group as u32).to_le_bytes();
        if sb.csum() {
            let mut c = crc32c(sb.csum_seed(), &g);
            c = crc32c(c, &b[..0x1E]);
            c = crc32c(c, &[0, 0]);
            c = crc32c(c, &b[0x20..]);
            put_le16(&mut b, 0x1E, c as u16);
        } else if sb.gd_csum() {
            let mut c = crc16(!0, &sb.raw[104..120]);
            c = crc16(c, &g);
            c = crc16(c, &b[..0x1E]);
            if sb.incompat & INCOMPAT_64BIT != 0 {
                c = crc16(c, &b[0x20..]);
            }
            put_le16(&mut b, 0x1E, c);
        }
        b
    }

    /// Record a bitmap's checksum (`inode`: the inode bitmap, else the block bitmap).
    fn set_bitmap_csum(&mut self, c: u32, inode: bool) {
        let (lo, hi) = if inode { (0x1A, 0x3A) } else { (0x18, 0x38) };
        if self.raw.len() < 32 {
            self.raw.resize(32, 0);
        }
        put_le16(&mut self.raw, lo, c as u16);
        if self.raw.len() >= hi + 2 {
            put_le16(&mut self.raw, hi, (c >> 16) as u16);
        }
    }

    fn parse_fields(b: &[u8], wide: bool) -> Gd {
        let hi32 = |at: usize| {
            if wide {
                u64::from(le32(b, at)) << 32
            } else {
                0
            }
        };
        let hi16 = |at: usize| {
            if wide {
                u64::from(le16(b, at)) << 16
            } else {
                0
            }
        };
        Gd {
            block_bitmap: u64::from(le32(b, 0)) | hi32(32),
            inode_bitmap: u64::from(le32(b, 4)) | hi32(36),
            inode_table: u64::from(le32(b, 8)) | hi32(40),
            free_blocks: u64::from(le16(b, 12)) | hi16(44),
            free_inodes: u64::from(le16(b, 14)) | hi16(46),
            used_dirs: u64::from(le16(b, 16)) | hi16(48),
            ..Default::default()
        }
    }

    /// The low (32-byte descriptor) halves of the counters and locations.
    fn store(&self, b: &mut [u8]) {
        put_le32(b, 0, self.block_bitmap as u32);
        put_le32(b, 4, self.inode_bitmap as u32);
        put_le32(b, 8, self.inode_table as u32);
        put_le16(b, 12, self.free_blocks as u16);
        put_le16(b, 14, self.free_inodes as u16);
        put_le16(b, 16, self.used_dirs as u16);
    }
}

// --- inodes ---------------------------------------------------------------------------

#[derive(Clone)]
struct Inode {
    raw: Vec<u8>,
}

impl Inode {
    fn mode(&self) -> u16 {
        le16(&self.raw, 0)
    }
    fn kind(&self) -> u16 {
        self.mode() & S_IFMT
    }
    fn is_dir(&self) -> bool {
        self.kind() == S_IFDIR
    }
    fn size(&self) -> u64 {
        let lo = u64::from(le32(&self.raw, 4));
        if self.kind() == S_IFREG || self.is_dir() {
            lo | u64::from(le32(&self.raw, 108)) << 32
        } else {
            lo
        }
    }
    fn set_size(&mut self, v: u64) {
        put_le32(&mut self.raw, 4, v as u32);
        put_le32(&mut self.raw, 108, (v >> 32) as u32);
    }
    fn links(&self) -> u16 {
        le16(&self.raw, 26)
    }
    fn set_links(&mut self, v: u16) {
        put_le16(&mut self.raw, 26, v);
    }
    fn blocks512(&self) -> u64 {
        u64::from(le32(&self.raw, 28)) | u64::from(le16(&self.raw, 116)) << 32
    }
    fn add_blocks512(&mut self, d: i64) {
        let v = (self.blocks512() as i64 + d).max(0) as u64;
        put_le32(&mut self.raw, 28, v as u32);
        put_le16(&mut self.raw, 116, (v >> 32) as u16);
    }
    fn flags(&self) -> u32 {
        le32(&self.raw, 32)
    }
    fn set_flags(&mut self, v: u32) {
        put_le32(&mut self.raw, 32, v);
    }
    fn ptr(&self, i: usize) -> u64 {
        u64::from(le32(&self.raw, 40 + i * 4))
    }
    fn set_ptr(&mut self, i: usize, v: u64) {
        put_le32(&mut self.raw, 40 + i * 4, v as u32);
    }
    fn generation(&self) -> u32 {
        le32(&self.raw, 100)
    }
    fn file_acl(&self) -> u64 {
        u64::from(le32(&self.raw, 104)) | u64::from(le16(&self.raw, 118)) << 32
    }
    fn extra(&self) -> usize {
        if self.raw.len() > 128 {
            usize::from(le16(&self.raw, 128))
        } else {
            0
        }
    }
    /// Whether the 4-byte field at `at` exists in this inode (fields past 128 bytes only
    /// when `i_extra_isize` covers them).
    fn has_field(&self, at: usize) -> bool {
        at + 4 <= self.raw.len() && (at < 128 || 128 + self.extra() >= at + 4)
    }
    /// A timestamp field (seconds at `at`, optional `_extra` word at `extra_at`).
    fn time(&self, at: usize, extra_at: Option<usize>) -> i64 {
        let mut secs = i64::from(le32(&self.raw, at) as i32);
        if let Some(x) = extra_at
            && self.has_field(x)
        {
            secs += i64::from(le32(&self.raw, x) & 3) << 32;
        }
        secs
    }
    fn mtime(&self) -> i64 {
        self.time(16, Some(136))
    }
    fn atime(&self) -> i64 {
        self.time(8, Some(140))
    }
    fn crtime(&self) -> Option<i64> {
        self.has_field(144).then(|| self.time(144, Some(148)))
    }
    fn set_time(&mut self, at: usize, extra_at: usize, t: SystemTime) {
        if !self.has_field(at) {
            return;
        }
        let (secs, nanos) = split_time(t);
        put_le32(&mut self.raw, at, secs as u32);
        if self.has_field(extra_at) {
            let epoch = ((secs - i64::from(secs as i32)) >> 32) as u32 & 3;
            put_le32(&mut self.raw, extra_at, (nanos << 2) | epoch);
        }
    }
    /// A fast symlink keeps its target in the block pointers: it owns no blocks other than
    /// an extended-attribute block (`bs` is the block size).
    fn is_fast_symlink(&self, bs: u64) -> bool {
        let ea = if self.file_acl() != 0 { bs / 512 } else { 0 };
        self.kind() == S_IFLNK
            && self.flags() & (FL_EXTENTS | FL_INLINE_DATA) == 0
            && self.size() < 60
            && self.blocks512() <= ea
    }
    /// Device nodes, FIFOs, sockets and fast symlinks have no data blocks.
    fn has_no_blocks(&self, bs: u64) -> bool {
        !matches!(self.kind(), S_IFREG | S_IFDIR | S_IFLNK) || self.is_fast_symlink(bs)
    }
}

fn split_time(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            let n = d.subsec_nanos();
            if n == 0 {
                (-(d.as_secs() as i64), 0)
            } else {
                (-(d.as_secs() as i64) - 1, 1_000_000_000 - n)
            }
        }
    }
}

fn format_secs(secs: i64, with_time: bool) -> String {
    if secs == 0 {
        return String::new();
    }
    match Local.timestamp_opt(secs, 0).single() {
        Some(t) if with_time => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        Some(t) => t.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

/// Mapped part of a file: `len` blocks starting at logical block `lblk`.
#[derive(Debug, Clone, Copy)]
struct Run {
    lblk: u64,
    pblk: u64,
    len: u64,
    /// An ext4 extent allocated but not yet written (reads as zeros).
    uninit: bool,
}

/// Sort runs and merge neighbours, keeping every run within ext4's extent length limit.
fn pack_runs(runs: &[Run]) -> Vec<Run> {
    let mut v: Vec<Run> = runs.to_vec();
    v.sort_by_key(|r| r.lblk);
    let mut out: Vec<Run> = Vec::new();
    for r in v {
        let max = if r.uninit {
            EXT_MAX_LEN - 1
        } else {
            EXT_MAX_LEN
        };
        let mut r = r;
        if let Some(last) = out.last_mut()
            && last.uninit == r.uninit
            && last.lblk + last.len == r.lblk
            && last.pblk + last.len == r.pblk
        {
            let take = (max - last.len.min(max)).min(r.len);
            last.len += take;
            r.lblk += take;
            r.pblk += take;
            r.len -= take;
        }
        while r.len > 0 {
            let take = r.len.min(max);
            out.push(Run { len: take, ..r });
            r.lblk += take;
            r.pblk += take;
            r.len -= take;
        }
    }
    out
}

/// Runs of a list of physical blocks holding logical blocks 0, 1, 2, ...
fn runs_of(data: &[u64]) -> Vec<Run> {
    let runs: Vec<Run> = data
        .iter()
        .enumerate()
        .map(|(i, &p)| Run {
            lblk: i as u64,
            pblk: p,
            len: 1,
            uninit: false,
        })
        .collect();
    pack_runs(&runs)
}

struct DirEnt {
    ino: u32,
    name: Vec<u8>,
}

impl DirEnt {
    fn name(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }
    fn is_dot(&self) -> bool {
        self.name == b"." || self.name == b".."
    }
}

fn rec_len_for(name_len: usize) -> usize {
    (8 + name_len).div_ceil(4) * 4
}

// --- the mounted file system ------------------------------------------------------------

struct Inner {
    w: Window,
    sb: Sb,
    gds: Vec<Gd>,
    /// Cached bitmap blocks (block and inode bitmaps), by block number.
    bitmaps: BTreeMap<u64, Vec<u8>>,
    /// Which group each cached bitmap belongs to, and whether it is an inode bitmap.
    owners: BTreeMap<u64, (u64, bool)>,
    dirty_bitmaps: BTreeSet<u64>,
    dirty: bool,
    blocker: Option<String>,
    /// metadata_csum seed.
    seed: u32,
}

impl Inner {
    fn mount(mut w: Window, writable: bool) -> Result<Inner> {
        let mut raw = vec![0u8; 1024];
        w.read_at(SB_OFFSET, &mut raw)?;
        let sb = Sb::parse(&raw)?;
        let unreadable = sb.incompat & !READ_INCOMPAT;
        if unreadable != 0 {
            return Err(Error::Unsupported(format!(
                "ext file systems with {}",
                feature_names(0, unreadable, 0).join(", ")
            )));
        }
        let wide = sb.incompat & INCOMPAT_64BIT != 0;
        let per = sb.bs / sb.desc;
        let mut gds = Vec::with_capacity(sb.groups as usize);
        let mut block = vec![0u8; sb.bs as usize];
        for db in 0..sb.gdt_blocks() {
            w.read_at(sb.desc_block(db) * sb.bs, &mut block)?;
            for i in 0..per {
                if gds.len() as u64 == sb.groups {
                    break;
                }
                let at = (i * sb.desc) as usize;
                gds.push(Gd::parse(&block[at..at + sb.desc as usize], wide));
            }
        }
        let blocker = if writable { sb.write_blocker() } else { None };
        let seed = sb.csum_seed();
        Ok(Inner {
            w,
            sb,
            gds,
            bitmaps: BTreeMap::new(),
            owners: BTreeMap::new(),
            dirty_bitmaps: BTreeSet::new(),
            dirty: false,
            blocker,
            seed,
        })
    }

    /// Per-inode checksum seed (metadata_csum).
    fn inode_seed(&self, n: u32, generation: u32) -> u32 {
        crc32c(
            crc32c(self.seed, &n.to_le_bytes()),
            &generation.to_le_bytes(),
        )
    }

    /// Bytes of a directory block available for entries (the rest is the checksum tail).
    fn dir_space(&self) -> usize {
        self.bs() as usize - if self.sb.csum() { 12 } else { 0 }
    }

    /// Put the fake entry that holds a directory block's checksum at its end.
    fn put_dir_tail(&self, buf: &mut [u8]) {
        if self.sb.csum() {
            let at = buf.len() - 12;
            buf[at..].fill(0);
            put_le16(buf, at + 4, 12);
            buf[at + 7] = 0xDE;
        }
    }

    /// Write a directory block of directory `dir`, checksumming it when needed.
    fn write_dir_block(&mut self, dir: u32, d: &Inode, pblk: u64, buf: &mut [u8]) -> Result<()> {
        if self.sb.csum() {
            let at = buf.len() - 12;
            if le32(buf, at) != 0
                || le16(buf, at + 4) != 12
                || buf[at + 6] != 0
                || buf[at + 7] != 0xDE
            {
                return Err(Error::Unsupported(
                    "an ext4 folder block without room for its checksum (run e2fsck -fD on it first)"
                        .into(),
                ));
            }
            let c = crc32c(self.inode_seed(dir, d.generation()), &buf[..at]);
            put_le32(buf, buf.len() - 4, c);
        }
        self.write_block(pblk, buf)
    }

    fn bs(&self) -> u64 {
        self.sb.bs
    }

    fn read_block(&mut self, b: u64) -> Result<Vec<u8>> {
        if b >= self.sb.blocks {
            return Err(Error::Invalid(format!(
                "ext: block {b} is outside the file system"
            )));
        }
        let mut buf = vec![0u8; self.bs() as usize];
        self.w.read_at(b * self.bs(), &mut buf)?;
        Ok(buf)
    }

    fn write_block(&mut self, b: u64, data: &[u8]) -> Result<()> {
        if b == 0 || b >= self.sb.blocks {
            return Err(Error::Invalid(format!("ext: refusing to write block {b}")));
        }
        self.w.write_at(b * self.bs(), data)?;
        Ok(())
    }

    fn inode_offset(&self, n: u32) -> Result<u64> {
        let n = u64::from(n);
        if n == 0 || n > self.sb.inodes {
            return Err(Error::Invalid(format!("ext: inode {n} does not exist")));
        }
        let (g, i) = ((n - 1) / self.sb.ipg, (n - 1) % self.sb.ipg);
        Ok(self.gds[g as usize].inode_table * self.bs() + i * self.sb.isz)
    }

    fn read_inode(&mut self, n: u32) -> Result<Inode> {
        let at = self.inode_offset(n)?;
        let mut raw = vec![0u8; self.sb.isz as usize];
        self.w.read_at(at, &mut raw)?;
        Ok(Inode { raw })
    }

    fn write_inode(&mut self, n: u32, ino: &Inode) -> Result<()> {
        let at = self.inode_offset(n)?;
        let mut raw = ino.raw.clone();
        if self.sb.csum() {
            let hi = raw.len() > 128 && ino.extra() >= 4;
            put_le16(&mut raw, 0x7C, 0);
            if hi {
                put_le16(&mut raw, 0x82, 0);
            }
            let c = crc32c(self.inode_seed(n, ino.generation()), &raw);
            put_le16(&mut raw, 0x7C, c as u16);
            if hi {
                put_le16(&mut raw, 0x82, (c >> 16) as u16);
            }
        }
        self.w.write_at(at, &raw)?;
        Ok(())
    }

    // --- reading files and directories ----------------------------------------------

    fn check_readable(ino: &Inode) -> Result<()> {
        if ino.flags() & FL_INLINE_DATA != 0 {
            return Err(Error::Unsupported("ext4 inline data files".into()));
        }
        if ino.flags() & FL_ENCRYPT != 0 {
            return Err(Error::Unsupported("encrypted ext4 files".into()));
        }
        Ok(())
    }

    /// The mapped block runs of a file, in logical order. With `meta`, also collects the
    /// indirect / extent-tree blocks.
    fn runs(&mut self, ino: &Inode, meta: Option<&mut Vec<u64>>) -> Result<Vec<Run>> {
        self.mapped(ino, meta, false)
    }

    /// Like `runs`; with `uninit`, also the allocated-but-unwritten ext4 extents.
    fn mapped(
        &mut self,
        ino: &Inode,
        mut meta: Option<&mut Vec<u64>>,
        uninit: bool,
    ) -> Result<Vec<Run>> {
        Self::check_readable(ino)?;
        let mut out = Vec::new();
        if ino.has_no_blocks(self.bs()) {
            return Ok(out);
        }
        let nblocks = ino.size().div_ceil(self.bs());
        if ino.flags() & FL_EXTENTS != 0 {
            let root = ino.raw[40..100].to_vec();
            self.extent_runs(&root, &mut out, meta, 0)?;
            if !uninit {
                out.retain(|r| !r.uninit);
            }
            out.sort_by_key(|r| r.lblk);
            return Ok(out);
        }
        let mut push = |out: &mut Vec<Run>, lblk: u64, pblk: u64| {
            if let Some(last) = out.last_mut()
                && last.lblk + last.len == lblk
                && last.pblk + last.len == pblk
            {
                last.len += 1;
                return;
            }
            out.push(Run {
                lblk,
                pblk,
                len: 1,
                uninit: false,
            });
        };
        for i in 0..12u64 {
            if i >= nblocks {
                return Ok(out);
            }
            let p = ino.ptr(i as usize);
            if p != 0 {
                push(&mut out, i, p);
            }
        }
        let per = self.bs() / 4;
        let mut base = 12u64;
        for (level, slot) in [(1u32, 12usize), (2, 13), (3, 14)] {
            let span = per.pow(level);
            if base >= nblocks {
                break;
            }
            let p = ino.ptr(slot);
            if p != 0 {
                self.walk_indirect(
                    p,
                    level,
                    base,
                    nblocks,
                    &mut out,
                    &mut push,
                    meta.as_deref_mut(),
                )?;
            }
            base += span;
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_indirect(
        &mut self,
        blk: u64,
        level: u32,
        base: u64,
        limit: u64,
        out: &mut Vec<Run>,
        push: &mut dyn FnMut(&mut Vec<Run>, u64, u64),
        mut meta: Option<&mut Vec<u64>>,
    ) -> Result<()> {
        if let Some(m) = meta.as_deref_mut() {
            m.push(blk);
        }
        let buf = self.read_block(blk)?;
        let per = self.bs() / 4;
        let span = per.pow(level - 1);
        for k in 0..per {
            let lblk = base + k * span;
            if lblk >= limit {
                break;
            }
            let p = u64::from(le32(&buf, (k * 4) as usize));
            if p == 0 {
                continue;
            }
            if level == 1 {
                push(out, lblk, p);
            } else {
                self.walk_indirect(p, level - 1, lblk, limit, out, push, meta.as_deref_mut())?;
            }
        }
        Ok(())
    }

    fn extent_runs(
        &mut self,
        node: &[u8],
        out: &mut Vec<Run>,
        mut meta: Option<&mut Vec<u64>>,
        depth_seen: u32,
    ) -> Result<()> {
        if le16(node, 0) != EXT_MAGIC || depth_seen > 8 {
            return Err(Error::Invalid("ext4 extent tree is damaged".into()));
        }
        let entries = usize::from(le16(node, 2));
        let depth = le16(node, 6);
        for i in 0..entries {
            let e = 12 + i * 12;
            if e + 12 > node.len() {
                break;
            }
            if depth == 0 {
                let lblk = u64::from(le32(node, e));
                let mut len = u64::from(le16(node, e + 4));
                let uninit = len > 32768;
                if uninit {
                    len -= 32768;
                }
                let start = u64::from(le16(node, e + 6)) << 32 | u64::from(le32(node, e + 8));
                if len > 0 {
                    out.push(Run {
                        lblk,
                        pblk: start,
                        len,
                        uninit,
                    });
                }
            } else {
                let child = u64::from(le32(node, e + 4)) | u64::from(le16(node, e + 8)) << 32;
                if let Some(m) = meta.as_deref_mut() {
                    m.push(child);
                }
                let buf = self.read_block(child)?;
                self.extent_runs(&buf, out, meta.as_deref_mut(), depth_seen + 1)?;
            }
        }
        Ok(())
    }

    /// Stream a file's contents (holes read as zeros).
    fn read_data(&mut self, ino: &Inode, out: &mut dyn Write) -> Result<u64> {
        let size = ino.size();
        if ino.is_fast_symlink(self.bs()) {
            let n = size.min(60) as usize;
            out.write_all(&ino.raw[40..40 + n])?;
            return Ok(n as u64);
        }
        let runs = self.runs(ino, None)?;
        let bs = self.bs();
        let mut pos = 0u64;
        let zeros = vec![0u8; 64 * 1024];
        let mut buf = vec![0u8; 1024 * 1024];
        for r in runs {
            let start = r.lblk * bs;
            if start >= size {
                break;
            }
            while pos < start {
                let n = (start - pos).min(zeros.len() as u64) as usize;
                out.write_all(&zeros[..n])?;
                pos += n as u64;
            }
            let end = (start + r.len * bs).min(size);
            while pos < end {
                let n = (end - pos).min(buf.len() as u64) as usize;
                let at = r.pblk * bs + (pos - start);
                self.w.read_at(at, &mut buf[..n])?;
                out.write_all(&buf[..n])?;
                pos += n as u64;
            }
        }
        while pos < size {
            let n = (size - pos).min(zeros.len() as u64) as usize;
            out.write_all(&zeros[..n])?;
            pos += n as u64;
        }
        Ok(size)
    }

    fn read_all(&mut self, ino: &Inode) -> Result<Vec<u8>> {
        let mut v = Vec::with_capacity(ino.size().min(1 << 24) as usize);
        self.read_data(ino, &mut v)?;
        Ok(v)
    }

    fn parse_dir_block(&self, buf: &[u8], out: &mut Vec<DirEnt>) {
        let filetype = self.sb.incompat & INCOMPAT_FILETYPE != 0;
        let mut off = 0usize;
        while off + 8 <= buf.len() {
            let ino = le32(buf, off);
            let mut rec = usize::from(le16(buf, off + 4));
            if rec == 0 || rec == 65535 {
                rec = buf.len() - off;
            }
            let name_len = if filetype {
                usize::from(buf[off + 6])
            } else {
                usize::from(le16(buf, off + 6))
            };
            if rec < 8 || off + rec > buf.len() || 8 + name_len > rec {
                break;
            }
            if ino != 0 && name_len > 0 {
                out.push(DirEnt {
                    ino,
                    name: buf[off + 8..off + 8 + name_len].to_vec(),
                });
            }
            off += rec;
        }
    }

    fn dir_entries(&mut self, dir: &Inode) -> Result<Vec<DirEnt>> {
        if !dir.is_dir() {
            return Err(Error::Invalid("not a folder".into()));
        }
        let data = self.read_all(dir)?;
        let mut out = Vec::new();
        for block in data.chunks(self.bs() as usize) {
            self.parse_dir_block(block, &mut out);
        }
        Ok(out)
    }

    fn find_in(&mut self, dir: u32, name: &str) -> Result<Option<DirEnt>> {
        let d = self.read_inode(dir)?;
        let entries = self.dir_entries(&d)?;
        let (mut exact, mut loose) = (None, None);
        let want = name.to_lowercase();
        for e in entries {
            if e.is_dot() {
                continue;
            }
            if e.name == name.as_bytes() {
                exact = Some(e);
                break;
            }
            if loose.is_none() && e.name().to_lowercase() == want {
                loose = Some(e);
            }
        }
        Ok(exact.or(loose))
    }

    /// Inode number of a path ("" is the root), or None.
    fn lookup(&mut self, path: &str) -> Result<Option<(u32, Option<DirEnt>)>> {
        if path.is_empty() {
            return Ok(Some((ROOT, None)));
        }
        let mut cur = ROOT;
        let mut parts = path.split('/').peekable();
        while let Some(name) = parts.next() {
            let d = self.read_inode(cur)?;
            if !d.is_dir() {
                return Ok(None);
            }
            let Some(e) = self.find_in(cur, name)? else {
                return Ok(None);
            };
            if parts.peek().is_none() {
                return Ok(Some((e.ino, Some(e))));
            }
            cur = e.ino;
        }
        Ok(None)
    }

    fn entry_of(&mut self, e: &DirEnt, parent: &str) -> Result<Entry> {
        let ino = self.read_inode(e.ino)?;
        let name = e.name();
        let is_dir = ino.is_dir();
        Ok(Entry {
            path: join(parent, &name),
            short_name: name.clone(),
            is_dir,
            size: if is_dir { 0 } else { ino.size() },
            modified: format_secs(ino.mtime(), true),
            created: ino
                .crtime()
                .map(|t| format_secs(t, true))
                .unwrap_or_default(),
            accessed: format_secs(ino.atime(), true),
            attributes: Attributes {
                read_only: ino.mode() & 0o222 == 0,
                hidden: name.starts_with('.'),
                system: !matches!(ino.kind(), S_IFREG | S_IFDIR),
                archive: false,
            },
            name,
        })
    }

    // --- allocation -------------------------------------------------------------------

    /// Group `g`'s block bitmap (built from the layout when the group is still
    /// uninitialized).
    fn block_bitmap(&mut self, g: u64) -> Result<&mut Vec<u8>> {
        let blk = self.gds[g as usize].block_bitmap;
        if !self.bitmaps.contains_key(&blk) {
            let b = if self.sb.gd_csum() && self.gds[g as usize].flags & BG_BLOCK_UNINIT != 0 {
                self.uninit_block_bitmap(g)?
            } else {
                self.read_block(blk)?
            };
            self.bitmaps.insert(blk, b);
            self.owners.insert(blk, (g, false));
        }
        Ok(self.bitmaps.get_mut(&blk).unwrap())
    }

    /// Group `g`'s inode bitmap (empty when the group is still uninitialized).
    fn inode_bitmap(&mut self, g: u64) -> Result<&mut Vec<u8>> {
        let blk = self.gds[g as usize].inode_bitmap;
        if !self.bitmaps.contains_key(&blk) {
            let b = if self.sb.gd_csum() && self.gds[g as usize].flags & BG_INODE_UNINIT != 0 {
                let mut b = vec![0u8; self.bs() as usize];
                for i in self.sb.ipg..self.bs() * 8 {
                    b[(i / 8) as usize] |= 1 << (i % 8);
                }
                b
            } else {
                self.read_block(blk)?
            };
            self.bitmaps.insert(blk, b);
            self.owners.insert(blk, (g, true));
        }
        Ok(self.bitmaps.get_mut(&blk).unwrap())
    }

    /// Mark group `g`'s block (or inode) bitmap changed; it is initialized from now on.
    fn touch_bitmap(&mut self, g: u64, inode: bool) {
        let gd = &mut self.gds[g as usize];
        if inode {
            self.dirty_bitmaps.insert(gd.inode_bitmap);
            gd.flags &= !BG_INODE_UNINIT;
        } else {
            self.dirty_bitmaps.insert(gd.block_bitmap);
            gd.flags &= !BG_BLOCK_UNINIT;
        }
        self.dirty = true;
    }

    fn inode_table_blocks(&self) -> u64 {
        (self.sb.ipg * self.sb.isz).div_ceil(self.bs())
    }

    /// The block bitmap of a group flagged BLOCK_UNINIT: only its share of the metadata
    /// (superblock and descriptor copies, and any group's bitmaps and inode table that
    /// flex_bg placed here) is in use.
    fn uninit_block_bitmap(&self, g: u64) -> Result<Vec<u8>> {
        let bs = self.bs();
        let (start, n) = (self.sb.group_start(g), self.sb.group_blocks(g));
        let mut bm = vec![0u8; bs as usize];
        let mut mark = |a: u64, len: u64| {
            for b in a.max(start)..(a + len).min(start + n) {
                let i = b - start;
                bm[(i / 8) as usize] |= 1 << (i % 8);
            }
        };
        if self.sb.has_super(g) {
            if self.sb.incompat & INCOMPAT_META_BG != 0 {
                return Err(Error::Unsupported(
                    "uninitialized block groups on meta_bg file systems".into(),
                ));
            }
            let reserved = u64::from(le16(&self.sb.raw, 0xCE));
            mark(start, 1 + self.sb.gdt_blocks() + reserved);
        }
        let itb = self.inode_table_blocks();
        for gd in &self.gds {
            mark(gd.block_bitmap, 1);
            mark(gd.inode_bitmap, 1);
            mark(gd.inode_table, itb);
        }
        let used = (0..n)
            .filter(|&i| bm[(i / 8) as usize] & (1 << (i % 8)) != 0)
            .count() as u64;
        for i in n..bs * 8 {
            bm[(i / 8) as usize] |= 1 << (i % 8);
        }
        if n - used != self.gds[g as usize].free_blocks {
            return Err(Error::Invalid(format!(
                "ext: block group {g} is uninitialized but its free block count does not \
                 match its layout (run e2fsck)"
            )));
        }
        Ok(bm)
    }

    fn free_blocks_total(&self) -> u64 {
        self.gds.iter().map(|g| g.free_blocks).sum()
    }

    fn free_inodes_total(&self) -> u64 {
        self.gds.iter().map(|g| g.free_inodes).sum()
    }

    fn group_of_block(&self, b: u64) -> u64 {
        (b.saturating_sub(self.sb.first_data) / self.sb.bpg).min(self.sb.groups - 1)
    }

    /// Allocate `count` blocks, first fit starting at `goal` (so mostly contiguous).
    fn alloc_blocks(&mut self, goal: u64, count: u64) -> Result<Vec<u64>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        if self.free_blocks_total() < count {
            return Err(no_space());
        }
        let mut out = Vec::with_capacity(count as usize);
        let groups = self.sb.groups;
        let g0 = self.group_of_block(goal);
        for k in 0..=groups {
            let g = (g0 + k) % groups;
            if self.gds[g as usize].free_blocks == 0 {
                continue;
            }
            let start = self.sb.group_start(g);
            let n = self.sb.group_blocks(g);
            let from = if k == 0 && goal > start {
                (goal - start).min(n)
            } else {
                0
            };
            let bm = self.block_bitmap(g)?;
            let mut taken = 0u64;
            // From the goal to the end of the group, then (first pass only) the start.
            let ranges: [(u64, u64); 2] = if k == 0 {
                [(from, n), (0, from)]
            } else {
                [(0, n), (0, 0)]
            };
            'scan: for (a, b) in ranges {
                for i in a..b {
                    if out.len() as u64 == count {
                        break 'scan;
                    }
                    let (byte, bit) = ((i / 8) as usize, i % 8);
                    if bm[byte] & (1 << bit) == 0 {
                        bm[byte] |= 1 << bit;
                        out.push(start + i);
                        taken += 1;
                    }
                }
            }
            if taken > 0 {
                self.touch_bitmap(g, false);
                self.gds[g as usize].free_blocks -= taken;
            }
            if out.len() as u64 == count {
                return Ok(out);
            }
        }
        // The descriptors claimed more free blocks than the bitmaps have.
        for b in out {
            self.free_block(b)?;
        }
        Err(no_space())
    }

    fn free_block(&mut self, b: u64) -> Result<()> {
        if b < self.sb.first_data || b >= self.sb.blocks {
            return Ok(());
        }
        let g = self.group_of_block(b);
        let i = b - self.sb.group_start(g);
        let bm = self.block_bitmap(g)?;
        let (byte, bit) = ((i / 8) as usize, i % 8);
        if bm[byte] & (1 << bit) != 0 {
            bm[byte] &= !(1 << bit);
            self.touch_bitmap(g, false);
            self.gds[g as usize].free_blocks += 1;
        }
        Ok(())
    }

    fn alloc_inode(&mut self, dir: bool, goal_group: u64) -> Result<u32> {
        let groups = self.sb.groups;
        for k in 0..groups {
            let g = (goal_group + k) % groups;
            if self.gds[g as usize].free_inodes == 0 {
                continue;
            }
            let ipg = self.sb.ipg;
            let first = if g == 0 {
                u64::from(self.sb.first_ino) - 1
            } else {
                0
            };
            let bm = self.inode_bitmap(g)?;
            let Some(i) = (first..ipg).find(|&i| bm[(i / 8) as usize] & (1 << (i % 8)) == 0) else {
                continue;
            };
            bm[(i / 8) as usize] |= 1 << (i % 8);
            self.touch_bitmap(g, true);
            let gd_csum = self.sb.gd_csum();
            let gd = &mut self.gds[g as usize];
            gd.free_inodes -= 1;
            if dir {
                gd.used_dirs += 1;
            }
            // Inodes past the high-water mark are never read by e2fsck or the kernel.
            if gd_csum && i >= ipg - gd.itable_unused.min(ipg) {
                gd.itable_unused = ipg - i - 1;
            }
            return Ok((g * ipg + i + 1) as u32);
        }
        Err(Error::Invalid(
            "no free inodes left in the ext file system".into(),
        ))
    }

    fn free_inode(&mut self, n: u32, dir: bool) -> Result<()> {
        let n = u64::from(n) - 1;
        let (g, i) = (n / self.sb.ipg, n % self.sb.ipg);
        let bm = self.inode_bitmap(g)?;
        let (byte, bit) = ((i / 8) as usize, i % 8);
        if bm[byte] & (1 << bit) != 0 {
            bm[byte] &= !(1 << bit);
            self.touch_bitmap(g, true);
            let gd = &mut self.gds[g as usize];
            gd.free_inodes += 1;
            if dir {
                gd.used_dirs = gd.used_dirs.saturating_sub(1);
            }
        }
        Ok(())
    }

    fn group_of_inode(&self, n: u32) -> u64 {
        (u64::from(n) - 1) / self.sb.ipg
    }

    /// Persist bitmaps, group descriptors and the superblock's counters.
    fn flush(&mut self) -> Result<()> {
        if !self.dirty && self.dirty_bitmaps.is_empty() {
            return Ok(());
        }
        let dirty: Vec<u64> = std::mem::take(&mut self.dirty_bitmaps)
            .into_iter()
            .collect();
        for b in dirty {
            let data = self.bitmaps[&b].clone();
            if self.sb.csum()
                && let Some(&(g, inode)) = self.owners.get(&b)
            {
                let len = if inode { self.sb.ipg } else { self.sb.bpg } / 8;
                let c = crc32c(self.seed, &data[..len as usize]);
                self.gds[g as usize].set_bitmap_csum(c, inode);
            }
            self.write_block(b, &data)?;
        }
        let per = (self.bs() / self.sb.desc) as usize;
        let dsz = self.sb.desc as usize;
        for db in 0..self.sb.gdt_blocks() {
            let at = self.sb.desc_block(db);
            let mut block = self.read_block(at)?;
            for i in 0..per {
                let g = db as usize * per + i;
                if g >= self.gds.len() {
                    break;
                }
                let o = i * dsz;
                let enc = self.gds[g].encode(&self.sb, g as u64);
                block[o..o + dsz].copy_from_slice(&enc);
            }
            self.write_block(at, &block)?;
        }
        let (fb, fi) = (self.free_blocks_total(), self.free_inodes_total());
        let wide = self.sb.incompat & INCOMPAT_64BIT != 0;
        let raw = &mut self.sb.raw;
        put_le32(raw, 12, fb as u32);
        if wide {
            put_le32(raw, 0x158, (fb >> 32) as u32);
        }
        put_le32(raw, 16, fi as u32);
        put_le32(raw, 48, unix_now() as u32);
        let raw = self.sb.sealed();
        self.w.write_at(SB_OFFSET, &raw)?;
        self.dirty = false;
        Ok(())
    }

    // --- writing ----------------------------------------------------------------------

    fn check_writable(&self) -> Result<()> {
        match &self.blocker {
            Some(why) => Err(Error::Unsupported(why.clone())),
            None => Ok(()),
        }
    }

    /// Indirect blocks a block-mapped file of `n` blocks needs.
    fn meta_blocks(&self, n: u64) -> Result<u64> {
        let p = self.bs() / 4;
        let mut rest = n.saturating_sub(12);
        let mut meta = 0;
        if rest > 0 {
            let k = rest.min(p);
            meta += 1;
            rest -= k;
        }
        if rest > 0 {
            let k = rest.min(p * p);
            meta += 1 + k.div_ceil(p);
            rest -= k;
        }
        if rest > 0 {
            let k = rest.min(p * p * p);
            meta += 1 + k.div_ceil(p * p) + k.div_ceil(p);
            rest -= k;
        }
        if rest > 0 {
            return Err(Error::Invalid(
                "file is too large for an ext2/ext3 block map at this block size".into(),
            ));
        }
        Ok(meta)
    }

    /// Fill in the block pointers for `data` (logical blocks 0..n), writing indirect blocks.
    fn build_map(&mut self, ino: &mut Inode, data: &[u64]) -> Result<u64> {
        let mut meta = 0u64;
        let mut rest = data;
        for (i, &b) in rest.iter().take(12).enumerate() {
            ino.set_ptr(i, b);
        }
        rest = &rest[rest.len().min(12)..];
        for (level, slot) in [(1u32, 12usize), (2, 13), (3, 14)] {
            if rest.is_empty() {
                break;
            }
            let goal = data.last().copied().unwrap_or(0);
            let (blk, used) = self.write_indirect(level, rest, goal, &mut meta)?;
            ino.set_ptr(slot, blk);
            rest = &rest[used..];
        }
        if !rest.is_empty() {
            return Err(Error::Invalid(
                "file is too large for an ext2/ext3 block map".into(),
            ));
        }
        Ok(meta)
    }

    fn write_indirect(
        &mut self,
        level: u32,
        data: &[u64],
        goal: u64,
        meta: &mut u64,
    ) -> Result<(u64, usize)> {
        let blk = self.alloc_blocks(goal, 1)?[0];
        *meta += 1;
        let per = (self.bs() / 4) as usize;
        let mut buf = vec![0u8; self.bs() as usize];
        let mut used = 0usize;
        for slot in 0..per {
            if used >= data.len() {
                break;
            }
            if level == 1 {
                put_le32(&mut buf, slot * 4, data[used] as u32);
                used += 1;
            } else {
                let (b, n) = self.write_indirect(level - 1, &data[used..], blk, meta)?;
                put_le32(&mut buf, slot * 4, b as u32);
                used += n;
            }
        }
        self.write_block(blk, &buf)?;
        Ok((blk, used))
    }

    /// Point logical block `lblk` of `ino` at `pblk`, allocating indirect blocks as needed
    /// (their 512-byte counts are added to the inode).
    fn set_block(&mut self, ino: &mut Inode, lblk: u64, pblk: u64) -> Result<()> {
        if lblk < 12 {
            ino.set_ptr(lblk as usize, pblk);
            return Ok(());
        }
        let p = self.bs() / 4;
        let mut idx = lblk - 12;
        let (slot, path): (usize, Vec<u64>) = if idx < p {
            (12, vec![idx])
        } else {
            idx -= p;
            if idx < p * p {
                (13, vec![idx / p, idx % p])
            } else {
                idx -= p * p;
                if idx >= p * p * p {
                    return Err(Error::Invalid("folder is too large".into()));
                }
                (14, vec![idx / (p * p), (idx / p) % p, idx % p])
            }
        };
        let per512 = (self.bs() / 512) as i64;
        let mut cur = ino.ptr(slot);
        if cur == 0 {
            cur = self.alloc_blocks(pblk, 1)?[0];
            self.write_block(cur, &vec![0u8; self.bs() as usize])?;
            ino.set_ptr(slot, cur);
            ino.add_blocks512(per512);
        }
        for (depth, &k) in path.iter().enumerate() {
            let mut buf = self.read_block(cur)?;
            let at = (k * 4) as usize;
            if depth + 1 == path.len() {
                put_le32(&mut buf, at, pblk as u32);
                self.write_block(cur, &buf)?;
                break;
            }
            let mut next = u64::from(le32(&buf, at));
            if next == 0 {
                next = self.alloc_blocks(pblk, 1)?[0];
                self.write_block(next, &vec![0u8; self.bs() as usize])?;
                put_le32(&mut buf, at, next as u32);
                self.write_block(cur, &buf)?;
                ino.add_blocks512(per512);
            }
            cur = next;
        }
        Ok(())
    }

    /// Extent-tree blocks (outside the inode) needed to map `extents` extents.
    fn extent_tree_blocks(&self, extents: u64) -> u64 {
        let per = (self.bs() - 12) / 12;
        let (mut level, mut total) = (extents, 0);
        while level > 4 {
            level = level.div_ceil(per);
            total += level;
        }
        total
    }

    /// Replace inode `num`'s mapping with a fresh, minimal-depth extent tree for `runs`
    /// (packed, in logical order). Returns the number of tree blocks written.
    fn build_extent_tree(
        &mut self,
        num: u32,
        ino: &mut Inode,
        runs: &[Run],
        goal: u64,
    ) -> Result<u64> {
        let bs = self.bs() as usize;
        let per = (bs - 12) / 12;
        let header = |buf: &mut [u8], entries: usize, max: usize, depth: u16| {
            put_le16(buf, 0, EXT_MAGIC);
            put_le16(buf, 2, entries as u16);
            put_le16(buf, 4, max as u16);
            put_le16(buf, 6, depth);
            put_le32(buf, 8, 0);
        };
        let mut level: Vec<(u64, [u8; 12])> = runs
            .iter()
            .map(|r| {
                let mut e = [0u8; 12];
                put_le32(&mut e, 0, r.lblk as u32);
                put_le16(
                    &mut e,
                    4,
                    (r.len + if r.uninit { EXT_MAX_LEN } else { 0 }) as u16,
                );
                put_le16(&mut e, 6, (r.pblk >> 32) as u16);
                put_le32(&mut e, 8, r.pblk as u32);
                (r.lblk, e)
            })
            .collect();
        if self.free_blocks_total() < self.extent_tree_blocks(level.len() as u64) {
            return Err(no_space());
        }
        let seed = self.inode_seed(num, ino.generation());
        let (mut depth, mut written) = (0u16, 0u64);
        while level.len() > 4 {
            let mut up = Vec::with_capacity(level.len().div_ceil(per));
            for chunk in level.chunks(per) {
                let blk = self.alloc_blocks(goal, 1)?[0];
                written += 1;
                let mut buf = vec![0u8; bs];
                header(&mut buf, chunk.len(), per, depth);
                for (i, (_, e)) in chunk.iter().enumerate() {
                    buf[12 + 12 * i..24 + 12 * i].copy_from_slice(e);
                }
                if self.sb.csum() {
                    let tail = 12 + 12 * per;
                    let c = crc32c(seed, &buf[..tail]);
                    put_le32(&mut buf, tail, c);
                }
                self.write_block(blk, &buf)?;
                let mut idx = [0u8; 12];
                put_le32(&mut idx, 0, chunk[0].0 as u32);
                put_le32(&mut idx, 4, blk as u32);
                put_le16(&mut idx, 8, (blk >> 32) as u16);
                up.push((chunk[0].0, idx));
            }
            level = up;
            depth += 1;
        }
        let root = &mut ino.raw[40..100];
        root.fill(0);
        header(root, level.len(), 4, depth);
        for (i, (_, e)) in level.iter().enumerate() {
            root[12 + 12 * i..24 + 12 * i].copy_from_slice(e);
        }
        ino.set_flags(ino.flags() | FL_EXTENTS);
        Ok(written)
    }

    /// Map the blocks of a new inode (logical blocks 0..n): an extent tree on file systems
    /// with extents, else a block map. Returns the number of mapping blocks written.
    fn map_blocks(&mut self, num: u32, ino: &mut Inode, data: &[u64]) -> Result<u64> {
        if self.sb.extents() {
            let goal = data.last().map_or(0, |b| b + 1);
            self.build_extent_tree(num, ino, &runs_of(data), goal)
        } else {
            self.build_map(ino, data)
        }
    }

    /// Point logical block `lblk` of inode `num` at `pblk`, updating its block count.
    fn append_block(&mut self, num: u32, ino: &mut Inode, lblk: u64, pblk: u64) -> Result<()> {
        let per512 = (self.bs() / 512) as i64;
        if ino.flags() & FL_EXTENTS == 0 {
            self.set_block(ino, lblk, pblk)?;
            ino.add_blocks512(per512);
            return Ok(());
        }
        let mut meta = Vec::new();
        let mut runs = self.mapped(ino, Some(&mut meta), true)?;
        runs.push(Run {
            lblk,
            pblk,
            len: 1,
            uninit: false,
        });
        let runs = pack_runs(&runs);
        for &b in &meta {
            self.free_block(b)?;
        }
        let written = self.build_extent_tree(num, ino, &runs, pblk)?;
        ino.add_blocks512((written as i64 - meta.len() as i64 + 1) * per512);
        Ok(())
    }

    /// Whether a directory with `links` links can take one more subfolder.
    fn can_add_subdir(&self, links: u16) -> bool {
        links == 1 || links < 64999 || self.sb.ro & RO_DIR_NLINK != 0
    }

    /// Adjust a directory's link count for a subfolder added (`+1`) or removed (`-1`).
    /// With dir_nlink, a count of 1 means "too many to count" and stays.
    fn link_dir(&mut self, n: u32, delta: i32) -> Result<()> {
        let mut d = self.read_inode(n)?;
        let links = d.links();
        let new = match (links, delta > 0) {
            (1, _) => 1,
            (l, true) if l >= 64999 => {
                if self.sb.ro & RO_DIR_NLINK == 0 {
                    return Err(Error::Invalid("the folder has too many subfolders".into()));
                }
                1
            }
            (l, true) => l + 1,
            (l, false) => l.saturating_sub(1).max(2),
        };
        d.set_links(new);
        d.set_time(12, 132, SystemTime::now());
        d.set_time(16, 136, SystemTime::now());
        self.write_inode(n, &d)
    }

    /// Turn a hashed (htree) directory into a plain linear one. Without metadata_csum
    /// clearing the flag is enough (index blocks read as empty entries); with it, the index
    /// blocks are rewritten as empty, checksummed leaves.
    fn deindex(&mut self, dir: u32) -> Result<()> {
        let mut d = self.read_inode(dir)?;
        if d.flags() & FL_INDEX == 0 {
            return Ok(());
        }
        if self.sb.csum() {
            let bs = self.bs() as usize;
            let runs = self.runs(&d, None)?;
            for r in runs {
                for k in 0..r.len {
                    let (lblk, pblk) = (r.lblk + k, r.pblk + k);
                    let mut buf = self.read_block(pblk)?;
                    let rec0 = usize::from(le16(&buf, 4));
                    if lblk == 0 {
                        // "." (12 bytes) then ".." spanning the rest, with the index inside.
                        if rec0 != 12 || usize::from(le16(&buf, 16)) + 12 != bs {
                            return Err(Error::Invalid(
                                "ext4 hashed folder root is damaged".into(),
                            ));
                        }
                        put_le16(&mut buf, 16, (bs - 24) as u16);
                        buf[24..].fill(0);
                    } else if le32(&buf, 0) == 0 && (rec0 == bs || (rec0 == 0 && bs == 65536)) {
                        buf.fill(0);
                        put_le16(&mut buf, 4, (bs - 12) as u16);
                    } else {
                        continue;
                    }
                    self.put_dir_tail(&mut buf);
                    self.write_dir_block(dir, &d, pblk, &mut buf)?;
                }
            }
        }
        d.set_flags(d.flags() & !FL_INDEX);
        self.write_inode(dir, &d)
    }

    fn new_inode(&self, mode: u16, links: u16, mtime: SystemTime) -> Inode {
        let mut ino = Inode {
            raw: vec![0u8; self.sb.isz as usize],
        };
        put_le16(&mut ino.raw, 0, mode);
        ino.set_links(links);
        if self.sb.isz >= 160 {
            put_le16(&mut ino.raw, 128, 32);
        }
        let now = SystemTime::now();
        ino.set_time(8, 140, now);
        ino.set_time(12, 132, now);
        ino.set_time(16, 136, mtime);
        ino.set_time(144, 148, now);
        put_le32(&mut ino.raw, 100, split_time(now).1 ^ unix_now() as u32);
        ino
    }

    /// Add a name to a directory, using slack space in an existing block or a new block.
    fn dir_add(&mut self, dir: u32, name: &[u8], child: u32, ft: u8) -> Result<()> {
        self.deindex(dir)?;
        let d = self.read_inode(dir)?;
        let filetype = self.sb.incompat & INCOMPAT_FILETYPE != 0;
        let need = rec_len_for(name.len());
        let write_entry = |buf: &mut [u8], at: usize, rec: usize| {
            put_le32(buf, at, child);
            put_le16(buf, at + 4, rec as u16);
            if filetype {
                buf[at + 6] = name.len() as u8;
                buf[at + 7] = ft;
            } else {
                put_le16(buf, at + 6, name.len() as u16);
            }
            buf[at + 8..at + 8 + name.len()].copy_from_slice(name);
        };
        let runs = self.runs(&d, None)?;
        let bs = self.bs() as usize;
        let end = self.dir_space();
        let mut last = 0u64;
        for r in &runs {
            for k in 0..r.len {
                let pblk = r.pblk + k;
                last = pblk;
                let mut buf = self.read_block(pblk)?;
                let mut off = 0usize;
                while off + 8 <= end {
                    let ino = le32(&buf, off);
                    let rec = usize::from(le16(&buf, off + 4));
                    if rec < 8 || off + rec > end {
                        break;
                    }
                    let nl = if filetype {
                        usize::from(buf[off + 6])
                    } else {
                        usize::from(le16(&buf, off + 6))
                    };
                    let used = if ino == 0 { 0 } else { rec_len_for(nl) };
                    if rec >= used + need {
                        if ino == 0 {
                            write_entry(&mut buf, off, rec);
                        } else {
                            put_le16(&mut buf, off + 4, used as u16);
                            write_entry(&mut buf, off + used, rec - used);
                        }
                        self.write_dir_block(dir, &d, pblk, &mut buf)?;
                        return self.touch_dir(dir);
                    }
                    off += rec;
                }
            }
        }
        // No room: append a block.
        let lblk = d.size().div_ceil(self.bs());
        let goal = if last != 0 {
            last + 1
        } else {
            self.sb.group_start(self.group_of_inode(dir))
        };
        let blk = self.alloc_blocks(goal, 1)?[0];
        let mut buf = vec![0u8; bs];
        write_entry(&mut buf, 0, end);
        self.put_dir_tail(&mut buf);
        self.write_dir_block(dir, &d, blk, &mut buf)?;
        let mut d = self.read_inode(dir)?;
        self.append_block(dir, &mut d, lblk, blk)?;
        d.set_size((lblk + 1) * self.bs());
        let now = SystemTime::now();
        d.set_time(12, 132, now);
        d.set_time(16, 136, now);
        self.write_inode(dir, &d)
    }

    /// A directory's contents changed: update its modification and change times.
    fn touch_dir(&mut self, dir: u32) -> Result<()> {
        let mut d = self.read_inode(dir)?;
        let now = SystemTime::now();
        d.set_time(12, 132, now);
        d.set_time(16, 136, now);
        self.write_inode(dir, &d)
    }

    /// Remove the entry with exactly this name (and inode) from a directory.
    fn dir_remove(&mut self, dir: u32, name: &[u8], child: u32) -> Result<()> {
        self.deindex(dir)?;
        let d = self.read_inode(dir)?;
        let filetype = self.sb.incompat & INCOMPAT_FILETYPE != 0;
        let runs = self.runs(&d, None)?;
        let end = self.dir_space();
        for r in &runs {
            for k in 0..r.len {
                let pblk = r.pblk + k;
                let mut buf = self.read_block(pblk)?;
                let (mut off, mut prev) = (0usize, None::<usize>);
                while off + 8 <= end {
                    let ino = le32(&buf, off);
                    let rec = usize::from(le16(&buf, off + 4));
                    if rec < 8 || off + rec > end {
                        break;
                    }
                    let nl = if filetype {
                        usize::from(buf[off + 6])
                    } else {
                        usize::from(le16(&buf, off + 6))
                    };
                    if ino == child && 8 + nl <= rec && &buf[off + 8..off + 8 + nl] == name {
                        match prev {
                            Some(p) => {
                                let prec = usize::from(le16(&buf, p + 4));
                                put_le16(&mut buf, p + 4, (prec + rec) as u16);
                            }
                            None => put_le32(&mut buf, off, 0),
                        }
                        self.write_dir_block(dir, &d, pblk, &mut buf)?;
                        return self.touch_dir(dir);
                    }
                    prev = Some(off);
                    off += rec;
                }
            }
        }
        Err(Error::NotFound(String::from_utf8_lossy(name).into_owned()))
    }

    fn release_blocks(&mut self, ino: &Inode) -> Result<()> {
        let mut meta = Vec::new();
        let runs = self.mapped(ino, Some(&mut meta), true)?;
        for r in runs {
            for k in 0..r.len {
                self.free_block(r.pblk + k)?;
            }
        }
        for b in meta {
            self.free_block(b)?;
        }
        let acl = ino.file_acl();
        if acl != 0 {
            let mut buf = self.read_block(acl)?;
            let refs = le32(&buf, 4);
            if le32(&buf, 0) == 0xEA02_0000 && refs > 1 {
                put_le32(&mut buf, 4, refs - 1);
                if self.sb.csum() {
                    put_le32(&mut buf, 0x10, 0);
                    let c = crc32c(crc32c(self.seed, &acl.to_le_bytes()), &buf);
                    put_le32(&mut buf, 0x10, c);
                }
                self.write_block(acl, &buf)?;
            } else {
                self.free_block(acl)?;
            }
        }
        Ok(())
    }

    /// Drop one link; free the inode and its blocks when none are left.
    fn unlink_inode(&mut self, n: u32) -> Result<()> {
        let mut ino = self.read_inode(n)?;
        let dir = ino.is_dir();
        let links = if dir {
            0
        } else {
            ino.links().saturating_sub(1)
        };
        if links > 0 {
            ino.set_links(links);
            ino.set_time(12, 132, SystemTime::now());
            return self.write_inode(n, &ino);
        }
        self.release_blocks(&ino)?;
        ino.set_links(0);
        put_le32(&mut ino.raw, 20, unix_now() as u32);
        self.write_inode(n, &ino)?;
        self.free_inode(n, dir)
    }

    fn create_file(
        &mut self,
        parent: u32,
        name: &str,
        src: &mut dyn Read,
        size: u64,
        mtime: SystemTime,
        read_only: bool,
    ) -> Result<u32> {
        check_name(name)?;
        let bs = self.bs();
        let n = size.div_ceil(bs);
        // Extent trees are sized once the blocks are known (build_extent_tree checks).
        let meta = if self.sb.extents() {
            0
        } else {
            self.meta_blocks(n)?
        };
        if self.free_blocks_total() < n + meta {
            return Err(no_space());
        }
        if size >= 1 << 31 && self.sb.ro & RO_LARGE_FILE == 0 {
            if le32(&self.sb.raw, 76) == 0 {
                return Err(Error::Unsupported(
                    "files over 2 GiB on revision 0 ext2".into(),
                ));
            }
            self.sb.ro |= RO_LARGE_FILE;
            let ro = self.sb.ro;
            put_le32(&mut self.sb.raw, 100, ro);
            self.dirty = true;
        }
        let num = self.alloc_inode(false, self.group_of_inode(parent))?;
        let goal = self.sb.group_start(self.group_of_inode(num));
        let data = match self.alloc_blocks(goal, n) {
            Ok(d) => d,
            Err(e) => {
                self.free_inode(num, false)?;
                return Err(e);
            }
        };
        let written = (|| -> Result<Inode> {
            let mut i = 0usize;
            let mut buf = Vec::new();
            let mut left = size;
            while i < data.len() {
                let mut j = i + 1;
                while j < data.len() && data[j] == data[j - 1] + 1 && j - i < 256 {
                    j += 1;
                }
                let bytes = ((j - i) as u64 * bs) as usize;
                buf.clear();
                buf.resize(bytes, 0);
                let want = left.min(bytes as u64) as usize;
                src.read_exact(&mut buf[..want])?;
                left -= want as u64;
                self.w.write_at(data[i] * bs, &buf)?;
                i = j;
            }
            let mode = S_IFREG | if read_only { 0o444 } else { 0o644 };
            let mut ino = self.new_inode(mode, 1, mtime);
            let meta = self.map_blocks(num, &mut ino, &data)?;
            ino.set_size(size);
            ino.add_blocks512(((n + meta) * bs / 512) as i64);
            self.write_inode(num, &ino)?;
            Ok(ino)
        })();
        // On failure, give back what was allocated (best effort).
        let ino = match written {
            Ok(ino) => ino,
            Err(e) => {
                for &b in &data {
                    let _ = self.free_block(b);
                }
                let _ = self.free_inode(num, false);
                return Err(e);
            }
        };
        if let Err(e) = self.dir_add(parent, name.as_bytes(), num, FT_REG) {
            let _ = self.release_blocks(&ino);
            let mut dead = ino;
            dead.set_links(0);
            put_le32(&mut dead.raw, 20, unix_now() as u32);
            let _ = self.write_inode(num, &dead);
            let _ = self.free_inode(num, false);
            return Err(e);
        }
        Ok(num)
    }

    fn make_dir(&mut self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        check_name(name)?;
        let p = self.read_inode(parent)?;
        if !self.can_add_subdir(p.links()) {
            return Err(Error::Invalid("the folder has too many subfolders".into()));
        }
        let num = self.alloc_inode(true, self.group_of_inode(parent))?;
        self.init_dir(num, parent, mode)?;
        self.dir_add(parent, name.as_bytes(), num, FT_DIR)?;
        self.link_dir(parent, 1)?;
        Ok(num)
    }

    /// Write a new, empty directory inode `num` (already allocated) whose ".." is `parent`.
    fn init_dir(&mut self, num: u32, parent: u32, mode: u16) -> Result<()> {
        let goal = self.sb.group_start(self.group_of_inode(num));
        let blk = self.alloc_blocks(goal, 1)?[0];
        let mut ino = self.new_inode(S_IFDIR | mode, 2, SystemTime::now());
        let mut dots = self.dot_block(num, parent);
        self.write_dir_block(num, &ino, blk, &mut dots)?;
        let meta = self.map_blocks(num, &mut ino, &[blk])?;
        ino.set_size(self.bs());
        ino.add_blocks512(((1 + meta) * self.bs() / 512) as i64);
        self.write_inode(num, &ino)
    }

    fn dot_block(&self, me: u32, parent: u32) -> Vec<u8> {
        let bs = self.bs() as usize;
        let filetype = self.sb.incompat & INCOMPAT_FILETYPE != 0;
        let mut b = vec![0u8; bs];
        for (at, ino, name, rec) in [
            (0usize, me, &b"."[..], 12usize),
            (12, parent, &b".."[..], self.dir_space() - 12),
        ] {
            put_le32(&mut b, at, ino);
            put_le16(&mut b, at + 4, rec as u16);
            if filetype {
                b[at + 6] = name.len() as u8;
                b[at + 7] = FT_DIR;
            } else {
                put_le16(&mut b, at + 6, name.len() as u16);
            }
            b[at + 8..at + 8 + name.len()].copy_from_slice(name);
        }
        self.put_dir_tail(&mut b);
        b
    }

    /// An empty directory block (one unused entry spanning it).
    fn empty_dir_block(&self) -> Vec<u8> {
        let mut b = vec![0u8; self.bs() as usize];
        put_le16(&mut b, 4, self.dir_space() as u16);
        self.put_dir_tail(&mut b);
        b
    }

    fn ensure_dir(&mut self, path: &str) -> Result<u32> {
        let mut cur = ROOT;
        let mut so_far = String::new();
        for name in path.split('/').filter(|s| !s.is_empty()) {
            so_far = join(&so_far, name);
            cur = match self.find_in(cur, name)? {
                Some(e) => {
                    if !self.read_inode(e.ino)?.is_dir() {
                        return Err(Error::Exists(format!(
                            "{} (a file is in the way)",
                            display(&so_far)
                        )));
                    }
                    e.ino
                }
                None => self.make_dir(cur, name, 0o755)?,
            };
        }
        Ok(cur)
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
            let mut children: Vec<_> = fs::read_dir(host)?.collect::<io::Result<_>>()?;
            children.sort_by_key(|c| c.file_name());
            for c in children {
                self.inject(&c.path(), &img_path, overwrite, stats)?;
            }
            return Ok(());
        }
        let dir = self.ensure_dir(dest_dir)?;
        if let Some(e) = self.find_in(dir, &name)? {
            if self.read_inode(e.ino)?.is_dir() {
                return Err(Error::Exists(format!("{} (a folder)", display(&img_path))));
            }
            if !overwrite {
                return Err(Error::Exists(display(&img_path)));
            }
            self.dir_remove(dir, &e.name, e.ino)?;
            self.unlink_inode(e.ino)?;
        }
        let mut f = File::open(host)?;
        let mtime = meta.modified().unwrap_or_else(|_| SystemTime::now());
        self.create_file(
            dir,
            &name,
            &mut f,
            meta.len(),
            mtime,
            meta.permissions().readonly(),
        )
        .map_err(|e| match e {
            Error::Invalid(m) if m.contains("no free blocks") => Error::Invalid(format!(
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
        let (parent_path, _) = split_parent(path);
        let Some((num, Some(e))) = self.lookup(path)? else {
            return Err(Error::NotFound(display(path)));
        };
        let Some((parent, _)) = self.lookup(parent_path)? else {
            return Err(Error::NotFound(display(parent_path)));
        };
        let ino = self.read_inode(num)?;
        if ino.is_dir() {
            let children: Vec<DirEnt> = self
                .dir_entries(&ino)?
                .into_iter()
                .filter(|c| !c.is_dot())
                .collect();
            if !children.is_empty() && !recursive {
                return Err(Error::Invalid(format!(
                    "folder is not empty: {}",
                    display(path)
                )));
            }
            for c in children {
                let child_path = join(path, &c.name());
                self.remove_entry(num, &c, &child_path, stats)?;
            }
            stats.dirs += 1;
        } else {
            stats.files += 1;
            stats.bytes += ino.size();
        }
        self.dir_remove(parent, &e.name, num)?;
        self.unlink_inode(num)?;
        if ino.is_dir() {
            self.link_dir(parent, -1)?;
        }
        Ok(())
    }

    /// Recursive removal of one directory entry (exact name, already looked up).
    fn remove_entry(
        &mut self,
        parent: u32,
        e: &DirEnt,
        path: &str,
        stats: &mut Stats,
    ) -> Result<()> {
        let ino = self.read_inode(e.ino)?;
        if ino.is_dir() {
            let children: Vec<DirEnt> = self
                .dir_entries(&ino)?
                .into_iter()
                .filter(|c| !c.is_dot())
                .collect();
            for c in children {
                self.remove_entry(e.ino, &c, &join(path, &c.name()), stats)?;
            }
            stats.dirs += 1;
        } else {
            stats.files += 1;
            stats.bytes += ino.size();
        }
        self.dir_remove(parent, &e.name, e.ino)?;
        self.unlink_inode(e.ino)?;
        if ino.is_dir() {
            self.link_dir(parent, -1)?;
        }
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let Some((src, Some(src_e))) = self.lookup(from)? else {
            return Err(Error::NotFound(display(from)));
        };
        let (src_parent_path, _) = split_parent(from);
        let src_parent = self
            .lookup(src_parent_path)?
            .map(|x| x.0)
            .ok_or_else(|| Error::NotFound(display(src_parent_path)))?;
        let src_name = src_e.name();
        let (dst_dir_path, dst_name) = match self.lookup(to)? {
            Some((t, _)) if t == src => {
                let (p, n) = split_parent(to);
                (p.to_string(), n.to_string())
            }
            Some((t, _)) if self.read_inode(t)?.is_dir() => (to.to_string(), src_name.clone()),
            Some(_) => return Err(Error::Exists(display(to))),
            None if to.is_empty() => (String::new(), src_name.clone()),
            None => {
                let (p, n) = split_parent(to);
                (p.to_string(), n.to_string())
            }
        };
        check_name(&dst_name)?;
        let Some((dst_dir, _)) = self.lookup(&dst_dir_path)? else {
            return Err(Error::NotFound(display(&dst_dir_path)));
        };
        if !self.read_inode(dst_dir)?.is_dir() {
            return Err(Error::Invalid(format!(
                "not a folder: {}",
                display(&dst_dir_path)
            )));
        }
        let ino = self.read_inode(src)?;
        let is_dir = ino.is_dir();
        if is_dir && (dst_dir_path == from || dst_dir_path.starts_with(&format!("{from}/"))) {
            return Err(Error::Invalid("cannot move a folder into itself".into()));
        }
        if dst_dir == src_parent && dst_name.as_bytes() == src_e.name.as_slice() {
            return Ok(());
        }
        if let Some(existing) = self.find_in(dst_dir, &dst_name)?
            && existing.ino != src
        {
            return Err(Error::Exists(display(&join(&dst_dir_path, &dst_name))));
        }
        let ft = if is_dir {
            FT_DIR
        } else if ino.kind() == S_IFREG {
            FT_REG
        } else {
            file_type_of(ino.kind())
        };
        let moving_dir = is_dir && dst_dir != src_parent;
        let dst_links = self.read_inode(dst_dir)?.links();
        if moving_dir && !self.can_add_subdir(dst_links) {
            return Err(Error::Invalid("the folder has too many subfolders".into()));
        }
        self.dir_add(dst_dir, dst_name.as_bytes(), src, ft)?;
        self.dir_remove(src_parent, &src_e.name, src)?;
        if moving_dir {
            // Re-point "..", and move the link it represents. (An index root would carry a
            // checksum over "..", so hashed folders become linear first.)
            self.deindex(src)?;
            let ino = self.read_inode(src)?;
            let runs = self.runs(&ino, None)?;
            if let Some(r) = runs.first().filter(|r| r.lblk == 0) {
                let mut buf = self.read_block(r.pblk)?;
                let rec0 = usize::from(le16(&buf, 4));
                if rec0 + 8 <= buf.len() {
                    put_le32(&mut buf, rec0, dst_dir);
                    self.write_dir_block(src, &ino, r.pblk, &mut buf)?;
                }
            }
            self.link_dir(src_parent, -1)?;
            self.link_dir(dst_dir, 1)?;
        }
        let mut ino = self.read_inode(src)?;
        ino.set_time(12, 132, SystemTime::now());
        self.write_inode(src, &ino)?;
        Ok(())
    }

    // --- properties ---------------------------------------------------------------------

    /// Apply `ed` to inode `num` (and, with `recursive`, everything below a folder).
    fn edit_tree(
        &mut self,
        num: u32,
        ed: &ExtEdit,
        recursive: bool,
        count: &mut u64,
    ) -> Result<()> {
        let mut ino = self.read_inode(num)?;
        if let Some(ro) = ed.read_only {
            let mode = ino.mode();
            let mode = if ro {
                mode & !0o222
            } else if mode & 0o222 == 0 {
                mode | 0o200
            } else {
                mode
            };
            put_le16(&mut ino.raw, 0, mode);
        }
        for (t, at, extra_at, what) in [
            (ed.modified, 16usize, 136usize, "modified"),
            (ed.accessed, 8, 140, "accessed"),
            (ed.created, 144, 148, "created"),
        ] {
            let Some(t) = t else { continue };
            if !ino.has_field(at) {
                return Err(Error::Unsupported(
                    "creation times on this ext file system (its inodes have no room for them)"
                        .into(),
                ));
            }
            let secs = split_time(t).0;
            let max = if ino.has_field(extra_at) {
                (3i64 << 32) + i64::from(i32::MAX)
            } else {
                i64::from(i32::MAX)
            };
            if secs < i64::from(i32::MIN) || secs > max {
                return Err(Error::Invalid(format!(
                    "the {what} time is outside what this ext file system can store (1901-2038, or up to 2446 with large inodes)"
                )));
            }
            ino.set_time(at, extra_at, t);
        }
        ino.set_time(12, 132, SystemTime::now());
        self.write_inode(num, &ino)?;
        *count += 1;
        if recursive && ino.is_dir() {
            for c in self.dir_entries(&ino)? {
                if !c.is_dot() {
                    self.edit_tree(c.ino, ed, true, count)?;
                }
            }
        }
        Ok(())
    }

    // --- consistency check (tests) ----------------------------------------------------

    /// Problems a full scan finds: checksums, bitmaps against what inodes use, free
    /// counts, link counts, directory block structure. Empty when the file system is
    /// consistent. Must run on a fresh mount (nothing pending).
    fn check(&mut self) -> Result<Vec<String>> {
        let mut p: Vec<String> = Vec::new();
        let sb = self.sb.clone();
        let bs = sb.bs;
        if sb.csum() && crc32c(!0, &sb.raw[..0x3FC]) != le32(&sb.raw, 0x3FC) {
            p.push("superblock checksum".into());
        }
        // Expected block usage.
        let mut used = vec![0u8; sb.blocks.div_ceil(8) as usize];
        let mut dup = 0u64;
        let mut mark = |b: u64, len: u64, dup_ok: bool, p: &mut Vec<String>| {
            for b in b..b + len {
                if b >= sb.blocks {
                    p.push(format!("block {b} is outside the file system"));
                    return;
                }
                let (i, m) = ((b / 8) as usize, 1u8 << (b % 8));
                if used[i] & m != 0 && !dup_ok {
                    dup += 1;
                    if dup <= 5 {
                        p.push(format!("block {b} is used twice"));
                    }
                }
                used[i] |= m;
            }
        };
        mark(0, sb.first_data, false, &mut p);
        let reserved_gdt = u64::from(le16(&sb.raw, 0xCE));
        let itb = self.inode_table_blocks();
        for g in 0..sb.groups {
            if sb.has_super(g) {
                mark(
                    sb.group_start(g),
                    1 + sb.gdt_blocks() + reserved_gdt,
                    false,
                    &mut p,
                );
            }
            let gd = self.gds[g as usize].clone();
            mark(gd.block_bitmap, 1, false, &mut p);
            mark(gd.inode_bitmap, 1, false, &mut p);
            mark(gd.inode_table, itb, false, &mut p);
            let enc = gd.encode(&sb, g);
            if sb.gd_csum() && enc[0x1E..0x20] != gd.raw[0x1E..0x20] {
                p.push(format!("group {g} descriptor checksum"));
            }
        }
        // Inodes in use, their blocks, and the names that refer to them.
        let mut refs: BTreeMap<u32, u32> = BTreeMap::new();
        let mut subdirs: BTreeMap<u32, u32> = BTreeMap::new();
        let mut inodes: Vec<(u32, Inode)> = Vec::new();
        let (mut free_inodes, mut dirs) = (
            vec![0u64; sb.groups as usize],
            vec![0u64; sb.groups as usize],
        );
        for g in 0..sb.groups {
            let gd = self.gds[g as usize].clone();
            let uninit = sb.gd_csum() && gd.flags & BG_INODE_UNINIT != 0;
            let bm = if uninit {
                vec![0u8; bs as usize]
            } else {
                self.read_block(gd.inode_bitmap)?
            };
            if sb.csum() && !uninit {
                let c = crc32c(self.seed, &bm[..(sb.ipg / 8) as usize]);
                let stored = u32::from(le16(&gd.raw, 0x1A))
                    | if sb.desc >= 64 {
                        u32::from(le16(&gd.raw, 0x3A)) << 16
                    } else {
                        c & 0xFFFF_0000
                    };
                if c != stored {
                    p.push(format!("group {g} inode bitmap checksum"));
                }
            }
            for i in 0..sb.ipg {
                let n = (g * sb.ipg + i + 1) as u32;
                if bm[(i / 8) as usize] & (1 << (i % 8)) == 0 {
                    free_inodes[g as usize] += 1;
                    continue;
                }
                if sb.gd_csum() && i >= sb.ipg - gd.itable_unused.min(sb.ipg) {
                    p.push(format!(
                        "inode {n} is in use past the group's itable_unused mark"
                    ));
                }
                let ino = self.read_inode(n)?;
                let mut raw = ino.raw.clone();
                if raw.iter().all(|&b| b == 0) {
                    continue;
                }
                if sb.csum() {
                    let hi = raw.len() > 128 && ino.extra() >= 4;
                    let stored = u32::from(le16(&raw, 0x7C))
                        | if hi {
                            u32::from(le16(&raw, 0x82)) << 16
                        } else {
                            0
                        };
                    put_le16(&mut raw, 0x7C, 0);
                    if hi {
                        put_le16(&mut raw, 0x82, 0);
                    }
                    let c = crc32c(self.inode_seed(n, ino.generation()), &raw);
                    let c = if hi { c } else { c & 0xFFFF };
                    if c != stored {
                        p.push(format!("inode {n} checksum"));
                    }
                }
                if u64::from(n) >= u64::from(sb.first_ino) && (ino.mode() == 0 || ino.links() == 0)
                {
                    p.push(format!("inode {n} is marked in use but deleted"));
                }
                if ino.is_dir() {
                    dirs[g as usize] += 1;
                }
                inodes.push((n, ino));
            }
        }
        for (n, ino) in &inodes {
            let n = *n;
            if n == 7 {
                // The resize inode maps the reserved descriptor blocks counted above.
                if ino.ptr(13) != 0 {
                    mark(ino.ptr(13), 1, false, &mut p);
                }
                continue;
            }
            let mut meta = Vec::new();
            let runs = match self.mapped(ino, Some(&mut meta), true) {
                Ok(r) => r,
                Err(e) => {
                    p.push(format!("inode {n}: {e}"));
                    continue;
                }
            };
            let mut count = 0u64;
            for r in &runs {
                mark(r.pblk, r.len, false, &mut p);
                count += r.len;
            }
            for &b in &meta {
                mark(b, 1, false, &mut p);
                count += 1;
                if sb.csum() && ino.flags() & FL_EXTENTS != 0 {
                    let buf = self.read_block(b)?;
                    let tail = 12 + 12 * usize::from(le16(&buf, 4));
                    if tail + 4 > buf.len()
                        || crc32c(self.inode_seed(n, ino.generation()), &buf[..tail])
                            != le32(&buf, tail)
                    {
                        p.push(format!("inode {n}: extent block {b} checksum"));
                    }
                }
            }
            if ino.file_acl() != 0 {
                mark(ino.file_acl(), 1, true, &mut p);
                count += 1;
            }
            if ino.flags() & 0x4_0000 == 0 && ino.blocks512() != count * (bs / 512) {
                p.push(format!(
                    "inode {n}: i_blocks {} but {} blocks mapped",
                    ino.blocks512(),
                    count
                ));
            }
            if !ino.is_dir() {
                continue;
            }
            // Directory blocks.
            let indexed = ino.flags() & FL_INDEX != 0;
            let space = self.dir_space();
            for r in &runs {
                for k in 0..r.len {
                    let (lblk, pblk) = (r.lblk + k, r.pblk + k);
                    let buf = self.read_block(pblk)?;
                    let rec0 = usize::from(le16(&buf, 4));
                    if indexed && (lblk == 0 || (le32(&buf, 0) == 0 && rec0 == bs as usize)) {
                        continue;
                    }
                    if sb.csum() {
                        let at = bs as usize - 12;
                        if le16(&buf, at + 4) != 12 || buf[at + 7] != 0xDE {
                            p.push(format!("dir {n} block {lblk}: no checksum tail"));
                        } else if crc32c(self.inode_seed(n, ino.generation()), &buf[..at])
                            != le32(&buf, bs as usize - 4)
                        {
                            p.push(format!("dir {n} block {lblk}: checksum"));
                        }
                    }
                    // The entry chain must end exactly at the end of the entry space.
                    let mut off = 0usize;
                    while off < space {
                        let rec = usize::from(le16(&buf, off + 4));
                        if rec < 12 || off + rec > space {
                            p.push(format!("dir {n} block {lblk}: bad entry at {off}"));
                            break;
                        }
                        off += rec;
                    }
                    let mut es = Vec::new();
                    self.parse_dir_block(&buf[..space], &mut es);
                    for e in es {
                        if e.is_dot() {
                            continue;
                        }
                        *refs.entry(e.ino).or_default() += 1;
                        let child = inodes.iter().find(|(m, _)| *m == e.ino);
                        match child {
                            None => p.push(format!(
                                "dir {n}: {:?} points to free inode {}",
                                e.name(),
                                e.ino
                            )),
                            Some((_, c)) if c.is_dir() => *subdirs.entry(n).or_default() += 1,
                            _ => {}
                        }
                    }
                }
            }
        }
        for (n, ino) in &inodes {
            let n = *n;
            if u64::from(n) < u64::from(sb.first_ino) && n != ROOT {
                continue;
            }
            let want = if ino.is_dir() {
                2 + subdirs.get(&n).copied().unwrap_or(0)
            } else {
                refs.get(&n).copied().unwrap_or(0)
            };
            let links = u32::from(ino.links());
            let ok = links == want || (ino.is_dir() && links == 1 && want >= 65000);
            let orphan_file = sb.compat & 0x1000 != 0 && n == le32(&sb.raw, 0x280);
            if !ok && !orphan_file {
                p.push(format!("inode {n}: {links} links, {want} found"));
            }
        }
        // Bitmaps against usage, and the free counts.
        let mut total_free = 0u64;
        for g in 0..sb.groups {
            let gd = self.gds[g as usize].clone();
            let bm = if sb.gd_csum() && gd.flags & BG_BLOCK_UNINIT != 0 {
                match self.uninit_block_bitmap(g) {
                    Ok(b) => b,
                    Err(e) => {
                        p.push(e.to_string());
                        continue;
                    }
                }
            } else {
                let b = self.read_block(gd.block_bitmap)?;
                if sb.csum() {
                    let c = crc32c(self.seed, &b[..(sb.bpg / 8) as usize]);
                    let stored = u32::from(le16(&gd.raw, 0x18))
                        | if sb.desc >= 64 {
                            u32::from(le16(&gd.raw, 0x38)) << 16
                        } else {
                            c & 0xFFFF_0000
                        };
                    if c != stored {
                        p.push(format!("group {g} block bitmap checksum"));
                    }
                }
                b
            };
            let start = sb.group_start(g);
            let mut free = 0u64;
            let mut wrong = 0u64;
            for i in 0..sb.group_blocks(g) {
                let b = start + i;
                let on = bm[(i / 8) as usize] & (1 << (i % 8)) != 0;
                let want = used[(b / 8) as usize] & (1 << (b % 8)) != 0;
                if !on {
                    free += 1;
                }
                if on != want {
                    wrong += 1;
                    if wrong <= 3 {
                        p.push(format!(
                            "block {b} is {} in the bitmap but {}",
                            if on { "used" } else { "free" },
                            if want { "in use" } else { "unused" }
                        ));
                    }
                }
            }
            if free != gd.free_blocks {
                p.push(format!(
                    "group {g}: {} free blocks recorded, {free} counted",
                    gd.free_blocks
                ));
            }
            if free_inodes[g as usize] != gd.free_inodes {
                p.push(format!(
                    "group {g}: {} free inodes recorded, {} counted",
                    gd.free_inodes, free_inodes[g as usize]
                ));
            }
            if dirs[g as usize] != gd.used_dirs {
                p.push(format!(
                    "group {g}: {} folders recorded, {} counted",
                    gd.used_dirs, dirs[g as usize]
                ));
            }
            total_free += free;
        }
        let mut sb_free = u64::from(le32(&sb.raw, 12));
        if sb.incompat & INCOMPAT_64BIT != 0 {
            sb_free |= u64::from(le32(&sb.raw, 0x158)) << 32;
        }
        if sb_free != total_free {
            p.push(format!(
                "superblock: {sb_free} free blocks recorded, {total_free} counted"
            ));
        }
        Ok(p)
    }
}

fn file_type_of(kind: u16) -> u8 {
    match kind {
        S_IFREG => 1,
        S_IFDIR => 2,
        0x2000 => 3,
        0x6000 => 4,
        0x1000 => 5,
        0xC000 => 6,
        S_IFLNK => 7,
        _ => 0,
    }
}

fn no_space() -> Error {
    Error::Invalid("no free blocks left in the ext file system".into())
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\0'])
        || name.len() > 255
    {
        return Err(Error::InvalidPath(format!(
            "{name:?} is not a valid ext file name (1–255 bytes, no / or NUL)"
        )));
    }
    Ok(())
}

// --- public face ------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ExtInfo {
    pub kind: &'static str,
    pub label: String,
    pub uuid: String,
    pub block_size: u64,
    pub blocks: u64,
    pub free_blocks: u64,
    pub inodes: u64,
    pub free_inodes: u64,
    pub features: Vec<&'static str>,
}

pub(crate) struct ExtFs {
    inner: RefCell<Inner>,
    writable: bool,
}

impl ExtFs {
    pub(crate) fn open(w: Window, writable: bool) -> Result<ExtFs> {
        Ok(ExtFs {
            inner: RefCell::new(Inner::mount(w, writable)?),
            writable,
        })
    }

    pub(crate) fn info(&self) -> ExtInfo {
        let i = self.inner.borrow();
        let sb = &i.sb;
        ExtInfo {
            kind: sb.kind(),
            label: sb.label(),
            uuid: sb.uuid(),
            block_size: sb.bs,
            blocks: sb.blocks,
            free_blocks: i.free_blocks_total(),
            inodes: sb.inodes,
            free_inodes: i.free_inodes_total(),
            features: feature_names(sb.compat, sb.incompat, sb.ro),
        }
    }

    /// Why the file system cannot be modified (`None` when it can, or when it was opened
    /// read-only and could be written).
    pub(crate) fn write_blocker(&self) -> Option<String> {
        self.inner.borrow().sb.write_blocker()
    }

    fn rw(&self) -> Result<std::cell::RefMut<'_, Inner>> {
        if !self.writable {
            return Err(Error::Invalid("the image is open read-only".into()));
        }
        let i = self.inner.borrow_mut();
        i.check_writable()?;
        Ok(i)
    }

    pub(crate) fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let mut i = self.inner.borrow_mut();
        let (num, _) = i
            .lookup(dir)?
            .ok_or_else(|| Error::NotFound(display(dir)))?;
        let d = i.read_inode(num)?;
        if !d.is_dir() {
            return Err(Error::Invalid(format!("not a folder: {}", display(dir))));
        }
        let mut out = Vec::new();
        for e in i.dir_entries(&d)? {
            if !e.is_dot() {
                out.push(i.entry_of(&e, dir)?);
            }
        }
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    pub(crate) fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let mut i = self.inner.borrow_mut();
        let (parent, _) = split_parent(path);
        match i.lookup(path)? {
            Some((_, Some(e))) => Ok(Some(i.entry_of(&e, parent)?)),
            _ => Ok(None),
        }
    }

    pub(crate) fn copy_out(&self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let mut i = self.inner.borrow_mut();
        let (num, _) = i
            .lookup(path)?
            .ok_or_else(|| Error::NotFound(display(path)))?;
        let ino = i.read_inode(num)?;
        if ino.is_dir() {
            return Err(Error::Invalid(format!("{} is a folder", display(path))));
        }
        i.read_data(&ino, out)
    }

    pub(crate) fn extract(&self, src: &str, dest: &Path, overwrite: bool) -> Result<Stats> {
        let mut i = self.inner.borrow_mut();
        fs::create_dir_all(dest)?;
        let mut stats = Stats::default();
        let (num, e) = i
            .lookup(src)?
            .ok_or_else(|| Error::NotFound(display(src)))?;
        match e {
            None => extract_dir(&mut i, num, dest, overwrite, &mut stats)?,
            Some(e) => extract_one(&mut i, &e, dest, overwrite, &mut stats)?,
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

    pub(crate) fn flush(&self) -> Result<()> {
        self.inner.borrow_mut().flush()
    }

    pub(crate) fn check(&self) -> Result<Vec<String>> {
        self.inner.borrow_mut().check()
    }

    /// Change the permissions-as-read-only flag and the times of `path` (the root for "").
    /// ext has no hidden, system or archive attributes. Returns the number of entries
    /// changed.
    pub(crate) fn edit(&self, path: &str, edit: &EntryEdit, recursive: bool) -> Result<u64> {
        let mut i = self.rw()?;
        let foreign: Vec<&str> = [
            ("hidden", edit.hidden),
            ("system", edit.system),
            ("archive", edit.archive),
        ]
        .iter()
        .filter(|(_, v)| v.is_some())
        .map(|(n, _)| *n)
        .collect();
        if !foreign.is_empty() {
            return Err(Error::Unsupported(format!(
                "the {} attribute on ext file systems (they have read-only, kept as the write permission bits, and times)",
                foreign.join(" / ")
            )));
        }
        let parse = |s: &Option<String>| s.as_deref().map(parse_local_time).transpose();
        let ed = ExtEdit {
            read_only: edit.read_only,
            modified: parse(&edit.modified)?,
            created: parse(&edit.created)?,
            accessed: parse(&edit.accessed)?,
        };
        let (num, _) = i
            .lookup(path)?
            .ok_or_else(|| Error::NotFound(display(path)))?;
        let mut n = 0;
        let r = i.edit_tree(num, &ed, recursive, &mut n);
        i.flush()?;
        r.map(|_| n)
    }
}

/// Parsed [`EntryEdit`] for ext inodes.
struct ExtEdit {
    read_only: Option<bool>,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    accessed: Option<SystemTime>,
}

/// Local `YYYY-MM-DD`, `YYYY-MM-DD HH:MM` or `YYYY-MM-DD HH:MM:SS` (a `T` may separate the
/// date and the time).
fn parse_local_time(s: &str) -> Result<SystemTime> {
    let bad = || {
        Error::Invalid(format!(
            "invalid date {s:?} (use YYYY-MM-DD or YYYY-MM-DD HH:MM[:SS])"
        ))
    };
    let t = s.trim().replacen('T', " ", 1);
    let naive = ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"]
        .iter()
        .find_map(|f| chrono::NaiveDateTime::parse_from_str(&t, f).ok())
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(&t, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })
        .ok_or_else(bad)?;
    let secs = Local
        .from_local_datetime(&naive)
        .earliest()
        .ok_or_else(bad)?
        .timestamp();
    Ok(if secs >= 0 {
        UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - std::time::Duration::from_secs(secs.unsigned_abs())
    })
}

fn extract_one(
    i: &mut Inner,
    e: &DirEnt,
    dest: &Path,
    overwrite: bool,
    stats: &mut Stats,
) -> Result<()> {
    let ino = i.read_inode(e.ino)?;
    let target = dest.join(host_safe_name(&e.name()));
    match ino.kind() {
        S_IFDIR => {
            fs::create_dir_all(&target)?;
            stats.dirs += 1;
            extract_dir(i, e.ino, &target, overwrite, stats)
        }
        S_IFREG => {
            if !overwrite && target.exists() {
                return Err(Error::Exists(target.display().to_string()));
            }
            let mut out = File::create(&target)?;
            stats.bytes += i.read_data(&ino, &mut out)?;
            stats.files += 1;
            let secs = ino.mtime();
            let t = if secs >= 0 {
                UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
            } else {
                UNIX_EPOCH - std::time::Duration::from_secs(secs.unsigned_abs())
            };
            let _ = out.set_modified(t);
            Ok(())
        }
        S_IFLNK => {
            #[cfg(unix)]
            {
                let target_text = i.read_all(&ino)?;
                let link = std::ffi::OsStr::from_bytes(&target_text);
                if overwrite {
                    let _ = fs::remove_file(&target);
                }
                std::os::unix::fs::symlink(link, &target)?;
                stats.files += 1;
            }
            Ok(())
        }
        // Device nodes, FIFOs and sockets have no contents to copy.
        _ => Ok(()),
    }
}

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

fn extract_dir(
    i: &mut Inner,
    dir: u32,
    dest: &Path,
    overwrite: bool,
    stats: &mut Stats,
) -> Result<()> {
    let d = i.read_inode(dir)?;
    for e in i.dir_entries(&d)? {
        if !e.is_dot() {
            extract_one(i, &e, dest, overwrite, stats)?;
        }
    }
    Ok(())
}

/// Set (or clear) the volume name of an ext file system in place.
pub(crate) fn set_label(w: &mut Window, label: Option<&str>) -> Result<()> {
    let mut raw = vec![0u8; 1024];
    w.read_at(SB_OFFSET, &mut raw)?;
    let sb = Sb::parse(&raw)?;
    if let Some(why) = sb.write_blocker() {
        return Err(Error::Unsupported(why));
    }
    let l = label.unwrap_or("").trim();
    if l.len() > 16 {
        return Err(Error::Invalid(
            "ext volume names are at most 16 bytes".into(),
        ));
    }
    raw[120..136].fill(0);
    raw[120..120 + l.len()].copy_from_slice(l.as_bytes());
    put_le32(&mut raw, 48, unix_now() as u32);
    w.write_at(SB_OFFSET, &raw)?;
    Ok(())
}

// --- formatting -------------------------------------------------------------------------

/// Options for a new ext2 / ext3 / ext4 file system.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExtFormat {
    /// A journal is required (ext3).
    pub journal: bool,
    /// ext4: extents, metadata checksums, dir_nlink, huge_file, 256-byte inodes, and a
    /// journal when the volume is big enough for one (as mke2fs does).
    pub ext4: bool,
    pub label: Option<String>,
}

fn default_journal_blocks(blocks: u64) -> Option<u64> {
    Some(match blocks {
        0..2048 => return None,
        2048..32768 => 1024,
        32768..262_144 => 4096,
        262_144..524_288 => 8192,
        524_288..4_194_304 => 16384,
        4_194_304..8_388_608 => 32768,
        8_388_608..16_777_216 => 65536,
        16_777_216..33_554_432 => 131_072,
        _ => 262_144,
    })
}

/// Make a new ext2, ext3 or ext4 file system in `w`, which must read as zeros (a freshly
/// created container): only the non-zero structures are written.
pub(crate) fn format(mut w: Window, opts: &ExtFormat) -> Result<()> {
    let size = w.len();
    const MIB: u64 = 1024 * 1024;
    let bs: u64 = if size < 512 * MIB { 1024 } else { 4096 };
    let isz: u64 = if size < 512 * MIB && !opts.ext4 {
        128
    } else {
        256
    };
    let ratio: u64 = if size < 3 * MIB {
        8192
    } else if size < 512 * MIB {
        4096
    } else {
        16384
    };
    let mut blocks = size / bs;
    if blocks > u64::from(u32::MAX) {
        return Err(Error::Invalid(
            "ext2/ext3 volumes are limited to 2^32 blocks (16 TiB)".into(),
        ));
    }
    let label = opts.label.as_deref().unwrap_or("").trim().to_string();
    if label.len() > 16 {
        return Err(Error::Invalid(
            "ext volume names are at most 16 bytes".into(),
        ));
    }
    let fdb = u64::from(bs == 1024);
    let bpg = 8 * bs;
    let has_super = |g: u64| g == 0 || g == 1 || [3u64, 5, 7].iter().any(|&b| is_power_of(g, b));
    let (groups, ipg, itb, gdt) = loop {
        if blocks <= fdb + 64 {
            return Err(Error::Invalid(
                "the volume is too small for ext2 (minimum 64 KiB)".into(),
            ));
        }
        let groups = (blocks - fdb).div_ceil(bpg);
        let gdt = (groups * 32).div_ceil(bs);
        let ipb = bs / isz;
        let wanted = (blocks * bs / ratio).max(16);
        let mut ipg = round_up(wanted.div_ceil(groups), ipb.max(8));
        ipg = ipg.min(bs * 8).max(16);
        let itb = ipg * isz / bs;
        let last = blocks - fdb - (groups - 1) * bpg;
        let overhead = if has_super(groups - 1) { 1 + gdt } else { 0 } + 2 + itb;
        if groups > 1 && last < overhead + 50 {
            blocks = fdb + (groups - 1) * bpg;
            continue;
        }
        if groups == 1 && last < overhead + 32 {
            return Err(Error::Invalid("the volume is too small for ext2".into()));
        }
        break (groups, ipg, itb, gdt);
    };
    let journal = if opts.journal {
        Some(default_journal_blocks(blocks).ok_or_else(|| {
            Error::Invalid("the volume is too small for an ext3 journal (minimum 2048 blocks, 2 MiB); use ext2".into())
        })?)
    } else if opts.ext4 {
        default_journal_blocks(blocks)
    } else {
        None
    };
    let first_ino = 11u64;

    // Superblock.
    let mut sb = vec![0u8; 1024];
    let now = unix_now() as u32;
    put_le32(&mut sb, 0, (ipg * groups) as u32);
    put_le32(&mut sb, 4, blocks as u32);
    put_le32(&mut sb, 8, (blocks * 5 / 100) as u32);
    put_le32(&mut sb, 20, fdb as u32);
    put_le32(&mut sb, 24, bs.trailing_zeros() - 10);
    put_le32(&mut sb, 28, bs.trailing_zeros() - 10);
    put_le32(&mut sb, 32, bpg as u32);
    put_le32(&mut sb, 36, bpg as u32);
    put_le32(&mut sb, 40, ipg as u32);
    put_le32(&mut sb, 48, now);
    put_le16(&mut sb, 54, 0xFFFF);
    put_le16(&mut sb, 56, MAGIC);
    put_le16(&mut sb, 58, 1);
    put_le16(&mut sb, 60, 1);
    put_le32(&mut sb, 64, now);
    put_le32(&mut sb, 76, 1);
    put_le32(&mut sb, 84, first_ino as u32);
    put_le16(&mut sb, 88, isz as u16);
    let mut compat = COMPAT_DIR_INDEX;
    if journal.is_some() {
        compat |= COMPAT_HAS_JOURNAL;
    }
    put_le32(&mut sb, 92, compat);
    let (mut incompat, mut ro) = (INCOMPAT_FILETYPE, RO_SPARSE_SUPER | RO_LARGE_FILE);
    if opts.ext4 {
        incompat |= INCOMPAT_EXTENTS;
        ro |= RO_HUGE_FILE | RO_DIR_NLINK | RO_EXTRA_ISIZE | RO_METADATA_CSUM;
        sb[0x175] = 1; // crc32c
    }
    put_le32(&mut sb, 96, incompat);
    put_le32(&mut sb, 100, ro);
    sb[104..120].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    sb[120..120 + label.len()].copy_from_slice(label.as_bytes());
    sb[236..252].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    sb[252] = 1; // half_md4 directory hash
    put_le32(&mut sb, 264, now);
    if isz > 128 {
        put_le16(&mut sb, 348, 32);
        put_le16(&mut sb, 350, 32);
    }
    put_le32(&mut sb, 352, 1); // signed directory hash, as on x86 Linux

    // Group descriptors and bitmaps.
    let mut gdt_raw = vec![0u8; (gdt * bs) as usize];
    for g in 0..groups {
        let start = fdb + g * bpg;
        let gsize = (blocks - start).min(bpg);
        let o = start + if has_super(g) { 1 + gdt } else { 0 };
        let used = (o - start) + 2 + itb;
        let gd = Gd {
            block_bitmap: o,
            inode_bitmap: o + 1,
            inode_table: o + 2,
            free_blocks: gsize - used,
            free_inodes: if g == 0 { ipg - (first_ino - 1) } else { ipg },
            used_dirs: 0,
            ..Default::default()
        };
        gd.store(&mut gdt_raw[(g * 32) as usize..(g * 32 + 32) as usize]);
        let mut bb = vec![0u8; bs as usize];
        for i in (0..used).chain(gsize..bs * 8) {
            bb[(i / 8) as usize] |= 1 << (i % 8);
        }
        w.write_at(o * bs, &bb)?;
        let mut ib = vec![0u8; bs as usize];
        let reserved = if g == 0 { first_ino - 1 } else { 0 };
        for i in (0..reserved).chain(ipg..bs * 8) {
            ib[(i / 8) as usize] |= 1 << (i % 8);
        }
        w.write_at((o + 1) * bs, &ib)?;
    }
    // The primary superblock and descriptors; the backups are copied once all is done.
    w.write_at(SB_OFFSET, &sb)?;
    w.write_at((fdb + 1) * bs, &gdt_raw)?;

    // Root, lost+found and the journal, through the normal allocation code.
    let mut fs = Inner::mount(w, true)?;
    for g in 0..groups {
        // Every bitmap gets written (and checksummed) by the final flush.
        fs.block_bitmap(g)?;
        fs.inode_bitmap(g)?;
        fs.touch_bitmap(g, false);
        fs.touch_bitmap(g, true);
    }
    fs.init_dir(ROOT, ROOT, 0o755)?;
    fs.gds[0].used_dirs += 1;
    fs.dirty = true;
    let lpf = fs.make_dir(ROOT, "lost+found", 0o700)?;
    // Pre-size lost+found (e2fsck should not need to allocate while repairing).
    let mut lpf_ino = fs.read_inode(lpf)?;
    let mut lpf_blocks = 1u64;
    let mut last = fs.runs(&lpf_ino, None)?[0].pblk;
    while lpf_blocks * bs < 16384 || lpf_blocks < 2 {
        let b = fs.alloc_blocks(last + 1, 1)?[0];
        let mut empty = fs.empty_dir_block();
        fs.write_dir_block(lpf, &lpf_ino, b, &mut empty)?;
        fs.append_block(lpf, &mut lpf_ino, lpf_blocks, b)?;
        lpf_blocks += 1;
        lpf_ino.set_size(lpf_blocks * bs);
        last = b;
    }
    fs.write_inode(lpf, &lpf_ino)?;

    if let Some(jb) = journal {
        let goal = fs.sb.group_start(groups / 2);
        let data = fs.alloc_blocks(goal, jb)?;
        let mut j = fs.new_inode(S_IFREG | 0o600, 1, SystemTime::now());
        let meta = fs.map_blocks(JOURNAL_INO, &mut j, &data)?;
        j.set_size(jb * bs);
        j.add_blocks512(((jb + meta) * bs / 512) as i64);
        fs.write_inode(JOURNAL_INO, &j)?;
        let mut jsb = vec![0u8; bs as usize];
        put_be32(&mut jsb, 0, 0xC03B_3998);
        put_be32(&mut jsb, 4, 4); // superblock v2
        put_be32(&mut jsb, 12, bs as u32);
        put_be32(&mut jsb, 16, jb as u32);
        put_be32(&mut jsb, 20, 1);
        put_be32(&mut jsb, 24, 1);
        let uuid = fs.sb.raw[104..120].to_vec();
        jsb[48..64].copy_from_slice(&uuid);
        put_be32(&mut jsb, 64, 1);
        fs.write_block(data[0], &jsb)?;
        let raw = &mut fs.sb.raw;
        put_le32(raw, 224, JOURNAL_INO);
        raw[253] = 1; // jnl_blocks holds a copy of the journal inode's block map
        for k in 0..15 {
            put_le32(raw, 268 + k * 4, le32(&j.raw, 40 + k * 4));
        }
        put_le32(raw, 268 + 15 * 4, ((jb * bs) >> 32) as u32);
        put_le32(raw, 268 + 16 * 4, (jb * bs) as u32);
    }
    fs.dirty = true;
    fs.flush()?;

    // Backup superblocks and descriptor tables, copied from the finished primaries.
    let mut gdt_now = Vec::with_capacity((gdt * bs) as usize);
    for db in 0..gdt {
        gdt_now.extend(fs.read_block(fdb + 1 + db)?);
    }
    for g in (1..groups).filter(|&g| has_super(g)) {
        let start = fdb + g * bpg;
        let mut copy = fs.sb.clone();
        put_le16(&mut copy.raw, 90, g as u16);
        fs.w.write_at(start * bs, &copy.sealed())?;
        fs.w.write_at((start + 1) * bs, &gdt_now)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_vectors() {
        // CRC-32C (iSCSI) and CRC-16/ARC check values, once the conventions are applied.
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }

    #[test]
    fn packing_runs() {
        let r = |lblk, pblk, len| Run {
            lblk,
            pblk,
            len,
            uninit: false,
        };
        let p = pack_runs(&[r(1, 101, 1), r(0, 100, 1), r(2, 500, 40000)]);
        let got: Vec<(u64, u64, u64)> = p.iter().map(|x| (x.lblk, x.pblk, x.len)).collect();
        assert_eq!(got, [(0, 100, 2), (2, 500, 32768), (32770, 33268, 7232)]);
    }

    #[test]
    fn sparse_super_groups() {
        let backups: Vec<u64> = (0..60)
            .filter(|&g| g == 0 || g == 1 || [3u64, 5, 7].iter().any(|&b| is_power_of(g, b)))
            .collect();
        assert_eq!(backups, vec![0, 1, 3, 5, 7, 9, 25, 27, 49]);
    }

    #[test]
    fn meta_block_counts() {
        let raw = {
            let mut r = vec![0u8; 1024];
            put_le16(&mut r, 56, MAGIC);
            put_le32(&mut r, 4, 8192);
            put_le32(&mut r, 20, 1);
            put_le32(&mut r, 32, 8192);
            put_le32(&mut r, 40, 256);
            put_le32(&mut r, 0, 256);
            r
        };
        let sb = Sb::parse(&raw).unwrap();
        assert_eq!(sb.bs, 1024);
        assert_eq!(sb.groups, 1);
        let p = 256u64;
        let fake = |n: u64| -> u64 {
            let mut rest = n.saturating_sub(12);
            let mut meta = 0;
            if rest > 0 {
                meta += 1;
                rest -= rest.min(p);
            }
            if rest > 0 {
                let k = rest.min(p * p);
                meta += 1 + k.div_ceil(p);
            }
            meta
        };
        assert_eq!(fake(12), 0);
        assert_eq!(fake(13), 1);
        assert_eq!(fake(12 + 256), 1);
        assert_eq!(fake(12 + 257), 3);
    }
}
