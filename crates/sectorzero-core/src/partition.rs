//! Disk layouts: bare volumes, ISO 9660, MBR (with extended/logical partitions) and GPT.
//! Also writes new MBR and GPT partition tables.

use std::io;

use serde::Serialize;

use crate::container::Disk;
use crate::error::{Error, Result};
use crate::util::*;

pub(crate) const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
pub(crate) const EFI_SYSTEM: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
pub(crate) const LINUX_FS: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
/// New partitions start at 1 MiB, like current Windows and Linux tools.
pub(crate) const ALIGN_SECTORS: u64 = 2048;
const GPT_ENTRIES: u64 = 128;
const GPT_ENTRY_SIZE: u64 = 128;

/// Partition table to put on a new disk image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PartitionScheme {
    /// A bare volume (superfloppy): the file system starts at sector 0.
    #[default]
    None,
    Mbr,
    Gpt,
}

#[derive(Debug, Clone, Serialize)]
pub struct PartitionInfo {
    /// 1-based. MBR: 1–4 primary, 5+ logical. GPT: entry number.
    pub index: usize,
    /// "mbr" or "gpt".
    pub scheme: &'static str,
    /// MBR type byte as `0x0C`, or the GPT type GUID.
    pub type_id: String,
    pub type_name: String,
    /// GPT partition name (empty for MBR).
    pub name: String,
    pub bootable: bool,
    pub start_lba: u64,
    pub sectors: u64,
    pub sector_size: u32,
    /// File system found in the partition: "FAT", "NTFS", "exFAT", "ext2", "ext3", "ext4",
    /// or None.
    pub fs: Option<&'static str>,
    pub is_fat: bool,
}

impl PartitionInfo {
    pub fn offset(&self) -> u64 {
        self.start_lba * u64::from(self.sector_size)
    }
    pub fn size_bytes(&self) -> u64 {
        self.sectors * u64::from(self.sector_size)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "partitions")]
pub enum Layout {
    /// A file system at offset 0 with no partition table (floppies, WinImage volume images).
    BareVolume,
    /// An ISO 9660 / Joliet CD or DVD image.
    Iso,
    Mbr(Vec<PartitionInfo>),
    Gpt(Vec<PartitionInfo>),
    Unknown,
}

impl Layout {
    pub fn partitions(&self) -> &[PartitionInfo] {
        match self {
            Layout::Mbr(p) | Layout::Gpt(p) => p,
            _ => &[],
        }
    }
}

/// Does this sector look like a FAT boot sector (as opposed to an MBR)?
pub(crate) fn looks_like_fat_boot_sector(s: &[u8]) -> bool {
    let jump_ok = s[0] == 0xE9 || (s[0] == 0xEB && s[2] == 0x90) || s[0] == 0xEB;
    let bps = le16(s, 11);
    let spc = s[13];
    let reserved = le16(s, 14);
    let fats = s[16];
    let total = if le16(s, 19) != 0 {
        u32::from(le16(s, 19))
    } else {
        le32(s, 32)
    };
    jump_ok
        && &s[3..11] != b"NTFS    "
        && &s[3..11] != b"EXFAT   "
        && matches!(bps, 512 | 1024 | 2048 | 4096)
        && spc != 0
        && spc.is_power_of_two()
        && reserved >= 1
        && (1..=4).contains(&fats)
        && total != 0
        && s[21] >= 0xF0
}

/// The file system whose boot sector or superblock starts `s` (the first 2 KiB of a volume;
/// with only 512 bytes, ext is not detected).
pub(crate) fn fs_of(s: &[u8]) -> Option<&'static str> {
    boot_fs_of(s).or_else(|| crate::ext::fs_name(s))
}

fn boot_fs_of(s: &[u8]) -> Option<&'static str> {
    if &s[3..11] == b"NTFS    " {
        Some("NTFS")
    } else if &s[3..11] == b"EXFAT   " {
        Some("exFAT")
    } else if looks_like_fat_boot_sector(s) {
        Some("FAT")
    } else {
        None
    }
}

fn mbr_type_name(id: u8) -> &'static str {
    match id {
        0x01 => "FAT12",
        0x04 => "FAT16 <32M",
        0x05 => "Extended",
        0x06 => "FAT16",
        0x07 => "NTFS/exFAT",
        0x0B => "FAT32 (CHS)",
        0x0C => "FAT32 (LBA)",
        0x0E => "FAT16 (LBA)",
        0x0F => "Extended (LBA)",
        0x11 | 0x14 | 0x16 | 0x1B | 0x1C | 0x1E => "Hidden FAT",
        0x17 => "Hidden NTFS",
        0x27 => "Windows recovery",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x85 => "Linux extended",
        0x8E => "Linux LVM",
        0xA5 => "FreeBSD",
        0xA6 => "OpenBSD",
        0xAF => "HFS+",
        0xEE => "GPT protective",
        0xEF => "EFI system",
        0xFD => "Linux RAID",
        _ => "Unknown",
    }
}

