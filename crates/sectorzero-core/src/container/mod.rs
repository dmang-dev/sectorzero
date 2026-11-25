//! Image containers: the file formats that hold a virtual disk's sectors.
//!
//! Every container exposes the same thing: a virtual disk of `size()` bytes with positional
//! reads and writes. Partition tables and file systems are layered on top (see `partition`,
//! `window` and `volume`).

mod cd;
mod clonezilla;
mod compressed;
mod mrimgx;
mod partclone;
mod pc98;
mod qcow2;
pub(crate) mod raw;
mod split;
mod vdi;
mod vhd;
mod vhdx;
mod vmdk;
mod wicrypt;
mod winbackup;
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
    /// Encrypt the stored image with this password from the next sync on, or store it
    /// unencrypted (`None`). Only IMZ/WLZ archives can do this.
    fn set_password(&mut self, _password: Option<&str>) -> Result<()> {
        Err(Error::Unsupported(format!(
            "passwords on {} images (only IMZ and WLZ archives can be encrypted)",
            self.info().kind.name()
        )))
    }
    /// `false` when the range is known to read as zeros without storing anything
    /// (an unallocated block in a sparse format). Used to keep conversions sparse.
    fn is_allocated(&mut self, _offset: u64, _len: u64) -> io::Result<bool> {
        Ok(true)
    }
}

pub(crate) type SharedDisk = Arc<Mutex<Box<dyn Disk>>>;

static PASSWORDS: Mutex<Vec<(std::path::PathBuf, String)>> = Mutex::new(Vec::new());

fn password_key(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        // Not created yet: canonicalize the folder and keep the name.
        match (path.parent(), path.file_name()) {
            (Some(dir), Some(name)) => std::fs::canonicalize(if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            })
            .map(|d| d.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
            _ => path.to_path_buf(),
        }
    })
}

/// Remember the password of an encrypted IMZ/WLZ archive at `path` (or forget it with
/// `None`). Every later open of that file uses it, and an IMZ created at `path` (by
/// [`crate::Volume::create`], [`crate::convert`] or a rebuild) is saved encrypted with it.
pub fn set_password(path: &Path, password: Option<&str>) {
    let key = password_key(path);
    let mut list = PASSWORDS.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(p, _)| *p != key);
    if let Some(pw) = password {
        list.push((key, pw.to_string()));
    }
}

pub(crate) fn password_for(path: &Path) -> Option<String> {
    let key = password_key(path);
    let list = PASSWORDS.lock().unwrap_or_else(|e| e.into_inner());
    list.iter()
        .find(|(p, _)| *p == key)
        .map(|(_, pw)| pw.clone())
}

static FALLBACK_PASSWORD: Mutex<Option<String>> = Mutex::new(None);

/// A password to try on any encrypted archive that has none registered with
/// [`set_password`] (for command-line tools that take one password for every image).
/// It is never used to encrypt a new file.
pub fn set_fallback_password(password: Option<&str>) {
    *FALLBACK_PASSWORD.lock().unwrap_or_else(|e| e.into_inner()) = password.map(str::to_string);
}

/// The password to open `path` with: its own, or the fallback.
pub(crate) fn open_password_for(path: &Path) -> Option<String> {
    password_for(path).or_else(|| {
        FALLBACK_PASSWORD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    })
}

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
    /// Virtual PC / Hyper-V disk: fixed, dynamic or differencing (.vhd, undo disks .vud).
    Vhd,
    Vhdx,
    Vmdk,
    /// VirtualBox disk image, fixed or dynamic.
    Vdi,
    /// QEMU copy-on-write v2/v3.
    Qcow2,
    /// Raw image split into pieces: .001, .002, ...
    Split,
    /// PC-98 T98-Next hard disk image.
    Nhd,
    /// PC-98 Anex86 hard disk image.
    Hdi,
    /// PC-98 Anex86 floppy image.
    Fdi,
    /// CD image: a cue sheet with its BIN track file, or a raw 2352-byte BIN (read-only).
    Cue,
    /// Nero CD image (read-only).
    Nrg,
    /// Alcohol 120% CD image: MDS descriptor + MDF data (read-only).
    Mds,
    /// Macrium Reflect X backup image (read-only).
    Mrimgx,
    /// A partclone partition image (read-only).
    Partclone,
    /// A Clonezilla / Rescuezilla image folder (read-only).
    Clonezilla,
    /// A physical drive (USB stick, memory card, floppy drive, hard disk), read and written
    /// sector for sector.
    Drive,
}

