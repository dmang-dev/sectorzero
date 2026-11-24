//! A mounted volume inside an image (FAT read/write via fatfs, ext2/ext3 read/write and
//! ext4 read via `ext`, NTFS and ISO 9660 read-only), and the WinImage-style operations on it.
//!
//! Paths inside the image use `/` (a `\` is accepted too) and are relative to the root;
//! `""` or `"/"` is the root. Name matching is case-insensitive.

use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::time::SystemTime;

use chrono::{Datelike, Local, TimeZone};
use fatfs::{FatType, FileAttributes, FileSystem, FormatVolumeOptions, FsOptions};
use serde::Serialize;

use crate::container::{self, ContainerInfo, ContainerKind, SharedDisk};
use crate::error::{Error, Result, display};
use crate::exfat::ExFatFs;
use crate::ext::{ExtFormat, ExtFs};
use crate::fatraw;
use crate::format::{FloppyFormat, MediaFormat, MediaLayout, NewImage};
use crate::hfs::{HfsFs, Kind as HfsKind};
use crate::image::{Image, OpenOptions};
use crate::iso::{IsoFs, IsoInfo};
use crate::ntfsfs::NtfsFs;
use crate::partition::{self, BASIC_DATA, Layout, PartitionInfo, PartitionScheme};
use crate::util::*;
use crate::window::{Window, lock};

type FatFs = FileSystem<Window>;
type FatDir<'a> = fatfs::Dir<'a, Window>;
type FatEntry<'a> = fatfs::DirEntry<'a, Window>;

#[derive(Debug, Clone, Serialize)]
pub struct Attributes {
    pub read_only: bool,
    pub hidden: bool,
    pub system: bool,
    pub archive: bool,
}

impl Attributes {
    /// `RHSA`-style flags, with `-` for unset ones.
    pub fn flags(&self) -> String {
        [
            (self.read_only, 'R'),
            (self.hidden, 'H'),
            (self.system, 'S'),
            (self.archive, 'A'),
        ]
        .iter()
        .map(|&(on, c)| if on { c } else { '-' })
        .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Long file name (or the 8.3 name when there is no long name).
    pub name: String,
    pub short_name: String,
    /// Full path inside the image, without a leading `/`.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// `YYYY-MM-DD HH:MM:SS` (local time for FAT).
    pub modified: String,
    /// `YYYY-MM-DD HH:MM:SS`, or empty when the file system has none.
    pub created: String,
    /// `YYYY-MM-DD` (FAT keeps only the date), or empty.
    pub accessed: String,
    pub attributes: Attributes,
    /// Macintosh file information (HFS / HFS+ files only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<MacInfo>,
}

/// Finder information of a Macintosh file.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MacInfo {
    /// Four-character file type, e.g. `TEXT`, `APPL`.
    pub file_type: String,
    /// Four-character creator code, e.g. `ttxt`.
    pub creator: String,
    /// Size of the resource fork in bytes.
    pub resource_size: u64,
}

/// What to do with Macintosh resource forks and Finder information when extracting from
/// HFS / HFS+ (other file systems have neither and ignore this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ForkMode {
    /// The data fork only.
    #[default]
    Data,
    /// The data fork, plus a `._name` AppleDouble file holding the resource fork and
    /// Finder information when there is any (what macOS writes on non-Mac disks).
    AppleDouble,
    /// One MacBinary III `name.bin` file per file, holding both forks and the Finder
    /// information (what Mac emulators and archives use).
    MacBinary,
}