fn gpt_type_name(g: &str) -> &'static str {
    match g {
        EFI_SYSTEM => "EFI system",
        BASIC_DATA => "Basic data",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery",
        "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3" => "LDM metadata",
        "AF9B60A0-1431-4F62-BC68-3311714A69AD" => "LDM data",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "E6D6D379-F507-44C2-A23C-238F2A3DF928" => "Linux LVM",
        "21686148-6449-6E6F-744E-656564454649" => "BIOS boot",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        "516E7CB4-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD",
        _ => "Unknown",
    }
}

fn read(disk: &mut dyn Disk, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; len];
    disk.read_at(offset, &mut b)?;
    Ok(b)
}

/// Read up to `len` bytes at `offset`, zero-filling past the end of the disk.
fn read_head(disk: &mut dyn Disk, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; len];
    let n = disk.size().saturating_sub(offset).min(len as u64) as usize;
    if n > 0 {
        disk.read_at(offset, &mut b[..n])?;
    }
    Ok(b)
}

fn probe_fs(disk: &mut dyn Disk, offset: u64) -> io::Result<Option<&'static str>> {
    Ok(fs_of(&read_head(disk, offset, 2048)?))
}

pub(crate) fn is_iso(disk: &mut dyn Disk) -> io::Result<bool> {
    if disk.size() < 17 * 2048 {
        return Ok(false);
    }
    let pvd = read(disk, 16 * 2048, 8)?;
    Ok(&pvd[1..6] == b"CD001")
}

pub(crate) fn detect(disk: &mut dyn Disk) -> Result<Layout> {
    let ss = u64::from(disk.sector_size());
    let head = read_head(disk, 0, 2048)?;
    let s0 = head[..512].to_vec();
    if boot_fs_of(&s0).is_some() {
        return Ok(Layout::BareVolume);
    }
    // An ext superblock at byte 1024, unless sector 0 is a GPT protective MBR (whose entry
    // array could in theory look like one).
    let protective =
        s0[510] == 0x55 && s0[511] == 0xAA && (0..4).any(|i| s0[446 + i * 16 + 4] == 0xEE);
    if !protective && crate::ext::fs_name(&head).is_some() {
        return Ok(Layout::BareVolume);
    }
    if is_iso(disk)? {
        return Ok(Layout::Iso);
    }
    if s0[510] != 0x55 || s0[511] != 0xAA {
        return Ok(Layout::Unknown);
    }
    let entries: Vec<&[u8]> = (0..4).map(|i| &s0[446 + i * 16..462 + i * 16]).collect();
    if entries.iter().any(|e| e[4] == 0xEE)
        && let Some(parts) = read_gpt(disk, ss)?
    {
        return Ok(Layout::Gpt(parts));
    }
    let mut parts = Vec::new();
    let mut extended = None;
    for (i, e) in entries.iter().enumerate() {
        let (type_id, start, sectors) = (e[4], u64::from(le32(e, 8)), u64::from(le32(e, 12)));
        if type_id == 0 || sectors == 0 {
            continue;
        }
        if matches!(type_id, 0x05 | 0x0F | 0x85) {
            extended = Some(start);
        }
        parts.push(mbr_part(disk, i + 1, e, start, ss)?);
    }
    if let Some(ext_start) = extended {
        // Walk the EBR chain: each EBR holds one logical partition (relative to the EBR)
        // and a link to the next EBR (relative to the extended partition start).
        let mut ebr = ext_start;
        let mut index = 5;
        let mut seen = 0;
        while seen < 128 {
            seen += 1;
            let s = read(disk, ebr * ss, 512)?;
            if s[510] != 0x55 || s[511] != 0xAA {
                break;
            }
            let (e0, e1) = (&s[446..462], &s[462..478]);
            if e0[4] != 0 && le32(e0, 12) != 0 {
                let start = ebr + u64::from(le32(e0, 8));
                parts.push(mbr_part(disk, index, e0, start, ss)?);
                index += 1;
            }
            if e1[4] == 0 || le32(e1, 12) == 0 {
                break;
            }
            ebr = ext_start + u64::from(le32(e1, 8));
        }
    }
    if parts.is_empty() {
        return Ok(Layout::Unknown);
    }
    Ok(Layout::Mbr(parts))
}

