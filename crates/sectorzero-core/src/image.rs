//! An opened image file: its container, its layout, and the byte range of the selected
//! volume. Also the raw operations that work on an unmounted volume (label, boot sector,
//! MBR boot code, defragmentation) and container conversion.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::checksum::{Checksums, Hasher};
use crate::container::{self, ContainerInfo, ContainerKind, SharedDisk};
use crate::error::{Error, Result};
use crate::fatraw::{self, BootSectorEdit, BootSectorInfo, DefragReport, EntryEdit};
use crate::partition::{self, Layout, PartitionInfo};
use crate::util::*;
use crate::window::{Window, lock};

#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Open for modification. Without it every write is refused before touching the file.
    pub writable: bool,
    /// 1-based partition to open. Default: the first FAT partition (or the bare volume).
    pub partition: Option<usize>,
    /// For WLZ archives: which image (1-based number or member name). Default: the first.
    pub member: Option<String>,
    /// For APFS containers: which volume (1-based number or name). Default: the first.
    pub volume: Option<String>,
}

pub struct Image {
    pub(crate) disk: SharedDisk,
    pub(crate) layout: Layout,
    pub(crate) writable: bool,
    pub(crate) path: PathBuf,
    /// Selected volume: byte offset, length and partition.
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) partition: Option<PartitionInfo>,
}

impl Image {
    pub fn open(path: &Path, opts: &OpenOptions) -> Result<Image> {
        let mut disk = container::open(path, opts.writable, opts.member.as_deref())?;
        if disk.size() < 512 {
            return Err(Error::NotFat("the disk is smaller than one sector".into()));
        }
        let layout = partition::detect(disk.as_mut())?;
        let disk_size = disk.size();
        let (offset, len, part) = match &layout {
            Layout::BareVolume | Layout::Iso => {
                if let Some(n) = opts.partition.filter(|&n| n != 1) {
                    return Err(Error::Invalid(format!(
                        "image has no partition table, so there is no partition {n}"
                    )));
                }
                let mut len = disk_size;
                if matches!(layout, Layout::BareVolume) {
                    // WinImage can save images with unused space truncated: use the BPB size.
                    let mut s = [0u8; 512];
                    disk.read_at(0, &mut s)?;
                    if let Ok(b) = fatraw::Bpb::parse(&s) {
                        len = len.max(b.total_sectors * b.bps);
                    }
                }
                (0, len, None)
            }
            Layout::Mbr(parts) | Layout::Gpt(parts) | Layout::Apm(parts) => {
                let part = match opts.partition {
                    Some(n) => parts
                        .iter()
                        .find(|p| p.index == n)
                        .ok_or_else(|| Error::NotFound(format!("partition {n}")))?,
                    None => parts
                        .iter()
                        .find(|p| p.is_fat)
                        .or_else(|| parts.iter().find(|p| p.fs.is_some()))
                        .or_else(|| parts.first())
                        .ok_or_else(|| Error::NotFat("the partition table is empty".into()))?,
                };
                (part.offset(), part.size_bytes(), Some(part.clone()))
            }
            Layout::Unknown => {
                return Err(Error::NotFat(
                    "no FAT boot sector, ISO 9660 descriptor, or MBR/GPT/Apple partition table found"
                        .into(),
                ));
            }
        };
        Ok(Image {
            disk: container::share(disk),
            layout,
            writable: opts.writable,
            path: path.to_path_buf(),
            offset,
            len,
            partition: part,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn container(&self) -> ContainerInfo {
        self.disk
            .lock()
            .map(|d| d.info())
            .unwrap_or_else(|_| ContainerInfo::simple(ContainerKind::Raw, "?", 0))
    }

    pub fn partition(&self) -> Option<&PartitionInfo> {
        self.partition.as_ref()
    }

    pub(crate) fn window(&self) -> Window {
        Window::new(self.disk.clone(), self.offset, self.len, self.writable)
    }

    fn require_writable(&self) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(Error::Invalid("the image is open read-only".into()))
        }
    }

    fn require_fat(&self) -> Result<()> {
        let fs = match (&self.layout, &self.partition) {
            (Layout::BareVolume, _) => {
                let mut s = vec![0u8; 2048];
                self.window().read_at(0, &mut s)?;
                partition::fs_of(&s)
            }
            (Layout::Iso, _) => Some("ISO 9660"),
            (_, Some(p)) => p.fs,
            _ => None,
        };
        match fs {
            Some("FAT") => Ok(()),
            Some(other) => Err(Error::Unsupported(format!(
                "this operation on {other} volumes (FAT only)"
            ))),
            None => Err(Error::NotFat(
                "the selected volume has no FAT file system".into(),
            )),
        }
    }

    /// Persist container metadata (VHD/VHDX/VMDK tables, IMZ re-pack).
    pub fn flush(&self) -> Result<()> {
        lock(&self.disk)?.sync()?;
        Ok(())
    }

    // --- volume label and boot sector -------------------------------------------------

    /// The file system of the selected volume ("FAT", "exFAT", "ext4", …).
    fn fs_name(&self) -> Result<Option<&'static str>> {
        if matches!(self.layout, Layout::Iso) {
            return Ok(Some("ISO 9660"));
        }
        let mut head = vec![0u8; 2048];
        self.window().read_at(0, &mut head)?;
        Ok(partition::fs_of(&head))
    }