impl ForkMode {
    pub fn parse(s: &str) -> Option<ForkMode> {
        match s.to_ascii_lowercase().as_str() {
            "data" | "" => Some(ForkMode::Data),
            "appledouble" | "double" => Some(ForkMode::AppleDouble),
            "macbinary" | "bin" => Some(ForkMode::MacBinary),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeInfo {
    /// "FAT12", "FAT16", "FAT32" or "ISO 9660".
    pub fs_type: String,
    pub label: String,
    pub volume_id: String,
    pub oem_name: String,
    pub bytes_per_sector: u16,
    pub cluster_size: u32,
    pub total_clusters: u32,
    pub free_clusters: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Byte offset of the volume inside the virtual disk (non-zero for a partition).
    pub volume_offset: u64,
    pub volume_size: u64,
    pub layout: Layout,
    pub partition: Option<PartitionInfo>,
    /// The standard floppy geometry this matches, if any.
    pub floppy_format: Option<&'static str>,
    /// The removable medium (Zip, Jaz, SuperDisk, MO, …) whose capacity the disk has.
    pub media_format: Option<&'static str>,
    /// Opened for writing.
    pub writable: bool,
    /// Whether this file system can be modified at all (ISO 9660 and NTFS cannot).
    pub fs_writable: bool,
    /// Why it cannot, when `fs_writable` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_reason: Option<String>,
    pub container: ContainerInfo,
    pub iso: Option<IsoInfo>,
}

/// File system for [`Volume::create`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FsKind {
    /// FAT12/16/32, picked from the size (or the floppy format / custom layout).
    #[default]
    Fat,
    Ext2,
    /// ext2 plus a journal.
    Ext3,
    /// Extents and metadata checksums; journaled when the volume is 2 MiB or more.
    Ext4,
    /// exFAT (from 1 MiB; MBR type 0x07 / GPT basic data when partitioned).
    Exfat,
    /// HFS, Mac OS Standard (from 400 KB; Apple_HFS in an Apple partition map, MBR type
    /// 0xAF, GPT Apple HFS).
    Hfs,
    /// An empty HFS+ volume (SectorZero reads HFS+ but does not write to it yet).
    #[doc(hidden)]
    HfsPlus,
}

impl FsKind {
    pub fn parse(s: &str) -> Option<FsKind> {
        match s.to_ascii_lowercase().as_str() {
            "fat" | "" => Some(FsKind::Fat),
            "ext2" => Some(FsKind::Ext2),
            "ext3" => Some(FsKind::Ext3),
            "ext4" => Some(FsKind::Ext4),
            "exfat" => Some(FsKind::Exfat),
            "hfs" => Some(FsKind::Hfs),
            "hfs+" | "hfsplus" => Some(FsKind::HfsPlus),
            _ => None,
        }
    }
}

/// Options for [`Volume::create`].
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub label: Option<String>,
    pub fs: FsKind,
    /// Container format. Default: from the file extension (`.vhdx`, `.imz`, …; raw otherwise).
    pub container: Option<ContainerKind>,
    /// Partition table for hard disk images. Floppies are always bare.
    pub scheme: PartitionScheme,
    /// Sparse/dynamic container (VHD, VHDX, VMDK). Fixed otherwise.
    pub dynamic: bool,
    pub overwrite: bool,
}

enum Fs {
    Fat(Box<FatFs>),
    Iso(Box<IsoFs>),
    Ext(Box<ExtFs>),
    Exfat(Box<ExFatFs>),
    Ntfs(Box<NtfsFs>),
    Hfs(Box<HfsFs>),
}

/// Result of [`Volume::resize_disk`].
#[derive(Debug, Clone, Serialize)]
pub struct DiskResize {
    pub old_size: u64,
    pub new_size: u64,
    /// The partition that changed size, if any.
    pub partition: Option<PartitionResize>,
    /// Files copied into the rebuilt partition.
    pub stats: Option<Stats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PartitionResize {
    pub index: usize,
    pub old_bytes: u64,
    pub new_bytes: u64,
}

/// Syncs the container when dropped, after the file system (declared before it) unmounted.
struct SyncOnDrop(SharedDisk);

impl Drop for SyncOnDrop {
    fn drop(&mut self) {
        if let Ok(mut d) = self.0.lock() {
            let _ = d.sync();
        }
    }
}

pub struct Volume {
    fs: Fs,
    _sync: SyncOnDrop,
    image: Image,
    boot: [u8; 512],
}

/// The partition table around a new HFS volume.
fn write_hfs_table(
    d: &mut dyn container::Disk,
    scheme: PartitionScheme,
    label: &str,
    start: u64,
    sectors: u64,
) -> Result<()> {
    match scheme {
        PartitionScheme::Apm => partition::write_apm(d, "Apple_HFS", label, start, sectors)?,
        PartitionScheme::Mbr => partition::write_mbr(d, 0xAF, start, sectors)?,
        PartitionScheme::Gpt => {
            partition::write_gpt(d, partition::APPLE_HFS, label, start, sectors)?
        }
        PartitionScheme::None => {}
    }
    Ok(())
}

/// Format a FAT file system into `w`. `hidden` is the partition's first LBA.
fn format_fat(
    w: Window,
    spec: &NewImage,
    label: Option<[u8; 11]>,
    hidden: u64,
    volume_id: Option<u32>,
) -> Result<()> {
    let ss = match spec {
        NewImage::Media(m) => m.sector_size,
        _ => 512,
    };
    let sectors = w.len() / u64::from(ss);
    if sectors < 64 {
        return Err(Error::Invalid("image is too small".into()));
    }
    let total = u32::try_from(sectors)
        .map_err(|_| Error::Invalid("volume is too large for FAT (2 TiB limit)".into()))?;
    let mut fmt = FormatVolumeOptions::new()
        .bytes_per_sector(ss as u16)
        .total_sectors(total)
        .fats(2)
        .volume_id(volume_id.unwrap_or_else(new_volume_id));
    fmt = match spec {
        NewImage::Floppy(f) => fmt
            .fat_type(FatType::Fat12)
            .bytes_per_cluster(f.bytes_per_cluster)
            .max_root_dir_entries(f.root_entries)
            .media(f.media)
            .sectors_per_track(f.sectors_per_track)
            .heads(f.heads)
            .drive_num(0x00),
        // WinImage's defaults for hard disk / removable volume images.
        NewImage::Volume { .. } if hidden == 0 => fmt
            .media(0xF8)
            .sectors_per_track(32)
            .heads(64)
            .drive_num(0x80),
        NewImage::Volume { .. } => fmt
            .media(0xF8)
            .sectors_per_track(63)
            .heads(255)
            .drive_num(0x80),
        NewImage::Media(m) => {
            let mut f = fmt
                .media(m.media)
                .sectors_per_track(m.sectors_per_track)
                .heads(m.heads)
                .drive_num(0x80);
            if m.bytes_per_cluster != 0 {
                f = f.bytes_per_cluster(m.bytes_per_cluster);
            }
            match m.fat_bits {
                12 => f.fat_type(FatType::Fat12),
                16 => f.fat_type(FatType::Fat16),
                32 => f.fat_type(FatType::Fat32),
                _ => f,
            }
        }
        NewImage::Custom(g) => {
            g.validate()?;
            let mut f = fmt
                .fats(g.fats)
                .media(if g.media == 0 { 0xF8 } else { g.media })
                .sectors_per_track(g.sectors_per_track)
                .heads(g.heads)
                .drive_num(g.drive_number);
            if g.bytes_per_cluster != 0 {
                f = f.bytes_per_cluster(g.bytes_per_cluster);
            }
            if g.root_entries != 0 {
                f = f.max_root_dir_entries(g.root_entries);
            }
            match g.fat_bits {
                12 => f = f.fat_type(FatType::Fat12),
                16 => f = f.fat_type(FatType::Fat16),
                32 => f = f.fat_type(FatType::Fat32),
                _ => {}
            }
            f
        }
    };
    if let Some(l) = label {
        fmt = fmt.volume_label(l);
    }
    let mut disk = w.reopen();
    fatfs::format_volume(w, fmt).map_err(|e| Error::Invalid(format!("cannot format: {e}")))?;
    if let NewImage::Custom(g) = spec
        && g.hidden_sectors != 0
        && hidden == 0
    {
        let mut boot = [0u8; 512];
        disk.read_at(0, &mut boot)?;
        put_le32(&mut boot, 28, g.hidden_sectors);
        disk.write_at(0, &boot)?;
        let b = fatraw::Bpb::parse(&boot)?;
        if b.backup_boot != 0 {
            disk.write_at(b.backup_boot * 512, &boot)?;
        }
    }
    Ok(())
}

impl Volume {
    /// Open the volume in an image file.
    pub fn open(path: &Path, opts: &OpenOptions) -> Result<Volume> {
        let image = Image::open(path, opts)?;
        let w = image.window();
        let mut boot = [0u8; 512];
        let mut head = vec![0u8; 2048];
        {
            let mut w = w.reopen();
            w.read_at(0, &mut head)?;
        }
        boot.copy_from_slice(&head[..512]);
        let fs_name = match &image.layout {
            Layout::Iso => Some("ISO 9660"),
            _ => partition::fs_of(&head),
        };
        let fs = match fs_name {
            Some("ISO 9660") => Fs::Iso(Box::new(IsoFs::open(w)?)),
            Some("FAT") => {
                let fs = FileSystem::new(w, FsOptions::new().update_accessed_date(false))
                    .map_err(|e| Error::NotFat(e.to_string()))?;
                Fs::Fat(Box::new(fs))
            }
            Some("ext2" | "ext3" | "ext4") => Fs::Ext(Box::new(ExtFs::open(w, image.writable)?)),
            Some("exFAT") => Fs::Exfat(Box::new(ExFatFs::open(w, image.writable)?)),
            Some("NTFS") => Fs::Ntfs(Box::new(NtfsFs::open(w)?)),
            Some("HFS" | "HFS+" | "HFSX") => Fs::Hfs(Box::new(HfsFs::open(w)?)),
            Some(other) => {
                return Err(Error::Unsupported(format!(
                    "{other} volumes (FAT, exFAT, ext2/3/4, NTFS, HFS, HFS+ and ISO 9660 are readable)"
                )));
            }
            None => {
                return Err(match &image.partition {
                    Some(p) => Error::NotFat(format!(
                        "partition {} ({}) has no file system SectorZero can read",
                        p.index, p.type_name
                    )),
                    None => Error::NotFat("no FAT, exFAT, ext or NTFS file system found".into()),
                });
            }
        };
        Ok(Volume {
            fs,
            _sync: SyncOnDrop(image.disk.clone()),
            image,
            boot,
        })
    }

    /// A removable medium preset: the vendor's layout (superfloppy, or one partition in a
    /// given MBR slot) in the drive's own geometry.
    fn create_media(path: &Path, m: &'static MediaFormat, opts: &CreateOptions) -> Result<()> {
        if opts.scheme != PartitionScheme::None {
            return Err(Error::Invalid(format!(
                "{} images come with their own layout; leave out the partition table",
                m.name
            )));
        }
        let ss = u64::from(m.sector_size);
        if ss != 512 && opts.fs != FsKind::Fat {
            return Err(Error::Invalid(format!(
                "{} has {ss}-byte sectors: only FAT is supported on it",
                m.name
            )));
        }
        let hfs = matches!(opts.fs, FsKind::Hfs | FsKind::HfsPlus);
        let (start, slot) = match m.layout {
            MediaLayout::Superfloppy => (0, 0),
            // Mac-formatted cartridges carry an Apple partition map instead.
            MediaLayout::Mbr { .. } if hfs => (partition::APM_START, 0),
            MediaLayout::Mbr { slot, start } => (u64::from(start), slot),
        };
        let sectors = m.total_sectors - start;
        let label = match opts.fs {
            FsKind::Fat => opts.label.as_deref().map(encode_label).transpose()?,
            _ => None,
        };
        let kind = opts
            .container
            .unwrap_or_else(|| ContainerKind::from_path(path));
        let disk = container::share(container::create(
            path,
            kind,
            m.size_bytes(),
            opts.dynamic,
            opts.overwrite,
        )?);
        let result = (|| -> Result<()> {
            let w = Window::new(disk.clone(), start * ss, sectors * ss, true);
            if hfs {
                let label = opts.label.clone().unwrap_or_else(|| "Untitled".into());
                if opts.fs == FsKind::Hfs {
                    crate::hfs::format_hfs(w, &label)?;
                } else {
                    crate::hfs::format_hfsplus(w, &label)?;
                }
                let mut d = lock(&disk)?;
                if start != 0 {
                    write_hfs_table(d.as_mut(), PartitionScheme::Apm, &label, start, sectors)?;
                }
                d.sync()?;
                return Ok(());
            }
            let type_id = match opts.fs {
                FsKind::Fat => {
                    format_fat(w, &NewImage::Media(m), label, start, None)?;
                    let mut d = lock(&disk)?;
                    let mut boot = vec![0u8; ss as usize];
                    d.read_at(start * ss, &mut boot)?;
                    let bits = fatraw::Bpb::parse(&boot)?.fat_bits;
                    if start != 0 {
                        put_le32(&mut boot, 28, start as u32);
                        d.write_at(start * ss, &boot)?;
                        let backup = u64::from(le16(&boot, 50));
                        if bits == 32 && backup != 0 {
                            d.write_at((start + backup) * ss, &boot)?;
                        }
                    }
                    // CHS types (0x06, 0x0B) while the partition ends below cylinder
                    // 1024, as Iomega's tools wrote them; LBA types beyond.
                    let chs_ok = m.total_sectors
                        <= 1024 * u64::from(m.heads) * u64::from(m.sectors_per_track);
                    match (bits, chs_ok) {
                        (16, true) if sectors >= 65536 => 0x06,
                        (32, true) => 0x0B,
                        _ => partition::mbr_type_for(bits, sectors),
                    }
                }
                FsKind::Exfat => {
                    crate::exfat::format(w, opts.label.as_deref(), start)?;
                    0x07
                }
                _ => {
                    crate::ext::format(
                        w,
                        &ExtFormat {
                            journal: opts.fs == FsKind::Ext3,
                            ext4: opts.fs == FsKind::Ext4,
                            label: opts.label.clone(),
                        },
                    )?;
                    0x83
                }
            };
            let mut d = lock(&disk)?;
            if slot != 0 {
                partition::write_media_mbr(
                    d.as_mut(),
                    slot,
                    type_id,
                    start,
                    sectors,
                    (m.heads, m.sectors_per_track),
                )?;
            }
            d.sync()?;
            Ok(())
        })();
        if result.is_err() {
            drop(disk);
            let _ = std::fs::remove_file(path);
        }
        result
    }

