//! Image containers: the file formats that hold a virtual disk's sectors.
//!
//! Every container exposes the same thing: a virtual disk of `size()` bytes with positional
//! reads and writes. Partition tables and file systems are layered on top (see `partition`,
//! `window` and `volume`).

pub(crate) mod raw;
mod vhd;
mod vhdx;
mod vmdk;
mod zip;

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::error::{Error, Result};

/// A virtual disk inside an image file.
pub(crate) trait Disk: Send {
    /// Virtual disk size in bytes.
    fn size(&self) -> u64;
    /// Logical sector size the disk reports (512 unless the container says otherwise).
    fn sector_size(&self) -> u32 {
        512
    }
    /// Fill `buf` from `offset`. Ranges past the end of the virtual disk read as zeros.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    /// Persist container metadata (allocation tables, footers, ZIP re-pack).
    fn sync(&mut self) -> io::Result<()>;
    fn info(&self) -> ContainerInfo;
    /// `false` when the range is known to read as zeros without storing anything
    /// (an unallocated block in a sparse format). Used to keep conversions sparse.
    fn is_allocated(&mut self, _offset: u64, _len: u64) -> io::Result<bool> {
        Ok(true)
    }
}

pub(crate) type SharedDisk = Arc<Mutex<Box<dyn Disk>>>;