    /// Whether the selected volume holds ext2/3/4.
    fn is_ext(&self) -> Result<bool> {
        Ok(self.fs_name()?.is_some_and(|f| f.starts_with("ext")))
    }

    fn is_exfat(&self) -> Result<bool> {
        Ok(self.fs_name()? == Some("exFAT"))
    }

    fn is_hfs(&self) -> Result<bool> {
        Ok(matches!(self.fs_name()?, Some("HFS" | "HFS+" | "HFSX")))
    }

    /// Store an IMZ/WLZ image encrypted with `password` (WinImage's password protection),
    /// or unencrypted with `None`. The archive is rewritten at once.
    pub fn set_password(&self, password: Option<&str>) -> Result<()> {
        self.require_writable()?;
        let mut disk = lock(&self.disk)?;
        disk.set_password(password)?;
        disk.sync()?;
        Ok(())
    }

    /// Set or (with `None` / empty) remove the volume label (FAT, exFAT, ext; HFS volumes
    /// always have a name).
    pub fn set_label(&self, label: Option<&str>) -> Result<()> {
        self.require_writable()?;
        if self.is_ext()? {
            crate::ext::set_label(&mut self.window(), label)?;
            return self.flush();
        }
        if self.is_exfat()? {
            crate::exfat::ExFatFs::open(self.window(), true)?.set_label(label)?;
            return self.flush();
        }
        if self.is_hfs()? {
            crate::hfs::HfsFs::open(self.window())?.set_label(label)?;
            return self.flush();
        }
        self.require_fat()?;
        let label = match label.map(str::trim).filter(|l| !l.is_empty()) {
            Some(l) => Some(crate::volume::encode_label(l)?),
            None => None,
        };
        fatraw::set_label(&mut self.window(), label)?;
        self.flush()
    }

    pub fn boot_sector(&self) -> Result<Vec<u8>> {
        fatraw::read_boot(&mut self.window())
    }

    pub fn boot_sector_info(&self) -> Result<BootSectorInfo> {
        self.require_fat()?;
        fatraw::boot_info(&self.boot_sector()?)
    }

    pub fn edit_boot_sector(&self, edit: &BootSectorEdit) -> Result<()> {
        self.require_writable()?;
        self.require_fat()?;
        fatraw::edit_boot(&mut self.window(), edit)?;
        self.flush()
    }

    /// Replace the boot code, keeping the BPB (see `fatraw::import_boot`).
    pub fn import_boot_sector(&self, data: &[u8]) -> Result<()> {
        self.require_writable()?;
        self.require_fat()?;
        fatraw::import_boot(&mut self.window(), data)?;
        self.flush()
    }

    /// Sector 0 of the whole disk (the MBR on a partitioned image).
    pub fn mbr(&self) -> Result<Vec<u8>> {
        let mut s = vec![0u8; 512];
        lock(&self.disk)?.read_at(0, &mut s)?;
        Ok(s)
    }