    /// Consistency problems in an ext file system (checksums, bitmaps, counts, links);
    /// empty when clean. A test aid: e2fsck remains the real check.
    #[doc(hidden)]
    pub fn ext_check(&self) -> Result<Vec<String>> {
        match &self.fs {
            Fs::Ext(e) => e.check(),
            _ => Err(Error::Unsupported("ext_check on a non-ext volume".into())),
        }
    }

    /// Consistency problems in an exFAT file system (boot checksums, entry set checksums,
    /// name hashes, cluster ownership against the bitmap); empty when clean. A test aid.
    #[doc(hidden)]
    pub fn exfat_check(&self) -> Result<Vec<String>> {
        match &self.fs {
            Fs::Exfat(e) => e.check(),
            _ => Err(Error::Unsupported(
                "exfat_check on a non-exFAT volume".into(),
            )),
        }
    }

    /// Consistency problems in an HFS or HFS+ file system (catalog threads, valences,
    /// counts, block ownership against the bitmap); empty when clean. A test aid.
    #[doc(hidden)]
    pub fn hfs_check(&self) -> Result<Vec<String>> {
        match &self.fs {
            Fs::Hfs(h) => h.check(),
            _ => Err(Error::Unsupported("hfs_check on a non-HFS volume".into())),
        }
    }

    /// Create a new, empty, formatted image file.
    pub fn create(path: &Path, spec: NewImage, opts: &CreateOptions) -> Result<()> {
        let size = spec.size_bytes();
        if matches!(spec, NewImage::Floppy(_) | NewImage::Custom(_))
            && opts.scheme != PartitionScheme::None
        {
            return Err(Error::Invalid(
                "floppy and custom-geometry images have no partition table".into(),
            ));
        }
        if let NewImage::Media(m) = spec {
            return Self::create_media(path, m, opts);
        }
        let label = match opts.fs {
            FsKind::Fat => opts.label.as_deref().map(encode_label).transpose()?,
            _ => None,
        };
        let hfs = matches!(opts.fs, FsKind::Hfs | FsKind::HfsPlus);
        if opts.scheme == PartitionScheme::Apm && !hfs {
            return Err(Error::Invalid(
                "Apple partition maps are for HFS volumes (use --fs hfs)".into(),
            ));
        }
        if hfs && matches!(spec, NewImage::Custom(_)) {
            return Err(Error::Invalid(
                "custom layouts are FAT boot sector parameters; they do not apply to HFS".into(),
            ));
        }
        let (start, sectors) = partition::new_partition_extent(opts.scheme, size / 512)?;
        let kind = opts
            .container
            .unwrap_or_else(|| ContainerKind::from_path(path));
        let disk = container::share(container::create(
            path,
            kind,
            size,
            opts.dynamic,
            opts.overwrite,
        )?);
        let result = (|| -> Result<()> {
            if hfs {
                let label = opts.label.clone().unwrap_or_else(|| "Untitled".into());
                let w = Window::new(disk.clone(), start * 512, sectors * 512, true);
                if opts.fs == FsKind::Hfs {
                    crate::hfs::format_hfs(w, &label)?;
                } else {
                    crate::hfs::format_hfsplus(w, &label)?;
                }
                let mut d = lock(&disk)?;
                write_hfs_table(d.as_mut(), opts.scheme, &label, start, sectors)?;
                d.sync()?;
                return Ok(());
            }
            if opts.fs == FsKind::Exfat {
                if matches!(spec, NewImage::Custom(_)) {
                    return Err(Error::Invalid(
                        "custom layouts are FAT boot sector parameters; they do not apply to exFAT"
                            .into(),
                    ));
                }
                crate::exfat::format(
                    Window::new(disk.clone(), start * 512, sectors * 512, true),
                    opts.label.as_deref(),
                    start,
                )?;
                let mut d = lock(&disk)?;
                match opts.scheme {
                    PartitionScheme::Mbr => partition::write_mbr(d.as_mut(), 0x07, start, sectors)?,
                    PartitionScheme::Gpt => partition::write_gpt(
                        d.as_mut(),
                        BASIC_DATA,
                        "Basic data partition",
                        start,
                        sectors,
                    )?,
                    PartitionScheme::None | PartitionScheme::Apm => {}
                }
                d.sync()?;
                return Ok(());
            }
            if opts.fs != FsKind::Fat {
                if matches!(spec, NewImage::Custom(_)) {
                    return Err(Error::Invalid(
                        "custom layouts are FAT boot sector parameters; they do not apply to ext"
                            .into(),
                    ));
                }
                crate::ext::format(
                    Window::new(disk.clone(), start * 512, sectors * 512, true),
                    &ExtFormat {
                        journal: opts.fs == FsKind::Ext3,
                        ext4: opts.fs == FsKind::Ext4,
                        label: opts.label.clone(),
                    },
                )?;
                let mut d = lock(&disk)?;
                match opts.scheme {
                    PartitionScheme::Mbr => partition::write_mbr(d.as_mut(), 0x83, start, sectors)?,
                    PartitionScheme::Gpt => partition::write_gpt(
                        d.as_mut(),
                        partition::LINUX_FS,
                        "Linux filesystem",
                        start,
                        sectors,
                    )?,
                    PartitionScheme::None | PartitionScheme::Apm => {}
                }
                d.sync()?;
                return Ok(());
            }
            format_fat(
                Window::new(disk.clone(), start * 512, sectors * 512, true),
                &spec,
                label,
                start,
                None,
            )?;
            let mut d = lock(&disk)?;
            if start != 0 {
                // fatfs has no hidden-sectors option: patch the BPB.
                let mut boot = [0u8; 512];
                d.read_at(start * 512, &mut boot)?;
                put_le32(&mut boot, 28, start as u32);
                d.write_at(start * 512, &boot)?;
                let bits = fatraw::Bpb::parse(&boot)?.fat_bits;
                if bits == 32 {
                    let backup = u64::from(le16(&boot, 50));
                    if backup != 0 {
                        d.write_at((start + backup) * 512, &boot)?;
                    }
                }
                match opts.scheme {
                    PartitionScheme::Mbr => partition::write_mbr(
                        d.as_mut(),
                        partition::mbr_type_for(bits, sectors),
                        start,
                        sectors,
                    )?,
                    PartitionScheme::Gpt => partition::write_gpt(
                        d.as_mut(),
                        BASIC_DATA,
                        "Basic data partition",
                        start,
                        sectors,
                    )?,
                    PartitionScheme::None | PartitionScheme::Apm => {}
                }
            }
            d.sync()?;
            Ok(())
        })();
        if let Err(e) = result {
            drop(disk);
            let _ = fs::remove_file(path);
            return Err(e);
        }
        Ok(())
    }