fn mbr_part(
    disk: &mut dyn Disk,
    index: usize,
    e: &[u8],
    start: u64,
    ss: u64,
) -> io::Result<PartitionInfo> {
    let type_id = e[4];
    let ext = matches!(type_id, 0x05 | 0x0F | 0x85);
    let fs = if ext {
        None
    } else {
        probe_fs(disk, start * ss)?
    };
    Ok(PartitionInfo {
        index,
        scheme: "mbr",
        type_id: format!("0x{type_id:02X}"),
        type_name: mbr_type_name(type_id).into(),
        name: String::new(),
        bootable: e[0] == 0x80,
        start_lba: start,
        sectors: u64::from(le32(e, 12)),
        sector_size: ss as u32,
        fs,
        is_fat: fs == Some("FAT"),
    })
}

fn gpt_header_ok(h: &[u8]) -> bool {
    if &h[0..8] != b"EFI PART" {
        return false;
    }
    let size = le32(h, 12) as usize;
    if !(92..=h.len()).contains(&size) {
        return false;
    }
    let mut c = h[..size].to_vec();
    put_le32(&mut c, 16, 0);
    crc32fast::hash(&c) == le32(h, 16)
}

fn read_gpt(disk: &mut dyn Disk, ss: u64) -> Result<Option<Vec<PartitionInfo>>> {
    let total = disk.size() / ss;
    let mut header = None;
    for lba in [1, total.saturating_sub(1)] {
        let h = read(disk, lba * ss, ss as usize)?;
        if gpt_header_ok(&h) {
            header = Some(h);
            break;
        }
    }
    let Some(h) = header else { return Ok(None) };
    let (entries_lba, count, esize) = (le64(&h, 72), le32(&h, 80) as u64, le32(&h, 84) as u64);
    if !(128..=4096).contains(&esize) || count > 4096 {
        return Err(Error::Invalid(
            "GPT header has an invalid entry size or count".into(),
        ));
    }
    let table = read(disk, entries_lba * ss, (count * esize) as usize)?;
    if crc32fast::hash(&table) != le32(&h, 88) {
        return Err(Error::Invalid(
            "GPT partition entries checksum mismatch".into(),
        ));
    }
    let mut parts = Vec::new();
    for i in 0..count as usize {
        let e = &table[i * esize as usize..(i + 1) * esize as usize];
        if is_zero(&e[0..16]) {
            continue;
        }
        let (first, last) = (le64(e, 32), le64(e, 40));
        if last < first {
            continue;
        }
        let type_guid = guid_string(&e[0..16]);
        let name: String =
            char::decode_utf16((0..36).map(|k| le16(e, 56 + k * 2)).take_while(|&u| u != 0))
                .map(|r| r.unwrap_or('\u{FFFD}'))
                .collect();
        let fs = probe_fs(disk, first * ss)?;
        parts.push(PartitionInfo {
            index: i + 1,
            scheme: "gpt",
            type_name: gpt_type_name(&type_guid).into(),
            type_id: type_guid,
            name,
            bootable: le64(e, 48) & 4 != 0,
            start_lba: first,
            sectors: last - first + 1,
            sector_size: ss as u32,
            fs,
            is_fat: fs == Some("FAT"),
        });
    }
    Ok(Some(parts))
}

/// CHS address for an MBR entry (255 heads, 63 sectors; saturates past cylinder 1023).
fn chs(lba: u64) -> [u8; 3] {
    let (spt, heads) = (63u64, 255u64);
    let c = lba / (spt * heads);
    if c > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    let h = (lba / spt) % heads;
    let s = lba % spt + 1;
    [h as u8, (s as u8) | (((c >> 8) as u8) << 6), c as u8]
}

fn mbr_entry(mbr: &mut [u8], slot: usize, boot: bool, type_id: u8, start: u64, sectors: u64) {
    let e = 446 + slot * 16;
    mbr[e] = if boot { 0x80 } else { 0 };
    mbr[e + 1..e + 4].copy_from_slice(&chs(start));
    mbr[e + 4] = type_id;
    mbr[e + 5..e + 8].copy_from_slice(&chs(start + sectors - 1));
    put_le32(mbr, e + 8, start.min(0xFFFF_FFFF) as u32);
    put_le32(mbr, e + 12, sectors.min(0xFFFF_FFFF) as u32);
}

