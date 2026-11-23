//! Direct FAT12/16/32 structure access for the operations fatfs does not offer:
//! volume label writing, boot sector inspection/editing, and defragmentation.
//! These run on an unmounted volume (no fatfs instance on the same bytes).

use std::collections::VecDeque;

use chrono::{Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::util::*;
use crate::window::Window;

#[derive(Debug, Clone)]
pub(crate) struct Bpb {
    pub bps: u64,
    pub cluster: u64,
    pub reserved: u64,
    pub fats: u64,
    pub total_sectors: u64,
    pub fat_sectors: u64,
    pub fat_bits: u8,
    pub root_cluster: u32,
    pub fsinfo: u64,
    pub backup_boot: u64,
    pub clusters: u32,
    pub fat_offset: u64,
    pub root_offset: u64,
    pub root_bytes: u64,
    pub data_offset: u64,
}

impl Bpb {
    pub(crate) fn parse(s: &[u8]) -> Result<Bpb> {
        let bps = u64::from(le16(s, 11));
        let spc = u64::from(s[13]);
        let reserved = u64::from(le16(s, 14));
        let fats = u64::from(s[16]);
        let root_bytes = u64::from(le16(s, 17)) * 32;
        let total_sectors = if le16(s, 19) != 0 {
            u64::from(le16(s, 19))
        } else {
            u64::from(le32(s, 32))
        };
        let fat_sectors = if le16(s, 22) != 0 {
            u64::from(le16(s, 22))
        } else {
            u64::from(le32(s, 36))
        };
        if !matches!(bps, 512 | 1024 | 2048 | 4096)
            || spc == 0
            || !spc.is_power_of_two()
            || fats == 0
            || fat_sectors == 0
        {
            return Err(Error::NotFat("boot sector BPB is invalid".into()));
        }
        let root_sectors = root_bytes.div_ceil(bps);
        let data_sector = reserved + fats * fat_sectors + root_sectors;
        if data_sector >= total_sectors {
            return Err(Error::NotFat(
                "boot sector BPB is invalid (no data area)".into(),
            ));
        }
        let clusters = ((total_sectors - data_sector) / spc) as u32;
        let fat_bits = if clusters < 4085 {
            12
        } else if clusters < 65525 {
            16
        } else {
            32
        };
        Ok(Bpb {
            bps,
            cluster: bps * spc,
            reserved,
            fats,
            total_sectors,
            fat_sectors,
            fat_bits,
            root_cluster: if fat_bits == 32 { le32(s, 44) } else { 0 },
            fsinfo: if fat_bits == 32 {
                u64::from(le16(s, 48))
            } else {
                0
            },
            backup_boot: if fat_bits == 32 {
                u64::from(le16(s, 50))
            } else {
                0
            },
            clusters,
            fat_offset: reserved * bps,
            root_offset: (reserved + fats * fat_sectors) * bps,
            root_bytes,
            data_offset: data_sector * bps,
        })
    }

    fn cluster_offset(&self, c: u32) -> u64 {
        self.data_offset + u64::from(c - 2) * self.cluster
    }

    fn eoc(&self) -> u32 {
        match self.fat_bits {
            12 => 0xFFF,
            16 => 0xFFFF,
            _ => 0x0FFF_FFFF,
        }
    }

    fn is_eoc(&self, v: u32) -> bool {
        v >= self.eoc() - 7
    }

    fn is_bad(&self, v: u32) -> bool {
        v == self.eoc() - 8
    }

    /// Offset of the "extended boot signature" fields (0x29, serial, label, fs type).
    fn ebpb(&self) -> usize {
        if self.fat_bits == 32 { 64 } else { 36 }
    }
}

pub(crate) fn read_boot(w: &mut Window) -> Result<Vec<u8>> {
    let mut s = vec![0u8; 512];
    w.read_at(0, &mut s)?;
    Ok(s)
}

struct Fat {
    raw: Vec<u8>,
    entries: Vec<u32>,
}

fn load_fat(w: &mut Window, b: &Bpb) -> Result<Fat> {
    let mut raw = vec![0u8; (b.fat_sectors * b.bps) as usize];
    w.read_at(b.fat_offset, &mut raw)?;
    let n = b.clusters as usize + 2;
    let needed = match b.fat_bits {
        12 => (n * 3).div_ceil(2),
        16 => n * 2,
        _ => n * 4,
    };
    if needed > raw.len() {
        return Err(Error::NotFat(
            "FAT is smaller than the cluster count needs".into(),
        ));
    }
    let entries = (0..n)
        .map(|i| match b.fat_bits {
            12 => {
                let v = u32::from(le16(&raw, i * 3 / 2));
                if i % 2 == 1 { v >> 4 } else { v & 0xFFF }
            }
            16 => u32::from(le16(&raw, i * 2)),
            _ => le32(&raw, i * 4) & 0x0FFF_FFFF,
        })
        .collect();
    Ok(Fat { raw, entries })
}

fn store_fat(w: &mut Window, b: &Bpb, fat: &mut Fat) -> Result<()> {
    for (i, &v) in fat.entries.iter().enumerate() {
        match b.fat_bits {
            12 => {
                let at = i * 3 / 2;
                let old = le16(&fat.raw, at);
                let nv = if i % 2 == 1 {
                    (old & 0x000F) | ((v as u16) << 4)
                } else {
                    (old & 0xF000) | (v as u16 & 0xFFF)
                };
                put_le16(&mut fat.raw, at, nv);
            }
            16 => put_le16(&mut fat.raw, i * 2, v as u16),
            _ => {
                let old = le32(&fat.raw, i * 4);
                put_le32(&mut fat.raw, i * 4, (old & 0xF000_0000) | (v & 0x0FFF_FFFF));
            }
        }
    }
    for k in 0..b.fats {
        w.write_at(b.fat_offset + k * b.fat_sectors * b.bps, &fat.raw)?;
    }
    Ok(())
}

fn chain(b: &Bpb, fat: &Fat, first: u32) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut c = first;
    loop {
        if c < 2 || c as usize >= fat.entries.len() {
            return Err(Error::Invalid(format!(
                "cluster chain from {first} leaves the volume (run a disk check)"
            )));
        }
        out.push(c);
        if out.len() > b.clusters as usize {
            return Err(Error::Invalid(format!(
                "cluster chain from {first} loops (run a disk check)"
            )));
        }
        let next = fat.entries[c as usize];
        if b.is_eoc(next) {
            return Ok(out);
        }
        if next == 0 || b.is_bad(next) {
            return Err(Error::Invalid(format!(
                "cluster chain from {first} is broken (run a disk check)"
            )));
        }
        c = next;
    }
}