    pub fn image(&self) -> &Image {
        &self.image
    }

    pub fn info(&self) -> Result<VolumeInfo> {
        let mut i = self.fs_info()?;
        i.media_format = MediaFormat::matching(i.container.virtual_size).map(|m| m.name);
        Ok(i)
    }

    fn fs_info(&self) -> Result<VolumeInfo> {
        let container = self.image.container();
        let (offset, size) = (self.image.offset, self.image.len);
        match &self.fs {
            Fs::Iso(iso) => {
                let i = iso.info();
                let total = u64::from(i.volume_blocks) * u64::from(i.block_size);
                Ok(VolumeInfo {
                    fs_type: if i.joliet {
                        "ISO 9660 + Joliet"
                    } else if i.rock_ridge {
                        "ISO 9660 + Rock Ridge"
                    } else {
                        "ISO 9660"
                    }
                    .into(),
                    label: i.volume_id.clone(),
                    volume_id: String::new(),
                    oem_name: i.system_id.clone(),
                    bytes_per_sector: i.block_size as u16,
                    cluster_size: i.block_size,
                    total_clusters: i.volume_blocks,
                    free_clusters: 0,
                    total_bytes: total,
                    free_bytes: 0,
                    volume_offset: offset,
                    volume_size: size,
                    layout: self.image.layout.clone(),
                    partition: None,
                    media_format: None,
                    floppy_format: None,
                    writable: false,
                    fs_writable: false,
                    read_only_reason: Some("ISO 9660 images are read-only".into()),
                    container,
                    iso: Some(i.clone()),
                })
            }
            Fs::Ext(e) => {
                let i = e.info();
                let blocker = e.write_blocker();
                Ok(VolumeInfo {
                    fs_type: i.kind.into(),
                    label: i.label,
                    volume_id: i.uuid,
                    oem_name: String::new(),
                    bytes_per_sector: 512,
                    cluster_size: i.block_size as u32,
                    total_clusters: i.blocks.min(u64::from(u32::MAX)) as u32,
                    free_clusters: i.free_blocks.min(u64::from(u32::MAX)) as u32,
                    total_bytes: i.blocks * i.block_size,
                    free_bytes: i.free_blocks * i.block_size,
                    volume_offset: offset,
                    volume_size: size,
                    layout: self.image.layout.clone(),
                    partition: self.image.partition.clone(),
                    media_format: None,
                    floppy_format: None,
                    writable: self.image.writable && blocker.is_none(),
                    fs_writable: blocker.is_none(),
                    read_only_reason: blocker,
                    container,
                    iso: None,
                })
            }
            Fs::Exfat(e) => {
                let i = e.info();
                let blocker = e.write_blocker();
                Ok(VolumeInfo {
                    fs_type: "exFAT".into(),
                    label: i.label,
                    volume_id: format!(
                        "{:04X}-{:04X}",
                        (i.serial >> 16) & 0xFFFF,
                        i.serial & 0xFFFF
                    ),
                    oem_name: String::new(),
                    bytes_per_sector: i.sector_size as u16,
                    cluster_size: i.cluster_size as u32,
                    total_clusters: i.clusters,
                    free_clusters: i.free_clusters as u32,
                    total_bytes: u64::from(i.clusters) * i.cluster_size,
                    free_bytes: i.free_clusters * i.cluster_size,
                    volume_offset: offset,
                    volume_size: size,
                    layout: self.image.layout.clone(),
                    partition: self.image.partition.clone(),
                    media_format: None,
                    floppy_format: None,
                    writable: self.image.writable && blocker.is_none(),
                    fs_writable: blocker.is_none(),
                    read_only_reason: blocker,
                    container,
                    iso: None,
                })
            }
            Fs::Hfs(h) => {
                let i = h.info();
                let blocker = h.write_blocker();
                let cluster = u64::from(i.block_size);
                Ok(VolumeInfo {
                    fs_type: match i.kind {
                        HfsKind::Hfs => "HFS",
                        HfsKind::Plus if i.journaled => "HFS+ (journaled)",
                        HfsKind::Plus => "HFS+",
                        HfsKind::X => "HFSX",
                    }
                    .into(),
                    label: i.label,
                    volume_id: if i.uuid != 0 {
                        format!("{:016X}", i.uuid)
                    } else {
                        String::new()
                    },
                    oem_name: if i.wrapped {
                        "HFS wrapper".into()
                    } else {
                        String::new()
                    },
                    bytes_per_sector: 512,
                    cluster_size: i.block_size,
                    total_clusters: i.total_blocks,
                    free_clusters: i.free_blocks,
                    total_bytes: u64::from(i.total_blocks) * cluster,
                    free_bytes: u64::from(i.free_blocks) * cluster,
                    volume_offset: offset,
                    volume_size: size,
                    layout: self.image.layout.clone(),
                    partition: self.image.partition.clone(),
                    media_format: None,
                    floppy_format: None,
                    writable: self.image.writable && blocker.is_none(),
                    fs_writable: blocker.is_none(),
                    read_only_reason: blocker,
                    container,
                    iso: None,
                })
            }
            Fs::Ntfs(n) => {
                let i = n.info();
                let cluster = u64::from(i.cluster_size);
                let total = i.size / cluster;
                let free = i.free_clusters.unwrap_or(0);
                Ok(VolumeInfo {
                    fs_type: "NTFS".into(),
                    label: i.label,
                    volume_id: format!(
                        "{:04X}-{:04X}",
                        (i.serial >> 16) & 0xFFFF,
                        i.serial & 0xFFFF
                    ),
                    oem_name: "NTFS".into(),
                    bytes_per_sector: i.sector_size,
                    cluster_size: i.cluster_size,
                    total_clusters: total.min(u64::from(u32::MAX)) as u32,
                    free_clusters: free.min(u64::from(u32::MAX)) as u32,
                    total_bytes: total * cluster,
                    free_bytes: free * cluster,
                    volume_offset: offset,
                    volume_size: size,
                    layout: self.image.layout.clone(),
                    partition: self.image.partition.clone(),
                    media_format: None,
                    floppy_format: None,
                    writable: false,
                    fs_writable: false,
                    read_only_reason: Some("writing to NTFS is not supported yet".into()),
                    container,
                    iso: None,
                })
            }
            Fs::Fat(fs) => {
                let stats = fs.stats()?;
                let s = &self.boot;
                let bytes_per_sector = le16(s, 11);
                let total16 = le16(s, 19);
                let total_sectors = if total16 != 0 {
                    u32::from(total16)
                } else {
                    le32(s, 32)
                };
                let id = fs.volume_id();
                let cluster = u64::from(stats.cluster_size());
                Ok(VolumeInfo {
                    fs_type: match fs.fat_type() {
                        FatType::Fat12 => "FAT12",
                        FatType::Fat16 => "FAT16",
                        FatType::Fat32 => "FAT32",
                    }
                    .into(),
                    label: self.label()?,
                    volume_id: format!("{:04X}-{:04X}", id >> 16, id & 0xFFFF),
                    oem_name: String::from_utf8_lossy(&s[3..11]).trim_end().to_string(),
                    bytes_per_sector,
                    cluster_size: stats.cluster_size(),
                    total_clusters: stats.total_clusters(),
                    free_clusters: stats.free_clusters(),
                    total_bytes: u64::from(stats.total_clusters()) * cluster,
                    free_bytes: u64::from(stats.free_clusters()) * cluster,
                    volume_offset: offset,
                    volume_size: size,
                    layout: self.image.layout.clone(),
                    partition: self.image.partition.clone(),
                    media_format: None,
                    floppy_format: FloppyFormat::matching(
                        total_sectors,
                        le16(s, 24),
                        le16(s, 26),
                        s[21],
                        u32::from(s[13]) * u32::from(bytes_per_sector),
                        le16(s, 17),
                    )
                    .map(|f| f.name),
                    writable: self.image.writable,
                    fs_writable: true,
                    read_only_reason: None,
                    container,
                    iso: None,
                })
            }
        }
    }