pub(crate) fn share(d: Box<dyn Disk>) -> SharedDisk {
    Arc::new(Mutex::new(d))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerKind {
    /// Plain sector image: .ima .img .vfd .flp .dsk .iso …
    Raw,
    /// WinImage compressed image: a ZIP holding one raw image.
    Imz,
    /// WinImage compressed list: a ZIP holding a .wil list and several raw images.
    Wlz,
    Vhd,
    Vhdx,
    Vmdk,
}

impl ContainerKind {
    /// Container implied by a file extension (for new files).
    pub fn from_path(path: &Path) -> ContainerKind {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "imz" => ContainerKind::Imz,
            "wlz" => ContainerKind::Wlz,
            "vhd" => ContainerKind::Vhd,
            "vhdx" => ContainerKind::Vhdx,
            "vmdk" => ContainerKind::Vmdk,
            _ => ContainerKind::Raw,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ContainerKind::Raw => "Raw image",
            ContainerKind::Imz => "WinImage compressed image (IMZ)",
            ContainerKind::Wlz => "WinImage compressed list (WLZ)",
            ContainerKind::Vhd => "Virtual Hard Disk (VHD)",
            ContainerKind::Vhdx => "Virtual Hard Disk v2 (VHDX)",
            ContainerKind::Vmdk => "VMware disk (VMDK)",
        }
    }

    /// Whether `dynamic: true` means anything for this container.
    pub fn has_dynamic_variant(self) -> bool {
        matches!(
            self,
            ContainerKind::Vhd | ContainerKind::Vhdx | ContainerKind::Vmdk
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerInfo {
    pub kind: ContainerKind,
    /// e.g. "fixed", "dynamic", "monolithicSparse", "monolithicFlat".
    pub variant: String,
    pub virtual_size: u64,
    /// For ZIP containers: the image members and which one is open.
    pub members: Vec<String>,
    pub member: Option<String>,
    /// IMZ/WLZ archive comment (WinImage's image comment).
    pub comment: Option<String>,
}

impl ContainerInfo {
    pub(crate) fn simple(kind: ContainerKind, variant: &str, virtual_size: u64) -> Self {
        ContainerInfo {
            kind,
            variant: variant.into(),
            virtual_size,
            members: vec![],
            member: None,
            comment: None,
        }
    }
}

fn read_up_to(file: &mut File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    file.seek(SeekFrom::Start(offset))?;
    let mut got = 0;
    while got < buf.len() {
        match file.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Detect the container by content (not extension) and open it.
pub(crate) fn open(path: &Path, writable: bool, member: Option<&str>) -> Result<Box<dyn Disk>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut head = [0u8; 4096];
    let got = read_up_to(&mut file, 0, &mut head)?;
    let head = &head[..got];
    let mut tail = [0u8; 512];
    if len >= 512 {
        read_up_to(&mut file, len - 512, &mut tail)?;
    }
    drop(file);

    if member.is_some() && !head.starts_with(b"PK\x03\x04") {
        return Err(Error::Invalid(
            "--member only applies to IMZ/WLZ archives".into(),
        ));
    }
    if head.starts_with(b"PK\x03\x04") {
        return Ok(Box::new(zip::ZipDisk::open(path, writable, member)?));
    }
    if head.starts_with(b"vhdxfile") {
        return Ok(Box::new(vhdx::VhdxDisk::open(path, writable)?));
    }
    if head.starts_with(b"KDMV") || vmdk::looks_like_descriptor(head) {
        return Ok(Box::new(vmdk::VmdkDisk::open(path, writable)?));
    }
    if head.starts_with(b"COWD") {
        return Err(Error::Unsupported("VMware ESX COWD sparse disks".into()));
    }
    if tail.starts_with(b"conectix") || head.starts_with(b"conectix") {
        return Ok(Box::new(vhd::VhdDisk::open(path, writable)?));
    }
    Ok(Box::new(raw::RawDisk::open(path, writable)?))
}

/// Create a new, zero-filled container of `size` bytes.
pub(crate) fn create(
    path: &Path,
    kind: ContainerKind,
    size: u64,
    dynamic: bool,
    overwrite: bool,
) -> Result<Box<dyn Disk>> {
    if !overwrite && path.exists() {
        return Err(Error::Exists(path.display().to_string()));
    }
    if size == 0 || !size.is_multiple_of(512) {
        return Err(Error::Invalid(
            "disk size must be a non-zero multiple of 512 bytes".into(),
        ));
    }
    Ok(match kind {
        ContainerKind::Raw => Box::new(raw::RawDisk::create(path, size)?),
        ContainerKind::Imz => Box::new(zip::ZipDisk::create(path, size)?),
        ContainerKind::Wlz => {
            return Err(Error::Unsupported(
                "creating WLZ lists (create IMZ images instead)".into(),
            ));
        }
        ContainerKind::Vhd => Box::new(vhd::VhdDisk::create(path, size, dynamic)?),
        ContainerKind::Vhdx => Box::new(vhdx::VhdxDisk::create(path, size, dynamic)?),
        ContainerKind::Vmdk => Box::new(vmdk::VmdkDisk::create(path, size, dynamic)?),
    })
}

/// Create a container of the same kind and variant as `like` (for rebuilding an image in
/// place). IMZ keeps its member name and comment.
pub(crate) fn create_like(path: &Path, like: &ContainerInfo, size: u64) -> Result<Box<dyn Disk>> {
    let dynamic = matches!(like.variant.as_str(), "dynamic" | "monolithicSparse");
    match like.kind {
        ContainerKind::Imz => Ok(Box::new(zip::ZipDisk::create_named(
            path,
            size,
            like.member.clone(),
            like.comment.clone().unwrap_or_default(),
        )?)),
        ContainerKind::Wlz => Err(Error::Unsupported(
            "rebuilding an image inside a WLZ list".into(),
        )),
        ContainerKind::Vmdk if !dynamic => Err(Error::Unsupported(format!(
            "rebuilding a {} VMDK in place (convert it to a single-file format first)",
            like.variant
        ))),
        kind => create(path, kind, size, dynamic, true),
    }
}

/// Copy `size` bytes between disks, skipping unallocated and all-zero ranges.
/// Returns the number of bytes actually written.
pub(crate) fn copy_sparse(from: &mut dyn Disk, to: &mut dyn Disk, size: u64) -> io::Result<u64> {
    const CHUNK: u64 = 1024 * 1024;
    let mut buf = vec![0u8; CHUNK as usize];
    let mut stored = 0;
    let mut at = 0;
    while at < size {
        let n = CHUNK.min(size - at);
        if from.is_allocated(at, n)? {
            let b = &mut buf[..n as usize];
            from.read_at(at, b)?;
            if !b.iter().all(|&x| x == 0) {
                to.write_at(at, b)?;
                stored += n;
            }
        }
        at += n;
    }
    to.sync()?;
    Ok(stored)
}

/// Atomically replace `path` with `tmp` (same folder).
pub(crate) fn replace_file(tmp: &Path, path: &Path) -> io::Result<()> {
    std::fs::rename(tmp, path)
}