    /// Replace the MBR boot code (bytes 0–439), keeping the disk signature and partition table.
    pub fn import_mbr_code(&self, data: &[u8]) -> Result<()> {
        self.require_writable()?;
        if !matches!(self.layout, Layout::Mbr(_) | Layout::Gpt(_)) {
            return Err(Error::Invalid(
                "this image has no MBR (it is a bare volume)".into(),
            ));
        }
        if data.len() < 440 {
            return Err(Error::Invalid(
                "MBR boot code must be at least 440 bytes".into(),
            ));
        }
        let mut s = self.mbr()?;
        s[..440].copy_from_slice(&data[..440]);
        lock(&self.disk)?.write_at(0, &s)?;
        self.flush()
    }

    // --- file properties ---------------------------------------------------------------

    /// Change attributes and times (see [`EntryEdit`]) of a file or folder (with
    /// `recursive`, of everything below it too). Returns how many entries changed. FAT,
    /// exFAT and ext: on ext, read-only is the absence of write permission bits, and
    /// hidden / system / archive do not exist.
    pub fn edit_entry(&self, path: &str, edit: &EntryEdit, recursive: bool) -> Result<u64> {
        self.require_writable()?;
        let path = crate::volume::norm(path)?;
        if self.is_ext()? {
            let fs = crate::ext::ExtFs::open(self.window(), true)?;
            let n = fs.edit(&path, edit, recursive)?;
            drop(fs);
            self.flush()?;
            return Ok(n);
        }
        if self.is_exfat()? {
            let fs = crate::exfat::ExFatFs::open(self.window(), true)?;
            let n = fs.edit(&path, edit, recursive)?;
            drop(fs);
            self.flush()?;
            return Ok(n);
        }
        if self.is_hfs()? {
            let n = crate::hfs::HfsFs::open(self.window())?.edit(&path, edit, recursive)?;
            self.flush()?;
            return Ok(n);
        }
        self.require_fat()?;
        let n = fatraw::edit_entries(&mut self.window(), &path, edit, recursive)?;
        self.flush()?;
        Ok(n)
    }

    // --- defragmentation --------------------------------------------------------------

    pub fn analyze_fragmentation(&self) -> Result<DefragReport> {
        self.require_fat()?;
        fatraw::analyze(&mut self.window())
    }

    pub fn defragment(&self, zero_free: bool) -> Result<DefragReport> {
        self.require_writable()?;
        self.require_fat()?;
        let r = fatraw::defrag(&mut self.window(), zero_free)?;
        self.flush()?;
        Ok(r)
    }

    // --- checksums and ISO edits -------------------------------------------------------

    /// Checksums of the virtual disk's contents. They do not depend on the container, so an
    /// IMA, its IMZ and its VHDX copy all give the same values. A truncated floppy image
    /// is hashed at its full size.
    pub fn checksum_disk(&self) -> Result<Checksums> {
        let mut d = lock(&self.disk)?;
        let mut size = d.size();
        if matches!(self.layout, Layout::BareVolume) {
            size = size.max(self.len);
        }
        let mut h = Hasher::new();
        let mut buf = vec![0u8; 1024 * 1024];
        let mut at = 0;
        while at < size {
            let n = (size - at).min(buf.len() as u64) as usize;
            d.read_at(at, &mut buf[..n])?;
            h.update(&buf[..n]);
            at += n as u64;
        }
        Ok(h.finish())
    }