    /// Volume label: the root-directory label entry if present, else the boot sector's.
    pub fn label(&self) -> Result<String> {
        match &self.fs {
            Fs::Iso(iso) => Ok(iso.info().volume_id.clone()),
            Fs::Ext(e) => Ok(e.info().label),
            Fs::Exfat(e) => Ok(e.info().label),
            Fs::Ntfs(n) => Ok(n.label()),
            Fs::Hfs(h) => Ok(h.label()),
            Fs::Fat(fs) => {
                let label = match fs.read_volume_label_from_root_dir()? {
                    Some(l) => l,
                    None => fs.volume_label(),
                };
                let label = label.trim_end().to_string();
                Ok(if label == "NO NAME" {
                    String::new()
                } else {
                    label
                })
            }
        }
    }

    /// Entries of one folder (not recursive), folders first, then by name.
    pub fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let fs = match &self.fs {
            Fs::Iso(iso) => return iso.list(dir),
            Fs::Ext(e) => return e.list(&norm(dir)?),
            Fs::Exfat(e) => return e.list(&norm(dir)?),
            Fs::Ntfs(n) => return n.list(&norm(dir)?),
            Fs::Hfs(h) => return h.list(&norm(dir)?),
            Fs::Fat(fs) => fs,
        };
        let dir = norm(dir)?;
        let mut out = Vec::new();
        for e in self.dir(fs, &dir)?.iter() {
            let e = e?;
            if is_dot(&e) {
                continue;
            }
            out.push(to_entry(&e, &dir));
        }
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    /// Every entry under a folder, depth-first.
    pub fn walk(&self, dir: &str) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for e in self.list(dir)? {
            let is_dir = e.is_dir;
            let path = e.path.clone();
            out.push(e);
            if is_dir {
                out.extend(self.walk(&path)?);
            }
        }
        Ok(out)
    }

    /// Look up one entry. The root has no entry and returns `None`.
    pub fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let fs = match &self.fs {
            Fs::Iso(iso) => return iso.stat(path),
            Fs::Ext(e) => return e.stat(&norm(path)?),
            Fs::Exfat(e) => return e.stat(&norm(path)?),
            Fs::Ntfs(n) => return n.stat(&norm(path)?),
            Fs::Hfs(h) => return h.stat(&norm(path)?),
            Fs::Fat(fs) => fs,
        };
        let path = norm(path)?;
        let (parent, _) = split_parent(&path);
        Ok(find(fs, &path)?.map(|e| to_entry(&e, parent)))
    }

    /// Copy a file or folder out of the image into `dest` (a host folder, created if missing).
    /// `src` of `""` extracts the whole root. Folders keep their structure.
    pub fn extract(&self, src: &str, dest: &Path, overwrite: bool) -> Result<Stats> {
        self.extract_with(src, dest, overwrite, ForkMode::Data)
    }

    /// [`Volume::extract`], choosing what happens to Macintosh resource forks and Finder
    /// information (HFS / HFS+ only; other file systems ignore `forks`).
    pub fn extract_with(
        &self,
        src: &str,
        dest: &Path,
        overwrite: bool,
        forks: ForkMode,
    ) -> Result<Stats> {
        let fs = match &self.fs {
            Fs::Iso(iso) => return iso.extract(src, dest, overwrite),
            Fs::Ext(e) => return e.extract(&norm(src)?, dest, overwrite),
            Fs::Exfat(e) => return e.extract(&norm(src)?, dest, overwrite),
            Fs::Ntfs(n) => return n.extract(&norm(src)?, dest, overwrite),
            Fs::Hfs(h) => return h.extract(&norm(src)?, dest, overwrite, forks),
            Fs::Fat(fs) => fs,
        };
        let src = norm(src)?;
        fs::create_dir_all(dest)?;
        let mut stats = Stats::default();
        if src.is_empty() {
            extract_dir(fs.root_dir(), dest, overwrite, &mut stats)?;
        } else {
            let e = find(fs, &src)?.ok_or_else(|| Error::NotFound(display(&src)))?;
            extract_entry(&e, dest, overwrite, &mut stats)?;
        }
        Ok(stats)
    }

    /// Write one file's contents to `out`. Returns its size.
    pub fn copy_out(&self, path: &str, out: &mut dyn io::Write) -> Result<u64> {
        let fs = match &self.fs {
            Fs::Iso(iso) => return iso.copy_out(path, out),
            Fs::Ext(e) => return e.copy_out(&norm(path)?, out),
            Fs::Exfat(e) => return e.copy_out(&norm(path)?, out),
            Fs::Ntfs(n) => return n.copy_out(&norm(path)?, out),
            Fs::Hfs(h) => return h.copy_out(&norm(path)?, out),
            Fs::Fat(fs) => fs,
        };
        let path = norm(path)?;
        let e = find(fs, &path)?.ok_or_else(|| Error::NotFound(display(&path)))?;
        if e.is_dir() {
            return Err(Error::Invalid(format!("{} is a folder", display(&path))));
        }
        Ok(io::copy(&mut e.to_file(), out)?)
    }

    /// Checksums of one file inside the image.
    pub fn checksum(&self, path: &str) -> Result<crate::Checksums> {
        let mut h = crate::checksum::Hasher::new();
        self.copy_out(path, &mut h)?;
        Ok(h.finish())
    }

    /// El Torito boot image of an ISO (1-based boot catalog entry).
    pub fn boot_image(&self, index: usize) -> Result<Vec<u8>> {
        match &self.fs {
            Fs::Iso(iso) => iso.boot_image(index),
            _ => Err(Error::Invalid(
                "boot images are part of ISO (El Torito) images; use the boot sector for FAT"
                    .into(),
            )),
        }
    }

    /// Copy a host file or folder into folder `dest_dir` of the image (created if missing).
    pub fn inject(&self, host: &Path, dest_dir: &str, overwrite: bool) -> Result<Stats> {
        self.inject_with(host, dest_dir, overwrite, ForkMode::Data)
    }

    /// [`Volume::inject`], choosing how Macintosh resource forks and Finder information
    /// arrive: from AppleDouble `._name` files next to the files, or from MacBinary files
    /// (HFS / HFS+ only; other file systems ignore `forks`).
    pub fn inject_with(
        &self,
        host: &Path,
        dest_dir: &str,
        overwrite: bool,
        forks: ForkMode,
    ) -> Result<Stats> {
        match &self.fs {
            Fs::Ext(e) => return e.inject(host, &norm(dest_dir)?, overwrite),
            Fs::Exfat(e) => return e.inject(host, &norm(dest_dir)?, overwrite),
            Fs::Hfs(h) => {
                return self
                    .hfs_rw(h)?
                    .inject(host, &norm(dest_dir)?, overwrite, forks);
            }
            _ => {}
        }
        let fs = self.fat_writable()?;
        let dest_dir = norm(dest_dir)?;
        let mut stats = Stats::default();
        inject_into(fs, host, &dest_dir, overwrite, &mut stats)?;
        Ok(stats)
    }

    /// Create a folder, including missing parents. Succeeds if it already exists.
    pub fn mkdir(&self, path: &str) -> Result<()> {
        let path = norm(path)?;
        if path.is_empty() {
            return Err(Error::InvalidPath("/".into()));
        }
        match &self.fs {
            Fs::Ext(e) => return e.mkdir(&path),
            Fs::Exfat(e) => return e.mkdir(&path),
            Fs::Hfs(h) => return self.hfs_rw(h)?.mkdir(&path),
            _ => {}
        }
        let fs = self.fat_writable()?;
        ensure_dir(fs, &path).map(|_| ())
    }

    /// Delete a file, or a folder (`recursive` needed when it is not empty).
    pub fn remove(&self, path: &str, recursive: bool) -> Result<Stats> {
        let path = norm(path)?;
        if path.is_empty() {
            return Err(Error::InvalidPath("cannot remove the root folder".into()));
        }
        match &self.fs {
            Fs::Ext(e) => return e.remove(&path, recursive),
            Fs::Exfat(e) => return e.remove(&path, recursive),
            Fs::Hfs(h) => return self.hfs_rw(h)?.remove(&path, recursive),
            _ => {}
        }
        self.fat_writable()?;
        let mut stats = Stats::default();
        self.remove_path(&path, recursive, &mut stats)?;
        Ok(stats)
    }