impl ContainerKind {
    /// Container implied by a file extension (for new files).
    /// A compressed name (`disk.vhd.gz`, `disk.zip`) gives the kind inside it.
    pub fn from_path(path: &Path) -> ContainerKind {
        if compressed::Codec::from_ext(path).is_some() {
            return match path.file_stem() {
                Some(stem) => ContainerKind::from_path(Path::new(stem)),
                None => ContainerKind::Raw,
            };
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "imz" => ContainerKind::Imz,
            "wlz" => ContainerKind::Wlz,
            "vhd" | "vud" | "avhd" => ContainerKind::Vhd,
            "vhdx" | "avhdx" => ContainerKind::Vhdx,
            "vmdk" => ContainerKind::Vmdk,
            "vdi" => ContainerKind::Vdi,
            "qcow2" => ContainerKind::Qcow2,
            "001" => ContainerKind::Split,
            "nhd" => ContainerKind::Nhd,
            "hdi" => ContainerKind::Hdi,
            "fdi" => ContainerKind::Fdi,
            "cue" => ContainerKind::Cue,
            "nrg" => ContainerKind::Nrg,
            "mds" | "mdf" => ContainerKind::Mds,
            "mrimgx" => ContainerKind::Mrimgx,
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
            ContainerKind::Vdi => "VirtualBox disk (VDI)",
            ContainerKind::Qcow2 => "QEMU copy-on-write disk (QCOW2)",
            ContainerKind::Split => "Split raw image (.001, .002, …)",
            ContainerKind::Nhd => "T98-Next hard disk (NHD)",
            ContainerKind::Hdi => "Anex86 hard disk (HDI)",
            ContainerKind::Fdi => "Anex86 floppy (FDI)",
            ContainerKind::Cue => "CD image (BIN/CUE)",
            ContainerKind::Nrg => "Nero CD image (NRG)",
            ContainerKind::Mds => "Alcohol 120% CD image (MDF/MDS)",
            ContainerKind::Mrimgx => "Macrium Reflect X image (MRIMGX)",
            ContainerKind::Partclone => "Partclone image",
            ContainerKind::Clonezilla => "Clonezilla image",
            ContainerKind::Drive => "Physical drive",
        }
    }

    /// Whether `dynamic: true` means anything for this container.
    pub fn has_dynamic_variant(self) -> bool {
        matches!(
            self,
            ContainerKind::Vhd | ContainerKind::Vhdx | ContainerKind::Vmdk | ContainerKind::Vdi
        )
    }