/// One directory: the fixed FAT12/16 root (empty chain) or a cluster chain.
pub(crate) struct DirNode {
    pub chain: Vec<u32>,
    pub parent: Option<usize>,
    pub path: String,
}

#[derive(Clone)]
pub(crate) struct RawEntry {
    pub dir: usize,
    /// Byte index of the short entry inside its directory.
    pub index: u64,
    pub raw: [u8; 32],
    pub path: String,
    pub chain: Vec<u32>,
    pub child: Option<usize>,
    /// 1 for ".", 2 for "..".
    pub dot: u8,
}

pub(crate) struct Tree {
    pub dirs: Vec<DirNode>,
    pub entries: Vec<RawEntry>,
}

fn dir_bytes(w: &mut Window, b: &Bpb, node: &DirNode) -> Result<Vec<u8>> {
    if node.chain.is_empty() {
        let mut v = vec![0u8; b.root_bytes as usize];
        w.read_at(b.root_offset, &mut v)?;
        return Ok(v);
    }
    let mut v = vec![0u8; node.chain.len() * b.cluster as usize];
    for (i, &c) in node.chain.iter().enumerate() {
        w.read_at(
            b.cluster_offset(c),
            &mut v[i * b.cluster as usize..(i + 1) * b.cluster as usize],
        )?;
    }
    Ok(v)
}

fn entry_offset(b: &Bpb, chain: &[u32], index: u64) -> u64 {
    if chain.is_empty() {
        b.root_offset + index
    } else {
        b.cluster_offset(chain[(index / b.cluster) as usize]) + index % b.cluster
    }
}

fn first_cluster(b: &Bpb, e: &[u8]) -> u32 {
    let hi = if b.fat_bits == 32 {
        u32::from(le16(e, 20)) << 16
    } else {
        0
    };
    hi | u32::from(le16(e, 26))
}

fn short_name(e: &[u8]) -> String {
    let mut name: Vec<u8> = e[0..8].to_vec();
    if name[0] == 0x05 {
        name[0] = 0xE5;
    }
    let base = String::from_utf8_lossy(&name).trim_end().to_string();
    let ext = String::from_utf8_lossy(&e[8..11]).trim_end().to_string();
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

fn lfn_checksum(e: &[u8]) -> u8 {
    e[0..11]
        .iter()
        .fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c))
}