    fn remove_path(&self, path: &str, recursive: bool, stats: &mut Stats) -> Result<()> {
        let fs = self.fat_writable()?;
        let e = find(fs, path)?.ok_or_else(|| Error::NotFound(display(path)))?;
        let (parent, _) = split_parent(path);
        let name = e.file_name();
        if e.is_dir() {
            let children = self.list(path)?;
            if !children.is_empty() && !recursive {
                return Err(Error::Invalid(format!(
                    "folder is not empty: {}",
                    display(path)
                )));
            }
            for c in children {
                self.remove_path(&c.path, true, stats)?;
            }
            stats.dirs += 1;
        } else {
            stats.files += 1;
            stats.bytes += e.len();
        }
        self.dir(fs, parent)?
            .remove(&name)
            .map_err(|err| Error::at(path, err))
    }

    /// Rename or move. If `to` is an existing folder, `from` is moved into it.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from = norm(from)?;
        let to = norm(to)?;
        if matches!(self.fs, Fs::Ext(_) | Fs::Exfat(_) | Fs::Hfs(_)) && from.is_empty() {
            return Err(Error::InvalidPath("cannot move the root folder".into()));
        }
        match &self.fs {
            Fs::Ext(e) => return e.rename(&from, &to),
            Fs::Exfat(e) => return e.rename(&from, &to),
            Fs::Hfs(h) => return self.hfs_rw(h)?.rename(&from, &to),
            _ => {}
        }
        let fs = self.fat_writable()?;
        let src = find(fs, &from)?.ok_or_else(|| Error::NotFound(display(&from)))?;
        let (src_parent, _) = split_parent(&from);
        let src_name = src.file_name();
        let (dst_dir, dst_name) = match find(fs, &to)? {
            Some(t) if t.is_dir() => (to.clone(), src_name.clone()),
            Some(_) => return Err(Error::Exists(display(&to))),
            None if to.is_empty() => (String::new(), src_name.clone()),
            None => {
                let (p, n) = split_parent(&to);
                (p.to_string(), n.to_string())
            }
        };
        let target_dir = self.dir(fs, &dst_dir)?;
        self.dir(fs, src_parent)?
            .rename(&src_name, &target_dir, &dst_name)
            .map_err(|e| Error::at(&from, e))
    }

    /// Flush, unmount and persist the container, reporting any error (dropping a `Volume`
    /// does the same silently).
    pub fn close(self) -> Result<()> {
        let Volume {
            fs, _sync, image, ..
        } = self;
        match fs {
            Fs::Fat(fs) => fs.unmount()?,
            Fs::Ext(e) => e.flush()?,
            Fs::Exfat(e) => e.flush()?,
            _ => {}
        }
        image.flush()?;
        Ok(())
    }

    /// Rebuild a bare FAT volume (floppy or volume image) in another format or size — WinImage's
    /// "Change format". Files, folders, attributes, timestamps, label, serial number and boot
    /// code are kept. The image keeps its container (IMZ stays IMZ, VHDX stays VHDX…).
    pub fn change_format(path: &Path, opts: &OpenOptions, spec: NewImage) -> Result<Stats> {
        if let NewImage::Media(m) = spec {
            return Err(Error::Invalid(format!(
                "{} is a removable medium preset: create a new image with it, or give a size",
                m.name
            )));
        }
        let src = Volume::open(
            path,
            &OpenOptions {
                writable: false,
                ..opts.clone()
            },
        )?;
        let Fs::Fat(sfs) = &src.fs else {
            return Err(Error::Unsupported(
                "changing the format of ISO, ext and NTFS volumes (FAT only)".into(),
            ));
        };
        if !matches!(src.image.layout, Layout::BareVolume) {
            return Err(Error::Unsupported(
                "changing the format of partitioned disks (bare volumes and floppies only)".into(),
            ));
        }
        let like = src.image.container();
        let size = spec.size_bytes();
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new().prefix(".s0-").tempdir_in(dir)?;
        let scratch = tmp.path().join("scratch.img");
        let new_disk = container::share(Box::new(container::raw::RawDisk::create(&scratch, size)?));
        let (stats, _) =
            src.rebuild_fat_into(sfs, Window::new(new_disk.clone(), 0, size, true), &spec, 0)?;
        let dw = Window::new(new_disk.clone(), 0, size, true);
        drop(src);

        // Write the new volume into a fresh container of the same kind, then swap it in.
        let file_name = path
            .file_name()
            .ok_or_else(|| Error::InvalidPath(path.display().to_string()))?;
        let out = tmp.path().join(file_name);
        {
            let mut to = container::create_like(&out, &like, size, path)?;
            let mut from = lock(&new_disk)?;
            container::copy_sparse(from.as_mut(), to.as_mut(), size)?;
        }
        drop(dw);
        drop(new_disk);
        container::replace_file(&out, path)?;
        Ok(stats)
    }

    /// Format `dst` as FAT (`spec`, first sector `hidden` on its disk) and copy this volume's
    /// files, folders, attributes, times, label, serial number and boot code into it.
    /// Returns the copy's statistics and its FAT type (12, 16 or 32).
    fn rebuild_fat_into(
        &self,
        sfs: &FatFs,
        dst: Window,
        spec: &NewImage,
        hidden: u64,
    ) -> Result<(Stats, u8)> {
        let label = self.label()?;
        let label_bytes = if label.is_empty() {
            None
        } else {
            Some(encode_label(&label)?)
        };
        let mut dw = dst.reopen();
        format_fat(dst, spec, label_bytes, hidden, Some(sfs.volume_id()))?;
        if hidden != 0 {
            // fatfs has no hidden-sectors option: patch the BPB (and FAT32's backup).
            let mut boot = fatraw::read_boot(&mut dw)?;
            put_le32(&mut boot, 28, hidden.min(u64::from(u32::MAX)) as u32);
            dw.write_at(0, &boot)?;
            let b = fatraw::Bpb::parse(&boot)?;
            if b.fat_bits == 32 && b.backup_boot != 0 {
                dw.write_at(b.backup_boot * 512, &boot)?;
            }
        }
        let mut stats = Stats::default();
        {
            let dfs = FileSystem::new(dw.reopen(), FsOptions::new().update_accessed_date(false))
                .map_err(|e| Error::NotFat(e.to_string()))?;
            copy_dir(sfs.root_dir(), dfs.root_dir(), "", &mut stats).map_err(|e| match e {
                Error::Io(ref io) if io.to_string().contains("No space") => {
                    Error::Invalid("the files do not fit in the new format".into())
                }
                other => other,
            })?;
            dfs.unmount()?;
        }
        // Raw pass: attributes and timestamps fatfs cannot set, then boot code and label.
        let mut sw = self.image.window();
        fatraw::copy_metadata(&mut sw, &mut dw)?;
        let sboot = fatraw::read_boot(&mut sw)?;
        let (sb, db) = (
            fatraw::Bpb::parse(&sboot)?,
            fatraw::Bpb::parse(&fatraw::read_boot(&mut dw)?)?,
        );
        if sb.fat_bits == db.fat_bits && fatraw::boot_info(&sboot)?.has_boot_code {
            fatraw::import_boot(&mut dw, &sboot)?;
        }
        fatraw::set_label(&mut dw, label_bytes)?;
        Ok((stats, db.fat_bits))
    }

    /// Change the size of a partitioned (MBR or GPT) disk image. The container keeps its kind.
    /// With `grow_last`, the last partition takes up the new end of the disk (or gives up the
    /// space when shrinking); its FAT volume is rebuilt at the new size, keeping files,
    /// attributes, times, label, serial and boot code. Without it partitions keep their size
    /// and only the table moves (GPT's backup header). Other partitions are copied as they are.
    pub fn resize_disk(
        path: &Path,
        opts: &OpenOptions,
        new_size: u64,
        grow_last: bool,
    ) -> Result<DiskResize> {
        if !new_size.is_multiple_of(512) || new_size < 2 << 20 {
            return Err(Error::Invalid(
                "the new size must be a whole number of sectors, 2 MiB or more".into(),
            ));
        }
        let mut src_disk = container::open(path, false, opts.member.as_deref())?;
        let old_size = src_disk.size();
        let layout = partition::detect(src_disk.as_mut())?;
        let parts = layout.partitions().to_vec();
        if !matches!(layout, Layout::Mbr(_) | Layout::Gpt(_)) || parts.is_empty() {
            return Err(Error::Invalid(
                "not a partitioned disk (use the floppy / volume resize for bare volumes)".into(),
            ));
        }
        if src_disk.sector_size() != 512 {
            return Err(Error::Unsupported(
                "resizing disks with sectors other than 512 bytes".into(),
            ));
        }
        if matches!(layout, Layout::Mbr(_)) && new_size > (1u64 << 32) * 512 {
            return Err(Error::Invalid(
                "MBR disks are limited to 2 TiB; use GPT".into(),
            ));
        }
        let like = src_disk.info();
        let last = parts
            .iter()
            .filter(|p| !matches!(p.type_id.as_str(), "0x05" | "0x0F" | "0x85"))
            .max_by_key(|p| p.start_lba + p.sectors)
            .cloned()
            .expect("partitions");
        let outer_end = parts
            .iter()
            .map(|p| p.start_lba + p.sectors)
            .max()
            .unwrap_or(0);
        if grow_last && last.start_lba + last.sectors < outer_end {
            return Err(Error::Unsupported(
                "growing a logical partition inside an extended one".into(),
            ));
        }
        let total = new_size / 512;
        // Where the last partition may end on the new disk.
        let probe_limit = partition::last_usable(&layout, src_disk.as_mut(), total)?;
        let resize = if grow_last {
            if last.start_lba + 64 > probe_limit {
                return Err(Error::Invalid(format!(
                    "the new size leaves no room for partition {}",
                    last.index
                )));
            }
            let sectors = probe_limit + 1 - last.start_lba;
            if !last.is_fat {
                return Err(Error::Unsupported(format!(
                    "resizing the {} file system in partition {} (FAT only; keep the partition sizes to resize just the disk)",
                    last.fs.unwrap_or("unknown"),
                    last.index
                )));
            }
            Some((last.index, sectors))
        } else {
            None
        };

        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new().prefix(".s0-").tempdir_in(dir)?;
        let file_name = path
            .file_name()
            .ok_or_else(|| Error::InvalidPath(path.display().to_string()))?;
        let out = tmp.path().join(file_name);
        let new_disk = container::share(container::create_like(&out, &like, new_size, path)?);
        // Everything up to the rebuilt partition (or the whole disk) as it is; on GPT, not
        // the old backup header and entries.
        let keep = match (&resize, &layout) {
            (Some(_), _) => last.start_lba * 512,
            (None, Layout::Gpt(_)) => outer_end * 512,
            (None, _) => old_size,
        };
        {
            let mut to = lock(&new_disk)?;
            container::copy_sparse(src_disk.as_mut(), to.as_mut(), keep.min(new_size))?;
        }
        drop(src_disk);
        let mut report = DiskResize {
            old_size,
            new_size,
            partition: None,
            stats: None,
        };
        let mut mbr_type = None;
        if let Some((index, sectors)) = resize {
            let src = Volume::open(
                path,
                &OpenOptions {
                    writable: false,
                    partition: Some(index),
                    member: opts.member.clone(),
                },
            )?;
            let Fs::Fat(sfs) = &src.fs else {
                return Err(Error::Unsupported("resizing a non-FAT partition".into()));
            };
            let (stats, bits) = src.rebuild_fat_into(
                sfs,
                Window::new(new_disk.clone(), last.start_lba * 512, sectors * 512, true),
                &NewImage::Volume {
                    size_bytes: sectors * 512,
                },
                last.start_lba,
            )?;
            if matches!(layout, Layout::Mbr(_)) {
                mbr_type = Some(partition::mbr_type_for(bits, sectors));
            }
            report.partition = Some(PartitionResize {
                index,
                old_bytes: last.sectors * 512,
                new_bytes: sectors * 512,
            });
            report.stats = Some(stats);
        }
        {
            let mut d = lock(&new_disk)?;
            partition::fit_table(d.as_mut(), &layout, resize.map(|(i, n)| (i, n, mbr_type)))?;
            d.sync()?;
        }
        drop(new_disk);
        container::replace_file(&out, path)?;
        Ok(report)
    }

    // --- helpers -------------------------------------------------------------------------

    fn hfs_rw<'h>(&self, h: &'h HfsFs) -> Result<&'h HfsFs> {
        if self.image.writable {
            Ok(h)
        } else {
            Err(Error::Invalid("the image is open read-only".into()))
        }
    }

    fn fat_writable(&self) -> Result<&FatFs> {
        match &self.fs {
            Fs::Iso(_) => Err(Error::Unsupported(
                "modifying ISO 9660 images (they are read-only)".into(),
            )),
            Fs::Ntfs(_) => Err(Error::Unsupported(
                "writing to NTFS volumes (read-only so far)".into(),
            )),
            Fs::Hfs(_) => Err(Error::Unsupported("this operation on HFS volumes".into())),
            Fs::Ext(_) => Err(Error::Unsupported("this operation on ext volumes".into())),
            Fs::Exfat(_) => Err(Error::Unsupported("this operation on exFAT volumes".into())),
            Fs::Fat(fs) if self.image.writable => Ok(fs),
            Fs::Fat(_) => Err(Error::Invalid("the image is open read-only".into())),
        }
    }

    fn dir<'a>(&self, fs: &'a FatFs, path: &str) -> Result<FatDir<'a>> {
        if path.is_empty() {
            return Ok(fs.root_dir());
        }
        match find(fs, path)? {
            Some(e) if e.is_dir() => Ok(e.to_dir()),
            Some(_) => Err(Error::Invalid(format!("not a folder: {}", display(path)))),
            None => Err(Error::NotFound(display(path))),
        }
    }
}