    /// Whether new images of this kind can be created (`create` / `convert` targets).
    pub fn can_create(self) -> bool {
        !matches!(
            self,
            ContainerKind::Wlz
                | ContainerKind::Split
                | ContainerKind::Cue
                | ContainerKind::Nrg
                | ContainerKind::Mds
                | ContainerKind::Mrimgx
                | ContainerKind::Partclone
                | ContainerKind::Clonezilla
                | ContainerKind::Drive
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
    /// Differencing disks: the parent image the unwritten sectors come from.
    pub parent: Option<String>,
    /// IMZ/WLZ: the image is stored password-encrypted.
    pub encrypted: bool,
    /// The image file is compressed or archived around this container: "gzip", "xz",
    /// "zstd", "bzip2", "zip" or "7z".
    pub compression: Option<String>,
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
            encrypted: false,
            parent: None,
            compression: None,
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

/// Size of a file or block device. Block devices (`/dev/fd0`, `/dev/sdb`) report a
/// metadata length of 0; seeking to the end gives their real size.
pub(crate) fn file_len(file: &mut File) -> io::Result<u64> {
    let meta = file.metadata()?;
    if meta.is_file() {
        Ok(meta.len())
    } else {
        file.seek(io::SeekFrom::End(0))
    }
}

/// Detect the container by content (not extension) and open it.
pub(crate) fn open(path: &Path, writable: bool, member: Option<&str>) -> Result<Box<dyn Disk>> {
    if crate::device::is_device_path(path) {
        return Ok(Box::new(crate::device::open(path, writable)?));
    }
    if path.is_dir() {
        if winbackup::looks_like_backup(path) {
            return Ok(Box::new(winbackup::WinBackupDisk::open(path, member)?));
        }
        if clonezilla::looks_like_clonezilla(path) {
            return Ok(Box::new(clonezilla::ClonezillaDisk::open(path, member)?));
        }
        return Err(Error::Invalid(format!(
            "{} is a folder, not a disk image (folders open only when they hold a Windows backup or a Clonezilla image)",
            path.display()
        )));
    }
    let mut file = File::open(path)?;
    let len = file_len(&mut file)?;
    let mut head = [0u8; 4096];
    let got = read_up_to(&mut file, 0, &mut head)?;
    let head = &head[..got];
    let mut tail = [0u8; 512];
    if len >= 512 {
        read_up_to(&mut file, len - 512, &mut tail)?;
    }
    drop(file);

    let codec = compressed::Codec::from_magic(head);
    let macrium = len >= 512 && mrimgx::looks_like_mrimgx(&tail);
    if member.is_some()
        && !head.starts_with(b"PK\x03\x04")
        && codec != Some(compressed::Codec::SevenZip)
        && !macrium
    {
        return Err(Error::Invalid(
            "--member only applies to archives (IMZ, WLZ, ZIP, 7z)".into(),
        ));
    }
    if macrium {
        return Ok(Box::new(mrimgx::MrimgxDisk::open(path, member)?));
    }
    if split::is_split_set(path) {
        return Ok(Box::new(split::SplitDisk::open(path, writable)?));
    }
    if let Some(codec) = codec {
        return Ok(Box::new(compressed::CompressedDisk::open(
            path, codec, writable, member,
        )?));
    }
    if head.starts_with(b"PK\x03\x04")
        && compressed::Codec::from_ext(path) == Some(compressed::Codec::Zip)
        && !compressed::zip_wants_imz(path)
    {
        return Ok(Box::new(compressed::CompressedDisk::open(
            path,
            compressed::Codec::Zip,
            writable,
            member,
        )?));
    }
    if head.starts_with(b"PK\x03\x04") {
        return Ok(Box::new(zip::ZipDisk::open(path, writable, member)?));
    }
    if partclone::looks_like_partclone(head) {
        return Ok(Box::new(partclone::PartcloneDisk::open(path)?));
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
    if vdi::looks_like_vdi(head) {
        return Ok(Box::new(vdi::VdiDisk::open(path, writable)?));
    }
    if qcow2::looks_like_qcow(head) {
        return Ok(Box::new(qcow2::Qcow2Disk::open(path, writable)?));
    }
    if pc98::looks_like_nhd(head) {
        return Ok(Box::new(pc98::Pc98Disk::open(
            path,
            ContainerKind::Nhd,
            writable,
        )?));
    }
    if cd::looks_like_mds(head) {
        return Ok(Box::new(cd::CdDisk::open_mds(path, writable)?));
    }
    if len >= 512 && cd::nrg_chunk_offset(&tail, len).is_some() {
        return Ok(Box::new(cd::CdDisk::open_nrg(path, writable)?));
    }
    // Formats without a signature: only when the extension says so.
    match ContainerKind::from_path(path) {
        ContainerKind::Cue => return Ok(Box::new(cd::CdDisk::open_cue(path, writable)?)),
        kind @ (ContainerKind::Hdi | ContainerKind::Fdi)
            if pc98::anex_header(head, len).is_some() =>
        {
            return Ok(Box::new(pc98::Pc98Disk::open(path, kind, writable)?));
        }
        ContainerKind::Mds => {
            if let Some(d) = cd::CdDisk::open_mdf(path, writable)? {
                return Ok(Box::new(d));
            }
        }
        _ => {}
    }
    if cd::has_sync(head) {
        return Ok(Box::new(cd::CdDisk::open_raw(path, writable)?));
    }
    Ok(Box::new(raw::RawDisk::open(path, writable)?))
}

thread_local! {
    static PARENT_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Open the parent of a differencing disk or snapshot (any container this crate reads),
/// read-only. Chains deeper than 32 disks (or loops) are refused.
pub(crate) fn open_parent(path: &Path) -> Result<Box<dyn Disk>> {
    let depth = PARENT_DEPTH.get();
    if depth >= 32 {
        return Err(Error::Invalid(
            "differencing disk chain is too long (or loops)".into(),
        ));
    }
    PARENT_DEPTH.set(depth + 1);
    let opened = open(path, false, None);
    PARENT_DEPTH.set(depth);
    opened.map_err(|e| Error::Invalid(format!("parent disk {}: {e}", path.display())))
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
    if let Some(codec) = compressed::Codec::from_ext(path)
        && !(codec == compressed::Codec::Zip && kind == ContainerKind::Imz)
    {
        return Ok(Box::new(compressed::CompressedDisk::create(
            path, codec, kind, size, dynamic,
        )?));
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
        ContainerKind::Vdi => Box::new(vdi::VdiDisk::create(path, size, dynamic)?),
        ContainerKind::Qcow2 => Box::new(qcow2::Qcow2Disk::create(path, size)?),
        ContainerKind::Nhd | ContainerKind::Hdi | ContainerKind::Fdi => {
            Box::new(pc98::Pc98Disk::create(path, kind, size)?)
        }
        ContainerKind::Split => {
            return Err(Error::Unsupported(
                "creating split images (create a raw image instead)".into(),
            ));
        }
        ContainerKind::Cue | ContainerKind::Nrg | ContainerKind::Mds => {
            return Err(Error::Unsupported(format!(
                "creating {} files (CD images are read-only; build an ISO instead)",
                kind.name()
            )));
        }
        ContainerKind::Mrimgx => {
            return Err(Error::Unsupported(
                "creating Macrium Reflect images (convert to another format instead)".into(),
            ));
        }
        ContainerKind::Partclone | ContainerKind::Clonezilla => {
            return Err(Error::Unsupported(
                "creating partclone or Clonezilla images (convert to another format instead)"
                    .into(),
            ));
        }
        ContainerKind::Drive => {
            return Err(Error::Unsupported(
                "creating a physical drive (write an image to a drive instead)".into(),
            ));
        }
    })
}

/// Create a container of the same kind and variant as `like` (for rebuilding an image in
/// place). IMZ keeps its member name and comment.
/// An encrypted IMZ is created encrypted with the password registered for `source`.
pub(crate) fn create_like(
    path: &Path,
    like: &ContainerInfo,
    size: u64,
    source: &Path,
) -> Result<Box<dyn Disk>> {
    let dynamic = matches!(like.variant.as_str(), "dynamic" | "monolithicSparse");
    if like.encrypted {
        set_password(path, password_for(source).as_deref());
    }
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
        ContainerKind::Split => Err(Error::Unsupported(
            "rebuilding a split image in place (convert it to a single file first)".into(),
        )),
        ContainerKind::Vhd if like.variant == "differencing" => Err(Error::Unsupported(
            "rebuilding a differencing VHD in place (convert it to a standalone image first)"
                .into(),
        )),
        _ if like.parent.is_some() => Err(Error::Unsupported(
            "rebuilding a differencing disk or snapshot in place (convert it to a standalone image first)"
                .into(),
        )),
        ContainerKind::Vmdk if !dynamic => Err(Error::Unsupported(format!(
            "rebuilding a {} VMDK in place (convert it to a single-file format first)",
            like.variant
        ))),
        kind => create(path, kind, size, dynamic, true),
    }
}

/// Create an empty differencing disk at `path` over `parent`: a VHD (`.vhd`, or `.vud` for a
/// Virtual PC undo disk) over any VHD, a VHDX (`.vhdx`, `.avhdx`) over a VHDX, or a VMDK
/// snapshot (`.vmdk`) over a VMDK. Reads of
/// unwritten sectors come from the parent; writes go to the new child only.
pub fn create_differencing(path: &Path, parent: &Path, overwrite: bool) -> Result<ContainerInfo> {
    if !overwrite && path.exists() {
        return Err(Error::Exists(path.display().to_string()));
    }
    if path.exists() && parent.exists() && same_path(path, parent) {
        return Err(Error::Invalid(
            "the differencing disk cannot replace its own parent".into(),
        ));
    }
    let mut disk: Box<dyn Disk> = match ContainerKind::from_path(path) {
        ContainerKind::Vhdx => Box::new(vhdx::VhdxDisk::create_differencing(path, parent)?),
        ContainerKind::Vmdk => Box::new(vmdk::VmdkDisk::create_differencing(path, parent)?),
        _ => Box::new(vhd::VhdDisk::create_differencing(path, parent)?),
    };
    disk.sync()?;
    Ok(disk.info())
}

fn same_path(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Copy `size` bytes between disks, skipping unallocated and all-zero ranges.
/// Returns the number of bytes actually written.
pub(crate) fn copy_sparse(from: &mut dyn Disk, to: &mut dyn Disk, size: u64) -> io::Result<u64> {
    copy_sparse_progress(from, to, size, &mut |_, _| {})
}

/// [`copy_sparse`], reporting (bytes done, total) after every chunk.
pub(crate) fn copy_sparse_progress(
    from: &mut dyn Disk,
    to: &mut dyn Disk,
    size: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<u64> {
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
        progress(at, size);
    }
    to.sync()?;
    Ok(stored)
}

/// Atomically replace `path` with `tmp` (same folder).
pub(crate) fn replace_file(tmp: &Path, path: &Path) -> io::Result<()> {
    std::fs::rename(tmp, path)
}