pub(crate) fn walk(w: &mut Window, b: &Bpb) -> Result<Tree> {
    let fat = load_fat(w, b)?;
    walk_with(w, b, &fat)
}

fn walk_with(w: &mut Window, b: &Bpb, fat: &Fat) -> Result<Tree> {
    let root_chain = if b.fat_bits == 32 {
        chain(b, fat, b.root_cluster)?
    } else {
        vec![]
    };
    let mut tree = Tree {
        dirs: vec![DirNode {
            chain: root_chain,
            parent: None,
            path: String::new(),
        }],
        entries: vec![],
    };
    let mut queue = VecDeque::from([0usize]);
    while let Some(d) = queue.pop_front() {
        let bytes = dir_bytes(w, b, &tree.dirs[d])?;
        let mut lfn: Vec<(u8, [u16; 13])> = Vec::new();
        let mut lfn_sum = 0u8;
        for (i, e) in bytes.chunks(32).enumerate() {
            if e[0] == 0 {
                break;
            }
            if e[0] == 0xE5 {
                lfn.clear();
                continue;
            }
            let attr = e[11];
            if attr & 0x3F == 0x0F {
                if e[0] & 0x40 != 0 {
                    lfn.clear();
                    lfn_sum = e[13];
                }
                let mut part = [0u16; 13];
                for (k, off) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
                    .iter()
                    .enumerate()
                {
                    part[k] = le16(e, *off);
                }
                lfn.push((e[0] & 0x1F, part));
                continue;
            }
            if attr & 0x08 != 0 {
                lfn.clear();
                continue; // volume label
            }
            let sname = short_name(e);
            let mut name = sname.clone();
            if !lfn.is_empty() && lfn_sum == lfn_checksum(e) {
                lfn.sort_by_key(|(n, _)| *n);
                let units: Vec<u16> = lfn
                    .iter()
                    .flat_map(|(_, p)| p.iter().copied())
                    .take_while(|&u| u != 0)
                    .collect();
                name = String::from_utf16_lossy(&units);
            }
            lfn.clear();
            let dot = match sname.as_str() {
                "." => 1,
                ".." => 2,
                _ => 0,
            };
            let first = first_cluster(b, e);
            let chain_v = if dot == 0 && first != 0 {
                chain(b, fat, first)?
            } else {
                vec![]
            };
            let parent_path = &tree.dirs[d].path;
            let path = if parent_path.is_empty() {
                name.clone()
            } else {
                format!("{parent_path}/{name}")
            };
            let mut child = None;
            if attr & 0x10 != 0 && dot == 0 && !chain_v.is_empty() {
                tree.dirs.push(DirNode {
                    chain: chain_v.clone(),
                    parent: Some(d),
                    path: path.clone(),
                });
                child = Some(tree.dirs.len() - 1);
                queue.push_back(tree.dirs.len() - 1);
            }
            tree.entries.push(RawEntry {
                dir: d,
                index: (i * 32) as u64,
                raw: e.try_into().unwrap(),
                path,
                chain: chain_v,
                child,
                dot,
            });
        }
    }
    Ok(tree)
}

// --- volume label ---------------------------------------------------------------------

fn fat_now() -> (u16, u16) {
    let n = Local::now();
    let time = ((n.hour() as u16) << 11) | ((n.minute() as u16) << 5) | (n.second() as u16 / 2);
    let date = (((n.year().clamp(1980, 2107) - 1980) as u16) << 9)
        | ((n.month() as u16) << 5)
        | n.day() as u16;
    (time, date)
}