/// Where the single data partition of a new disk goes: (first LBA, sectors).
pub(crate) fn new_partition_extent(
    scheme: PartitionScheme,
    disk_sectors: u64,
) -> Result<(u64, u64)> {
    let end = match scheme {
        PartitionScheme::None => return Ok((0, disk_sectors)),
        PartitionScheme::Mbr => disk_sectors,
        // Leave room for the backup entries and header.
        PartitionScheme::Gpt => disk_sectors.saturating_sub(1 + GPT_ENTRIES * GPT_ENTRY_SIZE / 512),
    };
    if end <= ALIGN_SECTORS + 64 {
        return Err(Error::Invalid(
            "disk is too small for a partition table (minimum about 2 MiB)".into(),
        ));
    }
    if scheme == PartitionScheme::Mbr && disk_sectors > 0xFFFF_FFFF {
        return Err(Error::Invalid(
            "MBR disks are limited to 2 TiB; use GPT".into(),
        ));
    }
    Ok((ALIGN_SECTORS, end - ALIGN_SECTORS))
}

/// MBR type byte for a freshly formatted FAT partition.
pub(crate) fn mbr_type_for(fat_bits: u8, sectors: u64) -> u8 {
    match fat_bits {
        12 => 0x01,
        16 if sectors < 65536 => 0x04,
        16 => 0x0E,
        _ => 0x0C,
    }
}

/// Write an MBR with one partition.
pub(crate) fn write_mbr(
    disk: &mut dyn Disk,
    type_id: u8,
    start: u64,
    sectors: u64,
) -> io::Result<()> {
    let mut mbr = vec![0u8; 512];
    mbr[440..444].copy_from_slice(&new_guid()[0..4]);
    mbr_entry(&mut mbr, 0, true, type_id, start, sectors);
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    disk.write_at(0, &mbr)
}

/// Write a protective MBR plus primary and backup GPT with one partition.
pub(crate) fn write_gpt(
    disk: &mut dyn Disk,
    type_guid: &str,
    name: &str,
    start: u64,
    sectors: u64,
) -> io::Result<()> {
    let total = disk.size() / 512;
    let mut mbr = vec![0u8; 512];
    mbr_entry(&mut mbr, 0, false, 0xEE, 1, (total - 1).min(0xFFFF_FFFF));
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    disk.write_at(0, &mbr)?;

    let table_sectors = GPT_ENTRIES * GPT_ENTRY_SIZE / 512;
    let mut entries = vec![0u8; (GPT_ENTRIES * GPT_ENTRY_SIZE) as usize];
    entries[0..16].copy_from_slice(&guid(type_guid));
    entries[16..32].copy_from_slice(&new_guid());
    put_le64(&mut entries, 32, start);
    put_le64(&mut entries, 40, start + sectors - 1);
    for (k, u) in name.encode_utf16().take(36).enumerate() {
        put_le16(&mut entries, 56 + k * 2, u);
    }
    let entries_crc = crc32fast::hash(&entries);
    let disk_guid = new_guid();
    let header = |current: u64, backup: u64, entries_lba: u64| {
        let mut h = vec![0u8; 512];
        h[0..8].copy_from_slice(b"EFI PART");
        put_le32(&mut h, 8, 0x0001_0000);
        put_le32(&mut h, 12, 92);
        put_le64(&mut h, 24, current);
        put_le64(&mut h, 32, backup);
        put_le64(&mut h, 40, 2 + table_sectors);
        put_le64(&mut h, 48, total - 2 - table_sectors);
        h[56..72].copy_from_slice(&disk_guid);
        put_le64(&mut h, 72, entries_lba);
        put_le32(&mut h, 80, GPT_ENTRIES as u32);
        put_le32(&mut h, 84, GPT_ENTRY_SIZE as u32);
        put_le32(&mut h, 88, entries_crc);
        let crc = crc32fast::hash(&h[..92]);
        put_le32(&mut h, 16, crc);
        h
    };
    disk.write_at(512, &header(1, total - 1, 2))?;
    disk.write_at(2 * 512, &entries)?;
    disk.write_at((total - 1 - table_sectors) * 512, &entries)?;
    disk.write_at(
        (total - 1) * 512,
        &header(total - 1, 1, total - 1 - table_sectors),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chs_encoding() {
        assert_eq!(chs(0), [0, 1, 0]);
        assert_eq!(chs(2048), [32, 33, 0]);
        assert_eq!(chs(u64::from(u32::MAX)), [0xFE, 0xFF, 0xFF]);
    }
}