fn find<'a>(fs: &'a FatFs, path: &str) -> Result<Option<FatEntry<'a>>> {
    if path.is_empty() {
        return Ok(None);
    }
    let mut dir = fs.root_dir();
    let mut parts = path.split('/').peekable();
    while let Some(name) = parts.next() {
        let Some(e) = find_in(&dir, name)? else {
            return Ok(None);
        };
        if parts.peek().is_none() {
            return Ok(Some(e));
        }
        if !e.is_dir() {
            return Ok(None);
        }
        dir = e.to_dir();
    }
    Ok(None)
}

fn ensure_dir<'a>(fs: &'a FatFs, path: &str) -> Result<FatDir<'a>> {
    let mut dir = fs.root_dir();
    let mut so_far = String::new();
    for name in path.split('/').filter(|s| !s.is_empty()) {
        so_far = join(&so_far, name);
        dir = match find_in(&dir, name)? {
            Some(e) if e.is_dir() => e.to_dir(),
            Some(_) => {
                return Err(Error::Exists(format!(
                    "{} (a file is in the way)",
                    display(&so_far)
                )));
            }
            None => dir.create_dir(name).map_err(|e| Error::at(&so_far, e))?,
        };
    }
    Ok(dir)
}

fn extract_entry(e: &FatEntry<'_>, dest: &Path, overwrite: bool, stats: &mut Stats) -> Result<()> {
    let target = dest.join(host_safe_name(&e.file_name()));
    if e.is_dir() {
        fs::create_dir_all(&target)?;
        stats.dirs += 1;
        extract_dir(e.to_dir(), &target, overwrite, stats)
    } else {
        if !overwrite && target.exists() {
            return Err(Error::Exists(target.display().to_string()));
        }
        let mut src = e.to_file();
        let mut out = File::create(&target)?;
        stats.bytes += io::copy(&mut src, &mut out)?;
        stats.files += 1;
        if let Some(t) = to_system_time(e.modified()) {
            let _ = out.set_modified(t);
        }
        Ok(())
    }
}