/// Set (or with `None`, remove) the volume label in both the boot sector and the root folder.
pub(crate) fn set_label(w: &mut Window, label: Option<[u8; 11]>) -> Result<()> {
    let mut boot = read_boot(w)?;
    let b = Bpb::parse(&boot)?;
    let e = b.ebpb();
    if boot[e + 2] == 0x29 {
        boot[e + 7..e + 18].copy_from_slice(&label.unwrap_or(*b"NO NAME    "));
        write_boot(w, &b, &boot)?;
    }

    let fat = load_fat(w, &b)?;
    let root = DirNode {
        chain: if b.fat_bits == 32 {
            chain(&b, &fat, b.root_cluster)?
        } else {
            vec![]
        },
        parent: None,
        path: String::new(),
    };
    let bytes = dir_bytes(w, &b, &root)?;
    let mut existing = None;
    let mut free = None;
    for (i, ent) in bytes.chunks(32).enumerate() {
        if ent[0] == 0 {
            free = free.or(Some(i));
            break;
        }
        if ent[0] == 0xE5 {
            free = free.or(Some(i));
            continue;
        }
        if ent[11] & 0x3F != 0x0F && ent[11] & 0x18 == 0x08 {
            existing = Some(i);
            break;
        }
    }
    let (time, date) = fat_now();
    match (label, existing) {
        (Some(l), Some(i)) => {
            let at = entry_offset(&b, &root.chain, i as u64 * 32);
            let mut ent = [0u8; 32];
            ent.copy_from_slice(&bytes[i * 32..i * 32 + 32]);
            ent[0..11].copy_from_slice(&l);
            put_le16(&mut ent, 22, time);
            put_le16(&mut ent, 24, date);
            w.write_at(at, &ent)?;
        }
        (Some(l), None) => {
            let i = free.ok_or_else(|| {
                Error::Invalid("the root folder is full, so no label entry can be added".into())
            })?;
            let mut ent = [0u8; 32];
            ent[0..11].copy_from_slice(&l);
            ent[11] = 0x08;
            put_le16(&mut ent, 22, time);
            put_le16(&mut ent, 24, date);
            w.write_at(entry_offset(&b, &root.chain, i as u64 * 32), &ent)?;
        }
        (None, Some(i)) => {
            w.write_at(entry_offset(&b, &root.chain, i as u64 * 32), &[0xE5])?;
        }
        (None, None) => {}
    }
    Ok(())
}

fn write_boot(w: &mut Window, b: &Bpb, boot: &[u8]) -> Result<()> {
    w.write_at(0, boot)?;
    if b.fat_bits == 32 && b.backup_boot != 0 && b.backup_boot < b.reserved {
        w.write_at(b.backup_boot * b.bps, boot)?;
    }
    Ok(())
}

// --- boot sector ----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct BootSectorInfo {
    pub jump: String,
    pub oem_name: String,
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub fats: u8,
    pub root_entries: u16,
    pub total_sectors: u64,
    pub media: String,
    pub sectors_per_fat: u64,
    pub sectors_per_track: u16,
    pub heads: u16,
    pub hidden_sectors: u32,
    pub drive_number: String,
    pub volume_id: Option<String>,
    pub label: Option<String>,
    pub fs_type: Option<String>,
    pub fat_type: String,
    pub signature_55aa: bool,
    /// Whether the boot code area holds anything (a bootable floppy, not a blank one).
    pub has_boot_code: bool,
}

/// Boot sector fields that can be changed without reformatting.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BootSectorEdit {
    pub oem_name: Option<String>,
    /// `XXXX-XXXX` or plain hex.
    pub volume_id: Option<String>,
    pub sectors_per_track: Option<u16>,
    pub heads: Option<u16>,
    pub hidden_sectors: Option<u32>,
    pub drive_number: Option<u8>,
}

pub(crate) fn boot_info(s: &[u8]) -> Result<BootSectorInfo> {
    let b = Bpb::parse(s)?;
    let e = b.ebpb();
    let ext = s[e + 2] == 0x29;
    let code_start = if b.fat_bits == 32 { 90 } else { 62 };
    Ok(BootSectorInfo {
        jump: format!("{:02X} {:02X} {:02X}", s[0], s[1], s[2]),
        oem_name: String::from_utf8_lossy(&s[3..11]).into_owned(),
        bytes_per_sector: le16(s, 11),
        sectors_per_cluster: s[13],
        reserved_sectors: le16(s, 14),
        fats: s[16],
        root_entries: le16(s, 17),
        total_sectors: b.total_sectors,
        media: format!("0x{:02X}", s[21]),
        sectors_per_fat: b.fat_sectors,
        sectors_per_track: le16(s, 24),
        heads: le16(s, 26),
        hidden_sectors: le32(s, 28),
        drive_number: format!("0x{:02X}", s[e]),
        volume_id: ext.then(|| {
            let id = le32(s, e + 3);
            format!("{:04X}-{:04X}", id >> 16, id & 0xFFFF)
        }),
        label: ext.then(|| {
            String::from_utf8_lossy(&s[e + 7..e + 18])
                .trim_end()
                .to_string()
        }),
        fs_type: ext.then(|| {
            String::from_utf8_lossy(&s[e + 18..e + 26])
                .trim_end()
                .to_string()
        }),
        fat_type: format!("FAT{}", b.fat_bits),
        signature_55aa: s[510] == 0x55 && s[511] == 0xAA,
        has_boot_code: !is_zero(&s[code_start..510])
            && s[code_start..510].iter().any(|&x| x != 0xF6),
    })
}