    /// Replace the contents of a file inside an ISO 9660 image, in place. The new data must
    /// fit in the sectors the old file occupies (WinImage only allows the same size).
    pub fn iso_replace_file(&self, path: &str, data: &[u8]) -> Result<()> {
        self.require_writable()?;
        if !matches!(self.layout, Layout::Iso) {
            return Err(Error::Invalid("this is not an ISO 9660 image".into()));
        }
        crate::iso::replace_file(self.window(), &crate::volume::norm(path)?, data)?;
        self.flush()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TruncateReport {
    pub old_size: u64,
    pub new_size: u64,
}

/// Cut the unused space off the end of a raw floppy or volume image (WinImage's
/// "truncate unused space"). SectorZero and WinImage read such images as zero-padded.
pub fn truncate_image(path: &Path) -> Result<TruncateReport> {
    let img = Image::open(path, &OpenOptions::default())?;
    if img.container().kind != ContainerKind::Raw || img.container().compression.is_some() {
        return Err(Error::Unsupported(
            "truncating this container (raw images only; IMZ/VHD/VMDK store free space compactly already)"
                .into(),
        ));
    }
    if !matches!(img.layout, Layout::BareVolume) {
        return Err(Error::Unsupported(
            "truncating partitioned or ISO images (floppy and bare volume images only)".into(),
        ));
    }
    img.require_fat()?;
    let end = round_up(fatraw::used_end(&mut img.window())?, 512);
    drop(img);
    let old_size = std::fs::metadata(path)?.len();
    if end < old_size {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .set_len(end)?;
    }
    Ok(TruncateReport {
        old_size,
        new_size: end.min(old_size),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct ConvertReport {
    pub container: ContainerInfo,
    pub bytes: u64,
    /// Bytes actually stored (non-zero data); the rest was left sparse.
    pub stored: u64,
}

/// Copy a virtual disk into a new container (e.g. IMA → VHDX, VMDK → IMZ), keeping it
/// sparse: unallocated and all-zero ranges are not written.
///
/// The source can be a physical drive (`\\.\PhysicalDrive1`, `/dev/sdb`): that reads the
/// drive into an image.
pub fn convert(
    src: &Path,
    member: Option<&str>,
    dst: &Path,
    kind: Option<ContainerKind>,
    dynamic: bool,
    overwrite: bool,
) -> Result<ConvertReport> {
    convert_with_progress(src, member, dst, kind, dynamic, overwrite, &mut |_, _| {})
}

/// [`convert`], reporting (bytes done, total) as it copies.
pub fn convert_with_progress(
    src: &Path,
    member: Option<&str>,
    dst: &Path,
    kind: Option<ContainerKind>,
    dynamic: bool,
    overwrite: bool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<ConvertReport> {
    if same_file(src, dst) {
        return Err(Error::Invalid(
            "source and destination are the same file".into(),
        ));
    }
    // Reading a drive: keep Windows from changing it mid-copy where its volumes can be
    // locked (not the system disk, nor a volume with open files: those are read as they are).
    let _locks = if crate::device::is_device_path(src) {
        crate::device::lock_for_reading(src).unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut from = container::open(src, false, member)?;
    let size = disk_size_of(from.as_mut())?;
    let kind = kind.unwrap_or_else(|| ContainerKind::from_path(dst));
    let mut to = container::create(dst, kind, size, dynamic, overwrite)?;
    let result = container::copy_sparse_progress(from.as_mut(), to.as_mut(), size, progress);
    match result {
        Ok(stored) => Ok(ConvertReport {
            container: to.info(),
            bytes: size,
            stored,
        }),
        Err(e) => {
            drop(to);
            let _ = std::fs::remove_file(dst);
            Err(e.into())
        }
    }
}

/// Checksums of the virtual disk inside an image (or of a drive) without looking for a file
/// system, so a blank or wiped disk can be hashed too. The same values as
/// [`Image::checksum_disk`] gives for a disk it can open.
pub fn checksum_virtual_disk(src: &Path, member: Option<&str>) -> Result<Checksums> {
    let mut from = container::open(src, false, member)?;
    let size = disk_size_of(from.as_mut())?;
    let mut h = Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut at = 0;
    while at < size {
        let n = (size - at).min(buf.len() as u64) as usize;
        from.read_at(at, &mut buf[..n])?;
        h.update(&buf[..n]);
        at += n as u64;
    }
    Ok(h.finish())
}

/// The full size of the disk an image holds, in whole sectors: a truncated floppy or volume
/// image still counts at the size its boot sector gives.
pub(crate) fn disk_size_of(from: &mut dyn container::Disk) -> Result<u64> {
    let mut size = from.size();
    let mut s0 = [0u8; 512];
    from.read_at(0, &mut s0)?;
    if partition::looks_like_fat_boot_sector(&s0)
        && let Ok(b) = fatraw::Bpb::parse(&s0)
    {
        size = size.max(b.total_sectors * b.bps);
    }
    Ok(round_up(size, 512))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}
