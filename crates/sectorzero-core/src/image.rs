//! An opened image file: its container, its layout, and the byte range of the selected
//! volume. Also the raw operations that work on an unmounted volume (label, boot sector,
//! MBR boot code, defragmentation) and container conversion.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::container::{self, ContainerInfo, ContainerKind, SharedDisk};
use crate::error::{Error, Result};
use crate::fatraw::{self, BootSectorEdit, BootSectorInfo, DefragReport};
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
            Layout::Mbr(parts) | Layout::Gpt(parts) => {
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
                    "no FAT boot sector, ISO 9660 descriptor, or MBR/GPT partition table found"
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
                let mut s = [0u8; 512];
                lock(&self.disk)?.read_at(0, &mut s)?;
                partition::looks_like_fat_boot_sector(&s).then_some("FAT")
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

    /// Set or (with `None` / empty) remove the FAT volume label.
    pub fn set_label(&self, label: Option<&str>) -> Result<()> {
        self.require_writable()?;
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
pub fn convert(
    src: &Path,
    member: Option<&str>,
    dst: &Path,
    kind: Option<ContainerKind>,
    dynamic: bool,
    overwrite: bool,
) -> Result<ConvertReport> {
    if same_file(src, dst) {
        return Err(Error::Invalid(
            "source and destination are the same file".into(),
        ));
    }
    let mut from = container::open(src, false, member)?;
    let mut size = from.size();
    // A truncated WinImage image still converts at its full size.
    let mut s0 = [0u8; 512];
    from.read_at(0, &mut s0)?;
    if partition::looks_like_fat_boot_sector(&s0)
        && let Ok(b) = fatraw::Bpb::parse(&s0)
    {
        size = size.max(b.total_sectors * b.bps);
    }
    let size = round_up(size, 512);
    let kind = kind.unwrap_or_else(|| ContainerKind::from_path(dst));
    let mut to = container::create(dst, kind, size, dynamic, overwrite)?;
    let result = container::copy_sparse(from.as_mut(), to.as_mut(), size);
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

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}