pub(crate) fn parse_volume_id(s: &str) -> Result<u32> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    u32::from_str_radix(&hex, 16)
        .map_err(|_| Error::Invalid(format!("volume serial {s:?} is not hex like 1A2B-3C4D")))
}

pub(crate) fn edit_boot(w: &mut Window, edit: &BootSectorEdit) -> Result<()> {
    let mut s = read_boot(w)?;
    let b = Bpb::parse(&s)?;
    let e = b.ebpb();
    if let Some(oem) = &edit.oem_name {
        if oem.len() > 8 || !oem.is_ascii() {
            return Err(Error::Invalid(
                "OEM name must be up to 8 ASCII characters".into(),
            ));
        }
        let mut f = [b' '; 8];
        f[..oem.len()].copy_from_slice(oem.as_bytes());
        s[3..11].copy_from_slice(&f);
    }
    if let Some(v) = &edit.volume_id {
        if s[e + 2] != 0x29 {
            return Err(Error::Invalid(
                "this boot sector has no volume serial field".into(),
            ));
        }
        put_le32(&mut s, e + 3, parse_volume_id(v)?);
    }
    if let Some(v) = edit.sectors_per_track {
        put_le16(&mut s, 24, v);
    }
    if let Some(v) = edit.heads {
        put_le16(&mut s, 26, v);
    }
    if let Some(v) = edit.hidden_sectors {
        put_le32(&mut s, 28, v);
    }
    if let Some(v) = edit.drive_number {
        s[e] = v;
    }
    write_boot(w, &b, &s)
}

/// Replace the boot code with the one in `new` (a 512-byte boot sector from a file or
/// another image) while keeping this volume's BPB, so the file system stays valid.
pub(crate) fn import_boot(w: &mut Window, new: &[u8]) -> Result<()> {
    if new.len() < 512 {
        return Err(Error::Invalid(
            "a boot sector file must be at least 512 bytes".into(),
        ));
    }
    let old = read_boot(w)?;
    let b = Bpb::parse(&old)?;
    let keep_end = if b.fat_bits == 32 { 90 } else { 62 };
    let mut s = new[..512].to_vec();
    s[11..keep_end].copy_from_slice(&old[11..keep_end]);
    if s[0] != 0xEB && s[0] != 0xE9 {
        return Err(Error::Invalid(
            "the new boot sector does not start with a jump instruction".into(),
        ));
    }
    s[510] = 0x55;
    s[511] = 0xAA;
    write_boot(w, &b, &s)
}

// --- defragmentation ------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct DefragReport {
    pub files: u64,
    pub dirs: u64,
    /// Files and folders whose clusters are not contiguous.
    pub fragmented: u64,
    /// Total extents over all files and folders (1 per contiguous run).
    pub fragments: u64,
    pub used_clusters: u64,
    pub free_clusters: u64,
    /// Allocated clusters no file owns (freed by defragmentation).
    pub lost_clusters: u64,
    /// Clusters whose data was moved (0 for an analysis).
    pub moved_clusters: u64,
    pub cluster_size: u64,
}

fn runs(chain: &[u32]) -> u64 {
    if chain.is_empty() {
        return 0;
    }
    1 + chain.windows(2).filter(|p| p[1] != p[0] + 1).count() as u64
}

struct Plan {
    b: Bpb,
    fat: Fat,
    tree: Tree,
    report: DefragReport,
}