fn extract_dir(dir: FatDir<'_>, dest: &Path, overwrite: bool, stats: &mut Stats) -> Result<()> {
    for e in dir.iter() {
        let e = e?;
        if !is_dot(&e) {
            extract_entry(&e, dest, overwrite, stats)?;
        }
    }
    Ok(())
}

fn no_space(e: &io::Error) -> bool {
    e.to_string().contains("No space")
}

fn inject_into(
    fs: &FatFs,
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
    if meta.is_dir() {
        let img_path = join(dest_dir, &name);
        ensure_dir(fs, &img_path)?;
        stats.dirs += 1;
        let mut children: Vec<_> = fs::read_dir(host)?.collect::<io::Result<_>>()?;
        children.sort_by_key(|c| c.file_name());
        for c in children {
            inject_into(fs, &c.path(), &img_path, overwrite, stats)?;
        }
        return Ok(());
    }

    let dir = ensure_dir(fs, dest_dir)?;
    let img_path = join(dest_dir, &name);
    if let Some(existing) = find_in(&dir, &name)? {
        if existing.is_dir() {
            return Err(Error::Exists(format!("{} (a folder)", display(&img_path))));
        }
        if !overwrite {
            return Err(Error::Exists(display(&img_path)));
        }
    }
    let mut src = File::open(host)?;
    let mut dst = dir
        .create_file(&name)
        .map_err(|e| Error::at(&img_path, e))?;
    let copied = dst.truncate().and_then(|_| io::copy(&mut src, &mut dst));
    match copied {
        Ok(n) => {
            // Keep the host file's modified time, as WinImage does. fatfs 0.3 deprecates this
            // in favour of a TimeProvider, but it is correct when set after the last write.
            if let Some(dt) = meta.modified().ok().and_then(to_fat_time) {
                #[allow(deprecated)]
                dst.set_modified(dt);
            }
            io::Write::flush(&mut dst)?;
            stats.files += 1;
            stats.bytes += n;
            Ok(())
        }
        Err(e) => {
            drop(dst);
            let _ = dir.remove(&name);
            if no_space(&e) {
                return Err(Error::Invalid(format!(
                    "not enough free space in the image for {}",
                    display(&img_path)
                )));
            }
            Err(Error::Io(e))
        }
    }
}

/// Copy a FAT folder tree between two mounted volumes (for change-format).
fn copy_dir(src: FatDir<'_>, dst: FatDir<'_>, path: &str, stats: &mut Stats) -> Result<()> {
    for e in src.iter() {
        let e = e?;
        if is_dot(&e) {
            continue;
        }
        let name = e.file_name();
        let p = join(path, &name);
        if e.is_dir() {
            let d = dst.create_dir(&name).map_err(|err| Error::at(&p, err))?;
            stats.dirs += 1;
            copy_dir(e.to_dir(), d, &p, stats)?;
        } else {
            let mut out = dst.create_file(&name).map_err(|err| Error::at(&p, err))?;
            out.truncate()?;
            stats.bytes += io::copy(&mut e.to_file(), &mut out)?;
            #[allow(deprecated)]
            out.set_modified(e.modified());
            io::Write::flush(&mut out)?;
            stats.files += 1;
        }
    }
    Ok(())
}

fn find_in<'a>(dir: &FatDir<'a>, name: &str) -> Result<Option<FatEntry<'a>>> {
    let want = name.to_lowercase();
    for e in dir.iter() {
        let e = e?;
        if is_dot(&e) {
            continue;
        }
        if e.file_name().to_lowercase() == want || e.short_file_name().to_lowercase() == want {
            return Ok(Some(e));
        }
    }
    Ok(None)
}

fn is_dot(e: &FatEntry<'_>) -> bool {
    let n = e.short_file_name();
    n == "." || n == ".."
}

fn to_entry(e: &FatEntry<'_>, parent: &str) -> Entry {
    let a = e.attributes();
    let name = e.file_name();
    Entry {
        path: join(parent, &name),
        short_name: e.short_file_name(),
        is_dir: e.is_dir(),
        size: if e.is_dir() { 0 } else { e.len() },
        modified: format_time(e.modified()),
        created: format_time(e.created()),
        accessed: format_date(e.accessed()),
        attributes: Attributes {
            read_only: a.contains(FileAttributes::READ_ONLY),
            hidden: a.contains(FileAttributes::HIDDEN),
            system: a.contains(FileAttributes::SYSTEM),
            archive: a.contains(FileAttributes::ARCHIVE),
        },
        mac: None,
        name,
    }
}

/// Normalise an in-image path: `\` → `/`, drop empty and `.` parts, refuse `..`.
pub(crate) fn norm(p: &str) -> Result<String> {
    let mut parts = Vec::new();
    for c in p.split(['/', '\\']) {
        match c {
            "" | "." => {}
            ".." => return Err(Error::InvalidPath(p.to_string())),
            c => parts.push(c),
        }
    }
    Ok(parts.join("/"))
}

pub(crate) fn split_parent(p: &str) -> (&str, &str) {
    match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// A name read from an image must not escape the destination folder on the host.
pub(crate) fn host_safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            let bad_here = cfg!(windows) && matches!(c, '<' | '>' | '"' | '|' | '?' | '*');
            if matches!(c, '/' | '\\' | ':' | '\0') || c.is_control() || bad_here {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Windows drops trailing dots and spaces from names; keep them distinct.
    let cleaned = match cleaned.trim_end_matches(['.', ' ']) {
        t if cfg!(windows) && t.len() != cleaned.len() && !t.is_empty() => format!("{t}_"),
        _ => cleaned,
    };
    match cleaned.trim() {
        "" | "." | ".." => "_".to_string(),
        _ => cleaned,
    }
}

pub(crate) fn encode_label(label: &str) -> Result<[u8; 11]> {
    let up = label.trim().to_ascii_uppercase();
    if up.len() > 11 {
        return Err(Error::Invalid(
            "volume label is longer than 11 characters".into(),
        ));
    }
    let ok = |c: char| c.is_ascii_alphanumeric() || c == ' ' || "!#$%&'()-@^_`{}~".contains(c);
    if let Some(bad) = up.chars().find(|&c| !ok(c)) {
        return Err(Error::Invalid(format!(
            "character {bad:?} is not allowed in a volume label"
        )));
    }
    let mut out = [b' '; 11];
    out[..up.len()].copy_from_slice(up.as_bytes());
    Ok(out)
}

pub(crate) fn new_volume_id() -> u32 {
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() as u32).rotate_left(16) ^ d.subsec_nanos()
}

fn format_date(d: fatfs::Date) -> String {
    if d.month == 0 || d.day == 0 {
        return String::new();
    }
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

fn format_time(t: fatfs::DateTime) -> String {
    if t.date.month == 0 || t.date.day == 0 {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.date.year, t.date.month, t.date.day, t.time.hour, t.time.min, t.time.sec
    )
}

fn to_system_time(t: fatfs::DateTime) -> Option<SystemTime> {
    Local
        .with_ymd_and_hms(
            i32::from(t.date.year),
            u32::from(t.date.month),
            u32::from(t.date.day),
            u32::from(t.time.hour),
            u32::from(t.time.min),
            u32::from(t.time.sec),
        )
        .single()
        .map(SystemTime::from)
}

/// FAT can only store 1980–2107; anything outside is left at fatfs's default (now).
fn to_fat_time(t: SystemTime) -> Option<fatfs::DateTime> {
    let local = chrono::DateTime::<Local>::from(t);
    (1980..=2107)
        .contains(&local.year())
        .then(|| fatfs::DateTime::from(local))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_normalise() {
        assert_eq!(norm("/A\\b//c/").unwrap(), "A/b/c");
        assert_eq!(norm("").unwrap(), "");
        assert!(norm("a/../b").is_err());
    }

    #[test]
    fn host_names_cannot_escape() {
        assert_eq!(host_safe_name(".."), "_");
        assert_eq!(host_safe_name("a/b"), "a_b");
        assert_eq!(host_safe_name("C:x"), "C_x");
    }

    #[test]
    fn labels() {
        assert_eq!(&encode_label("boot disk").unwrap(), b"BOOT DISK  ");
        assert!(encode_label("twelve chars").is_err());
        assert!(encode_label("a.b").is_err());
    }
}