fn plan(w: &mut Window) -> Result<Plan> {
    let boot = read_boot(w)?;
    let b = Bpb::parse(&boot)?;
    let fat = load_fat(w, &b)?;
    let tree = walk_with(w, &b, &fat)?;
    let mut owner = vec![false; fat.entries.len()];
    let mut report = DefragReport {
        cluster_size: b.cluster,
        ..Default::default()
    };
    let all_chains = std::iter::once(&tree.dirs[0].chain)
        .chain(tree.entries.iter().filter(|e| e.dot == 0).map(|e| &e.chain));
    for c in all_chains {
        for &cl in c {
            if owner[cl as usize] {
                return Err(Error::Invalid(format!(
                    "cluster {cl} belongs to two files (cross-linked; run a disk check)"
                )));
            }
            owner[cl as usize] = true;
        }
        if runs(c) > 1 {
            report.fragmented += 1;
        }
        report.fragments += runs(c);
    }
    for e in tree.entries.iter().filter(|e| e.dot == 0) {
        if e.raw[11] & 0x10 != 0 {
            report.dirs += 1;
        } else {
            report.files += 1;
        }
    }
    for (i, &v) in fat.entries.iter().enumerate().skip(2) {
        if owner[i] {
            report.used_clusters += 1;
        } else if v == 0 {
            report.free_clusters += 1;
        } else if !b.is_bad(v) {
            report.lost_clusters += 1;
        }
    }
    Ok(Plan {
        b,
        fat,
        tree,
        report,
    })
}

pub(crate) fn analyze(w: &mut Window) -> Result<DefragReport> {
    Ok(plan(w)?.report)
}

/// Rewrite the volume so every file and folder is contiguous, in folder order, starting at
/// the beginning of the data area. Lost clusters are freed; `zero_free` also clears the
/// free space (which makes IMZ images compress better).
pub(crate) fn defrag(w: &mut Window, zero_free: bool) -> Result<DefragReport> {
    let Plan {
        b,
        mut fat,
        tree,
        mut report,
    } = plan(w)?;
    if fat.entries.iter().skip(2).any(|&v| b.is_bad(v)) {
        return Err(Error::Unsupported(
            "defragmenting volumes with bad clusters".into(),
        ));
    }
    let n = fat.entries.len();

    // Placement order: the root chain, then for each folder (breadth first) its entries.
    let mut order: Vec<&Vec<u32>> = vec![&tree.dirs[0].chain];
    let mut by_dir: Vec<Vec<usize>> = vec![Vec::new(); tree.dirs.len()];
    for (i, e) in tree.entries.iter().enumerate() {
        by_dir[e.dir].push(i);
    }
    let mut queue = VecDeque::from([0usize]);
    while let Some(d) = queue.pop_front() {
        for &i in &by_dir[d] {
            let e = &tree.entries[i];
            if e.dot == 0 && !e.chain.is_empty() {
                order.push(&e.chain);
                if let Some(c) = e.child {
                    queue.push_back(c);
                }
            }
        }
    }

    // old -> new (0 = not moved/unused), and its inverse.
    let mut map = vec![0u32; n];
    let mut inv = vec![0u32; n];
    let mut next = 2u32;
    for c in &order {
        for &old in c.iter() {
            map[old as usize] = next;
            inv[next as usize] = old;
            next += 1;
        }
    }
    let used_end = next;

    // Move data with the permutation: destination d receives the content of inv[d].
    let cs = b.cluster as usize;
    let mut buf = vec![0u8; cs];
    let mut done = vec![false; n];
    let mut moved = 0u64;
    let copy = |w: &mut Window, from: u32, to: u32, buf: &mut [u8]| -> Result<()> {
        w.read_at(b.cluster_offset(from), buf)?;
        w.write_at(b.cluster_offset(to), buf)?;
        Ok(())
    };
    // Paths first: they start at a destination whose current content nobody needs.
    for d in 2..used_end {
        if done[d as usize] || inv[d as usize] == d || map[d as usize] != 0 {
            continue;
        }
        let mut cur = d;
        loop {
            let src = inv[cur as usize];
            copy(w, src, cur, &mut buf)?;
            done[cur as usize] = true;
            moved += 1;
            if src >= used_end || inv[src as usize] == 0 || done[src as usize] {
                break;
            }
            cur = src;
        }
    }
    // What is left are cycles (or clusters already in place).
    let mut saved = vec![0u8; cs];
    for d in 2..used_end {
        if done[d as usize] {
            continue;
        }
        if inv[d as usize] == d {
            done[d as usize] = true;
            continue;
        }
        w.read_at(b.cluster_offset(d), &mut saved)?;
        let mut cur = d;
        loop {
            let src = inv[cur as usize];
            if src == d {
                w.write_at(b.cluster_offset(cur), &saved)?;
                done[cur as usize] = true;
                moved += 1;
                break;
            }
            copy(w, src, cur, &mut buf)?;
            done[cur as usize] = true;
            moved += 1;
            cur = src;
        }
    }

    // New FAT.
    let eoc = b.eoc();
    for v in fat.entries.iter_mut().skip(2) {
        *v = 0;
    }
    for c in &order {
        for (k, &old) in c.iter().enumerate() {
            let new = map[old as usize];
            fat.entries[new as usize] = if k + 1 == c.len() { eoc } else { new + 1 };
        }
    }
    store_fat(w, &b, &mut fat)?;

    // Patch first-cluster fields, at the entries' new locations.
    let new_chain = |c: &[u32]| -> Vec<u32> { c.iter().map(|&o| map[o as usize]).collect() };
    let dir_first = |d: usize| -> u32 {
        tree.dirs[d]
            .chain
            .first()
            .map(|&o| map[o as usize])
            .unwrap_or(0)
    };
    for e in &tree.entries {
        let new_first = match e.dot {
            1 => dir_first(e.dir),
            2 => match tree.dirs[e.dir].parent {
                Some(0) | None => 0,
                Some(p) => dir_first(p),
            },
            _ => e.chain.first().map(|&o| map[o as usize]).unwrap_or(0),
        };
        if new_first == first_cluster(&b, &e.raw) {
            continue;
        }
        let at = entry_offset(&b, &new_chain(&tree.dirs[e.dir].chain), e.index);
        let mut patch = e.raw;
        put_le16(&mut patch, 26, new_first as u16);
        if b.fat_bits == 32 {
            put_le16(&mut patch, 20, (new_first >> 16) as u16);
        }
        w.write_at(at, &patch)?;
    }

    let used = u64::from(used_end - 2);
    let free = u64::from(b.clusters) - used;
    if b.fat_bits == 32 {
        let mut boot = read_boot(w)?;
        put_le32(&mut boot, 44, dir_first(0));
        write_boot(w, &b, &boot)?;
        if b.fsinfo != 0 && b.fsinfo < b.reserved {
            let mut fi = vec![0u8; 512];
            w.read_at(b.fsinfo * b.bps, &mut fi)?;
            if &fi[0..4] == b"RRaA" && &fi[484..488] == b"rrAa" {
                put_le32(&mut fi, 488, free as u32);
                put_le32(&mut fi, 492, used_end);
                w.write_at(b.fsinfo * b.bps, &fi)?;
            }
        }
    }
    if zero_free {
        let zeros = vec![0u8; cs];
        for c in used_end..(b.clusters + 2) {
            w.write_at(b.cluster_offset(c), &zeros)?;
        }
    }

    report.moved_clusters = moved;
    report.used_clusters = used;
    report.free_clusters = free;
    report.lost_clusters = 0;
    report.fragmented = 0;
    report.fragments = order.iter().filter(|c| !c.is_empty()).count() as u64;
    Ok(report)
}

/// Copy attributes and timestamps (which fatfs cannot set) from entries of `src` to the
/// entries with the same path in `dst`. Used after rebuilding a volume in a new format.
pub(crate) fn copy_metadata(src: &mut Window, dst: &mut Window) -> Result<()> {
    let sb = Bpb::parse(&read_boot(src)?)?;
    let st = walk(src, &sb)?;
    let db = Bpb::parse(&read_boot(dst)?)?;
    let dt = walk(dst, &db)?;
    let by_path: std::collections::HashMap<String, [u8; 32]> = st
        .entries
        .iter()
        .filter(|e| e.dot == 0)
        .map(|e| (e.path.to_lowercase(), e.raw))
        .collect();
    for e in dt.entries.iter().filter(|e| e.dot == 0) {
        let Some(s) = by_path.get(&e.path.to_lowercase()) else {
            continue;
        };
        if s[11] & 0x10 != e.raw[11] & 0x10 {
            continue;
        }
        let mut p = e.raw;
        p[11..20].copy_from_slice(&s[11..20]);
        p[22..26].copy_from_slice(&s[22..26]);
        if p != e.raw {
            w_entry(dst, &db, &dt.dirs[e.dir].chain, e.index, &p)?;
        }
    }
    Ok(())
}

fn w_entry(w: &mut Window, b: &Bpb, chain: &[u32], index: u64, raw: &[u8; 32]) -> Result<()> {
    w.write_at(entry_offset(b, chain, index), raw)?;
    Ok(())
}
