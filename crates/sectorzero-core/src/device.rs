//! Physical drives: USB sticks, memory cards, floppy drives and hard disks.
//!
//! A drive opens like a raw image (`\\.\PhysicalDrive2` or `\\.\A:` on Windows, `/dev/sdb`
//! or `/dev/fd0` on Linux), so everything that reads images also reads drives: browse it,
//! read it into an image with `convert`, or write an image back with
//! [`write_image_to_drive`].
//!
//! Raw drive access needs whole-sector, sector-aligned transfers on Windows, so every
//! read and write goes through a sector-aligned bounce buffer ([`SectorIo`]); partial
//! sectors are read, patched and written back.
//!
//! Safety: the drive holding the running operating system is never opened for writing.
//! Before writing, every volume on the drive is locked and dismounted (Windows) or must
//! not be mounted (Linux), so no file system has the drive's sectors cached while they
//! change underneath it.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::Serialize;

use crate::container::{ContainerInfo, ContainerKind, Disk};
use crate::error::{Error, Result};

/// One drive as listed by [`list_drives`].
#[derive(Debug, Clone, Serialize)]
pub struct DriveInfo {
    /// What to open: `\\.\PhysicalDrive1`, `\\.\A:`, `/dev/sdb`, …
    pub path: String,
    /// Short name: "Disk 1", "Floppy A:", "sdb".
    pub name: String,
    /// Vendor and model, as the device reports them.
    pub model: String,
    /// Size of the medium in bytes (0 when no medium is inserted or it cannot be read).
    pub size: u64,
    pub sector_size: u32,
    /// Removable medium or a removable bus (USB, SD/MMC, floppy).
    pub removable: bool,
    /// "USB", "SATA", "NVMe", "SD", "Floppy", "Virtual", …
    pub bus: String,
    /// Mounted volumes on the drive (drive letters or mount points).
    pub volumes: Vec<String>,
    /// Holds the running operating system: never written to.
    pub system: bool,
    /// A medium is present.
    pub media: bool,
}

impl DriveInfo {
    /// "Disk 2 — SanDisk Ultra (USB, 14.9 GB, E:)" style description.
    pub fn describe(&self) -> String {
        let mut parts = vec![self.bus.clone()];
        if self.media {
            parts.push(human(self.size));
        } else {
            parts.push("no medium".into());
        }
        if !self.volumes.is_empty() {
            parts.push(self.volumes.join(" "));
        }
        if self.system {
            parts.push("system disk".into());
        }
        let model = if self.model.is_empty() {
            String::new()
        } else {
            format!(" — {}", self.model)
        };
        format!("{}{} ({})", self.name, model, parts.join(", "))
    }
}

fn human(n: u64) -> String {
    const U: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} bytes")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Whether `path` names a drive rather than an image file.
pub fn is_device_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if cfg!(windows) {
        let s = s.replace('/', "\\");
        s.starts_with("\\\\.\\") || s.starts_with("\\\\?\\GLOBALROOT")
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            s.starts_with("/dev/")
                && std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device())
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

/// The drives on this computer, system disk included (marked).
pub fn list_drives() -> Result<Vec<DriveInfo>> {
    sys::list()
}

/// The listing entry for `path`, if it is a known drive.
pub fn drive_info(path: &str) -> Result<Option<DriveInfo>> {
    let want = normalize(path);
    Ok(list_drives()?
        .into_iter()
        .find(|d| normalize(&d.path) == want))
}

fn normalize(p: &str) -> String {
    p.replace('/', "\\").to_ascii_lowercase()
}

// --- sector-aligned I/O -------------------------------------------------------------------

/// Positional reads and writes over a device that only accepts whole, aligned sectors.
/// Transfers go through a buffer aligned in memory to 4 KiB.
pub(crate) struct SectorIo<F> {
    f: F,
    sector: u64,
    len: u64,
    buf: AlignedBuf,
}

/// Largest single transfer.
const MAX_IO: usize = 4 << 20;

struct AlignedBuf {
    v: Vec<u8>,
    off: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> AlignedBuf {
        let v = vec![0u8; len + 4096];
        let off = v.as_ptr().align_offset(4096);
        AlignedBuf { v, off }
    }
    fn get(&mut self, len: usize) -> &mut [u8] {
        &mut self.v[self.off..self.off + len]
    }
}

impl<F: Read + Write + Seek> SectorIo<F> {
    pub(crate) fn new(f: F, sector: u64, len: u64) -> SectorIo<F> {
        SectorIo {
            f,
            sector: sector.max(1),
            len,
            buf: AlignedBuf::new(MAX_IO),
        }
    }

    /// Whole sectors `[at, at + n)` into the bounce buffer.
    fn fill(&mut self, at: u64, n: usize) -> io::Result<()> {
        self.f.seek(SeekFrom::Start(at))?;
        let b = self.buf.get(n);
        let mut got = 0;
        while got < n {
            match self.f.read(&mut b[got..]) {
                Ok(0) => {
                    b[got..].fill(0);
                    break;
                }
                Ok(k) => got += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn flush_buf(&mut self, at: u64, n: usize) -> io::Result<()> {
        self.f.seek(SeekFrom::Start(at))?;
        let b = self.buf.get(n);
        self.f.write_all(b)
    }

    /// Aligned span covering `[off, off + want)`, at most `MAX_IO` bytes, clipped to the
    /// device: (start, length).
    fn span(&self, off: u64, want: usize) -> (u64, usize) {
        let s = self.sector;
        let start = off / s * s;
        let end = (off + want as u64).div_ceil(s) * s;
        let end = end.min(start + MAX_IO as u64).min(self.len.div_ceil(s) * s);
        (start, (end - start) as usize)
    }

    pub(crate) fn read_at(&mut self, mut off: u64, mut out: &mut [u8]) -> io::Result<()> {
        while !out.is_empty() {
            if off >= self.len {
                out.fill(0);
                return Ok(());
            }
            let (start, n) = self.span(off, out.len());
            self.fill(start, n)?;
            let skip = (off - start) as usize;
            let take = (n - skip).min(out.len()).min((self.len - off) as usize);
            out[..take].copy_from_slice(&self.buf.get(n)[skip..skip + take]);
            out = &mut out[take..];
            off += take as u64;
        }
        Ok(())
    }

    pub(crate) fn write_at(&mut self, mut off: u64, mut data: &[u8]) -> io::Result<()> {
        if off + data.len() as u64 > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write past the end of the drive",
            ));
        }
        while !data.is_empty() {
            let (start, n) = self.span(off, data.len());
            let skip = (off - start) as usize;
            let take = (n - skip).min(data.len());
            if skip != 0 || take != n {
                // Partial sectors at either end: read, patch, write back.
                self.fill(start, n)?;
            }
            self.buf.get(n)[skip..skip + take].copy_from_slice(&data[..take]);
            self.flush_buf(start, n)?;
            data = &data[take..];
            off += take as u64;
        }
        Ok(())
    }

    pub(crate) fn inner(&mut self) -> &mut F {
        &mut self.f
    }
}

// --- the drive as a disk -------------------------------------------------------------------

pub(crate) struct DeviceDisk {
    io: SectorIo<File>,
    sector: u32,
    len: u64,
    /// Locked volume handles (Windows); released when the disk is dropped.
    _locks: Vec<File>,
}

/// Open a drive. Writable opens refuse the system disk and lock / require unmounted
/// volumes.
pub(crate) fn open(path: &Path, writable: bool) -> Result<DeviceDisk> {
    let (file, len, sector, locks) =
        sys::open(path, writable).map_err(|e| access_error(path, e))?;
    Ok(DeviceDisk {
        io: SectorIo::new(file, u64::from(sector), len),
        sector,
        len,
        _locks: locks,
    })
}

/// Lock the volumes of a whole drive that is about to be read as a source (Windows), so
/// that nothing changes it mid-copy: Windows itself writes to a mounted volume now and then
/// (System Volume Information, for one). The locks hold until the handles are dropped.
/// A volume path (`\\.\A:`) or another platform gives no locks.
pub(crate) fn lock_for_reading(path: &Path) -> Result<Vec<File>> {
    sys::lock_for_reading(path).map_err(|e| access_error(path, e))
}

fn access_error(path: &Path, e: Error) -> Error {
    match e {
        Error::Io(io) if io.kind() == io::ErrorKind::PermissionDenied => Error::Invalid(format!(
            "access to {} was denied: opening drives needs {}",
            path.display(),
            if cfg!(windows) {
                "administrator rights (run SectorZero as administrator)"
            } else {
                "root (or membership in the disk group)"
            }
        )),
        other => other,
    }
}

impl Disk for DeviceDisk {
    fn size(&self) -> u64 {
        self.len
    }
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.io.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.io.write_at(offset, buf)
    }
    fn sync(&mut self) -> io::Result<()> {
        let f = self.io.inner();
        f.flush()?;
        sys::after_write(f);
        Ok(())
    }
    fn info(&self) -> ContainerInfo {
        ContainerInfo::simple(ContainerKind::Drive, "physical drive", self.len)
    }
}

// --- writing an image to a drive ------------------------------------------------------------

/// Options for [`write_image_to_drive`].
#[derive(Debug, Clone, Default)]
pub struct DriveWriteOptions {
    /// Read everything back and compare.
    pub verify: bool,
    /// Allow a drive that is neither removable nor on a removable bus (an internal or
    /// external hard disk). The system disk is refused regardless.
    pub allow_fixed: bool,
    /// For IMZ/WLZ: which image.
    pub member: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveWriteReport {
    pub drive: DriveInfo,
    /// Bytes written (the image's disk size).
    pub bytes: u64,
    pub verified: bool,
}

/// Progress callback: (phase, bytes done, bytes total). Phases: "writing", "verifying",
/// "reading".
pub type Progress<'a> = &'a mut dyn FnMut(&str, u64, u64);

/// The checks before anything is written to a drive: it is a drive, not the system disk,
/// and removable unless fixed disks were allowed.
fn writable_drive(drive: &str, allow_fixed: bool) -> Result<DriveInfo> {
    if !is_device_path(Path::new(drive)) {
        return Err(Error::Invalid(format!("{drive} is not a drive")));
    }
    let info = drive_info(drive)?.ok_or_else(|| Error::NotFound(format!("drive {drive}")))?;
    if info.system {
        return Err(Error::Invalid(format!(
            "{} holds the running operating system; SectorZero never writes to it",
            info.describe()
        )));
    }
    if !info.removable && !allow_fixed {
        return Err(Error::Invalid(format!(
            "{} is a fixed disk; confirm that you mean it (allow fixed disks) to write to it",
            info.describe()
        )));
    }
    Ok(info)
}

/// Whether two device paths name the same drive, or one is a partition of the other
/// (`/dev/sdb` and `/dev/sdb1`).
fn same_drive(a: &str, b: &str) -> bool {
    let canon = |p: &str| {
        std::fs::canonicalize(p)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.to_string())
    };
    let (a, b) = (normalize(&canon(a)), normalize(&canon(b)));
    let part_of = |disk: &str, part: &str| {
        part.strip_prefix(disk).is_some_and(|rest| {
            let rest = rest.strip_prefix('p').unwrap_or(rest);
            !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
        })
    };
    a == b || (cfg!(unix) && (part_of(&a, &b) || part_of(&b, &a)))
}

/// Write the disk inside an image (any container, or another drive: a drive-to-drive copy)
/// sector for sector onto a drive, optionally verifying it.
pub fn write_image_to_drive(
    image: &Path,
    drive: &str,
    opts: &DriveWriteOptions,
    progress: Progress,
) -> Result<DriveWriteReport> {
    let dpath = Path::new(drive);
    if is_device_path(image) && same_drive(&image.to_string_lossy(), drive) {
        return Err(Error::Invalid(format!(
            "{} and {drive} are the same drive",
            image.display()
        )));
    }
    let info = writable_drive(drive, opts.allow_fixed)?;
    // A drive-to-drive copy: the source must not change while it is copied and verified.
    let _source_locks = if is_device_path(image) {
        lock_for_reading(image)?
    } else {
        Vec::new()
    };
    let mut src = crate::container::open(image, false, opts.member.as_deref())?;
    let size = crate::image::disk_size_of(src.as_mut())?;
    let mut dev = open(dpath, true)?;
    if size > dev.size() {
        return Err(Error::Invalid(format!(
            "the image ({size} bytes) is larger than the drive ({} bytes)",
            dev.size()
        )));
    }
    const CHUNK: u64 = 1 << 20;
    let mut buf = vec![0u8; CHUNK as usize];
    let mut at = 0;
    while at < size {
        let n = CHUNK.min(size - at) as usize;
        src.read_at(at, &mut buf[..n])?;
        dev.write_at(at, &buf[..n])?;
        at += n as u64;
        progress("writing", at, size);
    }
    dev.sync()?;
    let mut verified = false;
    if opts.verify {
        let mut back = vec![0u8; CHUNK as usize];
        let mut at = 0;
        while at < size {
            let n = CHUNK.min(size - at) as usize;
            src.read_at(at, &mut buf[..n])?;
            dev.read_at(at, &mut back[..n])?;
            if buf[..n] != back[..n] {
                let i = buf[..n]
                    .iter()
                    .zip(&back[..n])
                    .position(|(a, b)| a != b)
                    .unwrap_or(0);
                return Err(Error::Invalid(format!(
                    "verification failed: the drive differs from the image at byte {}",
                    at + i as u64
                )));
            }
            at += n as u64;
            progress("verifying", at, size);
        }
        verified = true;
    }
    Ok(DriveWriteReport {
        drive: info,
        bytes: size,
        verified,
    })
}

// --- wiping a drive -----------------------------------------------------------------------

/// What [`wipe_drive`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WipePattern {
    /// Zeros (one pass).
    #[default]
    Zero,
    /// Pseudo-random data, then zeros (two passes).
    Random,
}

/// Options for [`wipe_drive`].
#[derive(Debug, Clone, Default)]
pub struct DriveWipeOptions {
    pub pattern: WipePattern,
    /// Read the final pass back and check it.
    pub verify: bool,
    pub allow_fixed: bool,
}

/// Overwrite a whole drive (partition tables and file systems included). Progress phases:
/// "random", "zeroing", "verifying".
pub fn wipe_drive(
    drive: &str,
    opts: &DriveWipeOptions,
    progress: Progress,
) -> Result<DriveWriteReport> {
    let info = writable_drive(drive, opts.allow_fixed)?;
    let mut dev = open(Path::new(drive), true)?;
    let size = dev.size();
    const CHUNK: u64 = 1 << 20;
    let mut buf = vec![0u8; CHUNK as usize];
    if opts.pattern == WipePattern::Random {
        // xorshift64*: fast, and not something a drive can compress or deduplicate.
        let mut x = crate::volume::new_volume_id() as u64 | 1 | (crate::util::unix_now() << 32);
        let mut at = 0;
        while at < size {
            let n = CHUNK.min(size - at) as usize;
            for w in buf[..n].chunks_mut(8) {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
                w.copy_from_slice(&v[..w.len()]);
            }
            dev.write_at(at, &buf[..n])?;
            at += n as u64;
            progress("random", at, size);
        }
        dev.sync()?;
    }
    buf.fill(0);
    let mut at = 0;
    while at < size {
        let n = CHUNK.min(size - at) as usize;
        dev.write_at(at, &buf[..n])?;
        at += n as u64;
        progress("zeroing", at, size);
    }
    dev.sync()?;
    let mut verified = false;
    if opts.verify {
        let mut at = 0;
        while at < size {
            let n = CHUNK.min(size - at) as usize;
            dev.read_at(at, &mut buf[..n])?;
            if let Some(i) = buf[..n].iter().position(|&b| b != 0) {
                return Err(Error::Invalid(format!(
                    "verification failed: byte {} of the drive is not zero",
                    at + i as u64
                )));
            }
            at += n as u64;
            progress("verifying", at, size);
        }
        verified = true;
    }
    Ok(DriveWriteReport {
        drive: info,
        bytes: size,
        verified,
    })
}

/// Eject the medium (Zip, Jaz, SuperDisk, magneto-optical, CD…). Windows locks and
/// dismounts the drive's volumes first; Linux refuses a drive that is mounted. Drives that
/// cannot eject by themselves (PC floppy drives, card readers) report an error.
pub fn eject_drive(drive: &str) -> Result<()> {
    let path = Path::new(drive);
    if !is_device_path(path) {
        return Err(Error::Invalid(format!("{drive} is not a drive")));
    }
    sys::eject(path).map_err(|e| access_error(path, e))
}

// --- Windows ------------------------------------------------------------------------------

#[cfg(windows)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Storage::FileSystem::{
        FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetLogicalDrives,
        GetVolumePathNamesForVolumeNameW, QueryDosDeviceW,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

    use super::DriveInfo;
    use crate::error::{Error, Result};
    use crate::util::{le32, le64};

    const IOCTL_DISK_GET_DRIVE_GEOMETRY: u32 = 0x0007_0000;
    const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
    const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;
    const IOCTL_DISK_UPDATE_PROPERTIES: u32 = 0x0007_0140;
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
    const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
    const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
    const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020;
    const FSCTL_ALLOW_EXTENDED_DASD_IO: u32 = 0x0009_0083;
    const IOCTL_STORAGE_MEDIA_REMOVAL: u32 = 0x002D_4804;
    const IOCTL_STORAGE_EJECT_MEDIA: u32 = 0x002D_4808;
    const SHARE_RW: u32 = 0x1 | 0x2;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn from_wide(b: &[u16]) -> String {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf16_lossy(&b[..end])
    }

    /// DeviceIoControl with byte buffers (the output is 8-byte aligned).
    fn ioctl(f: &File, code: u32, input: &[u8], out_len: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![0u64; out_len.div_ceil(8)];
        let mut got = 0u32;
        // SAFETY: the buffers outlive the call and their lengths are passed with them.
        let ok = unsafe {
            DeviceIoControl(
                f.as_raw_handle() as _,
                code,
                if input.is_empty() {
                    null()
                } else {
                    input.as_ptr().cast()
                },
                input.len() as u32,
                if out_len == 0 {
                    null_mut()
                } else {
                    out.as_mut_ptr().cast()
                },
                (out.len() * 8) as u32,
                &mut got,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let bytes: Vec<u8> = out.iter().flat_map(|w| w.to_le_bytes()).collect();
        Ok(bytes[..(got as usize).min(bytes.len())].to_vec())
    }

    /// Open for device queries only (no read/write access: works without administrator
    /// rights).
    fn open_query(path: &str) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(0)
            .share_mode(SHARE_RW)
            .open(path)
    }

    fn open_rw(path: &str, write: bool) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(write)
            .share_mode(SHARE_RW)
            .open(path)
    }

    /// Size and logical sector size of an open drive or volume.
    fn geometry(f: &File) -> io::Result<(u64, u32)> {
        if let Ok(g) = ioctl(f, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], 256)
            && g.len() >= 32
        {
            let bps = le32(&g, 20);
            let size = le64(&g, 24);
            return Ok((size, bps.max(512)));
        }
        let g = ioctl(f, IOCTL_DISK_GET_DRIVE_GEOMETRY, &[], 24)?;
        let (c, t, s, b) = (le64(&g, 0), le32(&g, 12), le32(&g, 16), le32(&g, 20));
        Ok((c * u64::from(t) * u64::from(s) * u64::from(b), b.max(512)))
    }

    fn length(f: &File) -> Option<u64> {
        ioctl(f, IOCTL_DISK_GET_LENGTH_INFO, &[], 8)
            .ok()
            .map(|b| le64(&b, 0))
    }

    struct Props {
        model: String,
        removable: bool,
        bus: String,
    }

    fn properties(f: &File) -> Option<Props> {
        // STORAGE_PROPERTY_QUERY { StorageDeviceProperty, PropertyStandardQuery }.
        let d = ioctl(f, IOCTL_STORAGE_QUERY_PROPERTY, &[0u8; 12], 1024).ok()?;
        if d.len() < 32 {
            return None;
        }
        let text = |off: u32| -> String {
            let o = off as usize;
            if o == 0 || o >= d.len() {
                return String::new();
            }
            let end = d[o..]
                .iter()
                .position(|&c| c == 0)
                .map_or(d.len(), |e| o + e);
            String::from_utf8_lossy(&d[o..end]).trim().to_string()
        };
        let vendor = text(le32(&d, 12));
        let product = text(le32(&d, 16));
        let model = [vendor, product]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let bus_type = le32(&d, 28);
        let bus = match bus_type {
            1 => "SCSI",
            2 => "ATAPI",
            3 => "ATA",
            4 => "FireWire",
            6 => "Fibre Channel",
            7 => "USB",
            8 => "RAID",
            9 => "iSCSI",
            0xA => "SAS",
            0xB => "SATA",
            0xC => "SD",
            0xD => "MMC",
            0xE => "Virtual",
            0xF => "Virtual disk file",
            0x10 => "Storage Spaces",
            0x11 => "NVMe",
            0x13 => "UFS",
            _ => "Other",
        }
        .to_string();
        let removable = d[10] != 0 || matches!(bus_type, 7 | 0xC | 0xD);
        Some(Props {
            model,
            removable,
            bus,
        })
    }

    /// Every volume: (\\?\Volume{…} without the trailing backslash, mount paths, disks).
    fn volumes() -> Vec<(String, Vec<String>, Vec<u32>)> {
        let mut out = Vec::new();
        let mut name = vec![0u16; 300];
        // SAFETY: the buffer is valid for its length.
        let h = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
        if h.is_null() || h as isize == -1 {
            return out;
        }
        loop {
            let vol = from_wide(&name);
            let mut paths = vec![0u16; 1024];
            let mut n = 0u32;
            let wv = wide(&vol);
            // SAFETY: as above.
            let ok = unsafe {
                GetVolumePathNamesForVolumeNameW(
                    wv.as_ptr(),
                    paths.as_mut_ptr(),
                    paths.len() as u32,
                    &mut n,
                )
            };
            let mounts: Vec<String> = if ok != 0 {
                paths[..n as usize]
                    .split(|&c| c == 0)
                    .filter(|s| !s.is_empty())
                    .map(String::from_utf16_lossy)
                    .collect()
            } else {
                Vec::new()
            };
            let trimmed = vol.trim_end_matches('\\').to_string();
            let disks = open_query(&trimmed)
                .ok()
                .map(|f| volume_disks(&f))
                .unwrap_or_default();
            out.push((trimmed, mounts, disks));
            // SAFETY: as above.
            if unsafe { FindNextVolumeW(h, name.as_mut_ptr(), name.len() as u32) } == 0 {
                break;
            }
        }
        // SAFETY: `h` came from FindFirstVolumeW.
        unsafe { FindVolumeClose(h) };
        out
    }

    fn volume_disks(f: &File) -> Vec<u32> {
        let Ok(b) = ioctl(f, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], 8 + 24 * 32) else {
            return Vec::new();
        };
        let n = le32(&b, 0) as usize;
        (0..n)
            .filter(|i| 8 + 24 * i + 4 <= b.len())
            .map(|i| le32(&b, 8 + 24 * i))
            .collect()
    }

    fn system_disks() -> Vec<u32> {
        let mut buf = vec![0u16; 300];
        // SAFETY: the buffer is valid for its length.
        let n = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
        let dir = from_wide(&buf[..(n as usize).min(buf.len())]);
        let Some(letter) = dir.get(..2) else {
            return Vec::new();
        };
        open_query(&format!("\\\\.\\{letter}"))
            .map(|f| volume_disks(&f))
            .unwrap_or_default()
    }

    fn floppy_letters() -> Vec<char> {
        // SAFETY: no arguments.
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let mut target = vec![0u16; 512];
            let dev = wide(&format!("{letter}:"));
            // SAFETY: the buffers are valid for their lengths.
            let n =
                unsafe { QueryDosDeviceW(dev.as_ptr(), target.as_mut_ptr(), target.len() as u32) };
            if n > 0
                && from_wide(&target)
                    .to_ascii_lowercase()
                    .contains("\\device\\floppy")
            {
                out.push(letter);
            }
        }
        out
    }

    pub(super) fn list() -> Result<Vec<DriveInfo>> {
        let vols = volumes();
        let system = system_disks();
        let mut out = Vec::new();
        for n in 0..64u32 {
            let path = format!("\\\\.\\PhysicalDrive{n}");
            let Ok(f) = open_query(&path) else { continue };
            let geo = geometry(&f).ok();
            let props = properties(&f);
            let mut volumes: Vec<String> = vols
                .iter()
                .filter(|v| v.2.contains(&n))
                .flat_map(|v| v.1.iter().map(|m| m.trim_end_matches('\\').to_string()))
                .collect();
            volumes.sort();
            out.push(DriveInfo {
                path,
                name: format!("Disk {n}"),
                model: props.as_ref().map(|p| p.model.clone()).unwrap_or_default(),
                size: geo.map_or(0, |g| g.0),
                sector_size: geo.map_or(512, |g| g.1),
                removable: props.as_ref().is_some_and(|p| p.removable),
                bus: props.map(|p| p.bus).unwrap_or_else(|| "Unknown".into()),
                volumes,
                system: system.contains(&n),
                media: geo.is_some_and(|g| g.0 > 0),
            });
        }
        for letter in floppy_letters() {
            let path = format!("\\\\.\\{letter}:");
            let geo = open_query(&path).ok().and_then(|f| geometry(&f).ok());
            out.push(DriveInfo {
                path,
                name: format!("Floppy {letter}:"),
                model: String::new(),
                size: geo.map_or(0, |g| g.0),
                sector_size: geo.map_or(512, |g| g.1),
                removable: true,
                bus: "Floppy".into(),
                volumes: vec![format!("{letter}:")],
                system: false,
                media: geo.is_some_and(|g| g.0 > 0),
            });
        }
        Ok(out)
    }

    /// Disk number of a `\\.\PhysicalDriveN` path, or None for a volume path.
    fn disk_number(path: &str) -> Option<u32> {
        let lower = path.to_ascii_lowercase();
        lower
            .rsplit_once("physicaldrive")
            .and_then(|(_, n)| n.parse().ok())
    }

    pub(super) fn open(path: &Path, writable: bool) -> Result<(File, u64, u32, Vec<File>)> {
        let p = path.to_string_lossy().replace('/', "\\");
        let disk = disk_number(&p).or_else(|| {
            open_query(&p)
                .ok()
                .and_then(|f| ioctl(&f, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], 12).ok())
                .filter(|b| b.len() >= 12 && le32(b, 8) == 0)
                .map(|b| le32(&b, 4))
        });
        let mut locks = Vec::new();
        if writable {
            if let Some(n) = disk
                && system_disks().contains(&n)
            {
                return Err(Error::Invalid(format!(
                    "{p} holds the running operating system; SectorZero never writes to it"
                )));
            }
            if let Some(n) = disk.filter(|_| disk_number(&p).is_some()) {
                // A whole disk: lock and dismount every volume on it first.
                locks = lock_volumes(&p, n, true)?;
            }
        }
        let f = open_rw(&p, writable)?;
        let volume = disk_number(&p).is_none();
        if volume {
            // A volume (floppy drive letter): the whole medium, not just the file system,
            // and nobody else using it while it is written.
            let _ = ioctl(&f, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], 0);
            if writable {
                ioctl(&f, FSCTL_LOCK_VOLUME, &[], 0).map_err(|e| {
                    Error::Invalid(format!(
                        "{p} is in use ({e}); close the programs and windows using it"
                    ))
                })?;
                ioctl(&f, FSCTL_DISMOUNT_VOLUME, &[], 0)?;
            }
        }
        let (geo_size, sector) = geometry(&f)?;
        let len = length(&f).filter(|&l| l > 0).unwrap_or(geo_size);
        if len == 0 {
            return Err(Error::Invalid(format!("{p}: no medium in the drive")));
        }
        Ok((f, len, sector, locks))
    }

    /// Lock every volume on disk `n` (`p` is its path, for messages), dismounting them too
    /// when the disk is about to be overwritten.
    fn lock_volumes(p: &str, n: u32, dismount: bool) -> Result<Vec<File>> {
        let mut locks = Vec::new();
        for (vol, mounts, disks) in volumes() {
            if !disks.contains(&n) {
                continue;
            }
            let v = open_rw(&vol, true)?;
            let what = mounts.first().cloned().unwrap_or(vol.clone());
            ioctl(&v, FSCTL_LOCK_VOLUME, &[], 0).map_err(|e| {
                Error::Invalid(format!(
                    "volume {what} on {p} is in use ({e}); close the programs and \
                     windows using it and try again"
                ))
            })?;
            if dismount {
                ioctl(&v, FSCTL_DISMOUNT_VOLUME, &[], 0)?;
            }
            locks.push(v);
        }
        Ok(locks)
    }

    pub(super) fn lock_for_reading(path: &Path) -> Result<Vec<File>> {
        let p = path.to_string_lossy().replace('/', "\\");
        match disk_number(&p) {
            Some(n) => lock_volumes(&p, n, false),
            None => Ok(Vec::new()),
        }
    }

    /// Ask Windows to re-read the partition table after a whole-disk write.
    pub(super) fn after_write(f: &File) {
        let _ = ioctl(f, IOCTL_DISK_UPDATE_PROPERTIES, &[], 0);
    }

    /// Lock and dismount like a write does, allow removal, then eject.
    pub(super) fn eject(path: &Path) -> Result<()> {
        let (f, _, _, locks) = open(path, true)?;
        let _ = ioctl(&f, IOCTL_STORAGE_MEDIA_REMOVAL, &[0], 0);
        let r = ioctl(&f, IOCTL_STORAGE_EJECT_MEDIA, &[], 0);
        drop(f);
        drop(locks);
        r.map(|_| ()).map_err(|e| {
            Error::Invalid(format!(
                "{} did not eject its medium ({e}); remove it by hand",
                path.display()
            ))
        })
    }
}

// --- Linux and other Unix -------------------------------------------------------------------

#[cfg(not(windows))]
mod sys {
    use std::fs::{self, File};
    use std::path::Path;

    use super::DriveInfo;
    use crate::error::{Error, Result};

    fn read(p: &str) -> String {
        fs::read_to_string(p).unwrap_or_default().trim().to_string()
    }

    /// (device, mount point) for every mounted block device.
    fn mounts() -> Vec<(String, String)> {
        read("/proc/mounts")
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?.to_string(), it.next()?.to_string()))
            })
            .filter(|(d, _)| d.starts_with("/dev/"))
            .collect()
    }

    /// Whether `dev` (e.g. /dev/sdb1) is `disk` (sdb) or one of its partitions.
    fn on_disk(dev: &str, disk: &str) -> bool {
        let Some(name) = dev.strip_prefix("/dev/") else {
            return false;
        };
        name == disk
            || (name.starts_with(disk) && Path::new(&format!("/sys/block/{disk}/{name}")).exists())
    }

    pub(super) fn list() -> Result<Vec<DriveInfo>> {
        let mounts = mounts();
        let mut out = Vec::new();
        let Ok(dir) = fs::read_dir("/sys/block") else {
            return Ok(out);
        };
        let mut names: Vec<String> = dir
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                !["loop", "ram", "zram", "dm-", "md", "sr"]
                    .iter()
                    .any(|p| n.starts_with(p))
            })
            .collect();
        names.sort();
        for n in names {
            let base = format!("/sys/block/{n}");
            let size = read(&format!("{base}/size")).parse::<u64>().unwrap_or(0) * 512;
            let sector = read(&format!("{base}/queue/logical_block_size"))
                .parse()
                .unwrap_or(512);
            let link = fs::read_link(&base)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let bus = if n.starts_with("fd") {
                "Floppy"
            } else if n.starts_with("mmcblk") {
                "SD"
            } else if n.starts_with("nvme") {
                "NVMe"
            } else if n.starts_with("vd") {
                "Virtual"
            } else if link.contains("/usb") {
                "USB"
            } else {
                "ATA/SCSI"
            };
            let model = [
                read(&format!("{base}/device/vendor")),
                read(&format!("{base}/device/model")),
            ]
            .into_iter()
            // virtio and NVMe give a PCI vendor id ("0x1af4") here, not a name.
            .filter(|s| !s.is_empty() && !s.starts_with("0x"))
            .collect::<Vec<_>>()
            .join(" ");
            let volumes: Vec<String> = mounts
                .iter()
                .filter(|(d, _)| on_disk(d, &n))
                .map(|(_, m)| m.clone())
                .collect();
            let system = volumes
                .iter()
                .any(|m| m == "/" || m == "/boot" || m == "/usr");
            out.push(DriveInfo {
                path: format!("/dev/{n}"),
                name: n.clone(),
                model,
                size,
                sector_size: sector,
                removable: read(&format!("{base}/removable")) == "1"
                    || matches!(bus, "USB" | "SD" | "Floppy"),
                bus: bus.into(),
                volumes,
                system,
                media: size > 0,
            });
        }
        Ok(out)
    }

    pub(super) fn open(path: &Path, writable: bool) -> Result<(File, u64, u32, Vec<File>)> {
        let disk = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if writable {
            let busy: Vec<String> = mounts()
                .into_iter()
                .filter(|(d, _)| on_disk(d, &disk) || Path::new(d) == path)
                .map(|(d, m)| format!("{d} on {m}"))
                .collect();
            if busy.iter().any(|b| b.ends_with(" on /")) {
                return Err(Error::Invalid(format!(
                    "{} holds the running operating system; SectorZero never writes to it",
                    path.display()
                )));
            }
            if !busy.is_empty() {
                return Err(Error::Invalid(format!(
                    "{} is mounted ({}); unmount it first",
                    path.display(),
                    busy.join(", ")
                )));
            }
        }
        let mut f = File::options().read(true).write(writable).open(path)?;
        let len = crate::container::file_len(&mut f)?;
        if len == 0 {
            return Err(Error::Invalid(format!(
                "{}: no medium in the drive",
                path.display()
            )));
        }
        let sector = read(&format!("/sys/class/block/{disk}/queue/logical_block_size"))
            .parse()
            .unwrap_or(512);
        Ok((f, len, sector, Vec::new()))
    }

    pub(super) fn after_write(_f: &File) {}

    /// Linux has no volume locks; writing refuses mounted drives instead.
    pub(super) fn lock_for_reading(_path: &Path) -> Result<Vec<File>> {
        Ok(Vec::new())
    }

    /// Unlock the door (SCSI disks lock it while open), then eject: SCSI START STOP UNIT
    /// through CDROMEJECT, which Linux accepts on any SCSI block device, or FDEJECT.
    pub(super) fn eject(path: &Path) -> Result<()> {
        use std::os::fd::AsRawFd;
        const CDROMEJECT: u64 = 0x5309;
        const SCSI_IOCTL_DOORUNLOCK: u64 = 0x5381;
        const FDEJECT: u64 = 0x025A;
        let (f, _, _, _) = open(path, true)?;
        f.sync_all()?;
        let fd = f.as_raw_fd();
        // SAFETY: plain ioctls on an open descriptor; none takes an argument.
        let ok = unsafe {
            libc::ioctl(fd, SCSI_IOCTL_DOORUNLOCK as _, 0);
            libc::ioctl(fd, CDROMEJECT as _, 0) == 0 || libc::ioctl(fd, FDEJECT as _, 0) == 0
        };
        if ok {
            Ok(())
        } else {
            Err(Error::Invalid(format!(
                "{} did not eject its medium ({}); remove it by hand",
                path.display(),
                std::io::Error::last_os_error()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A "device" that rejects anything not aligned to its sector size, like Windows does.
    struct Strict {
        data: Cursor<Vec<u8>>,
        sector: u64,
    }

    impl Read for Strict {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(
                self.data.position().is_multiple_of(self.sector),
                "unaligned read offset"
            );
            assert!(
                (buf.len() as u64).is_multiple_of(self.sector),
                "unaligned read length"
            );
            assert!(
                (buf.as_ptr() as usize).is_multiple_of(4096),
                "unaligned buffer"
            );
            self.data.read(buf)
        }
    }

    impl Write for Strict {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            assert!(
                self.data.position().is_multiple_of(self.sector),
                "unaligned write offset"
            );
            assert!(
                (buf.len() as u64).is_multiple_of(self.sector),
                "unaligned write length"
            );
            self.data.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for Strict {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.data.seek(p)
        }
    }

    #[test]
    fn sector_io_aligns_everything() {
        for sector in [512u64, 4096] {
            let len = 64 * sector;
            let orig: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut io = SectorIo::new(
                Strict {
                    data: Cursor::new(orig.clone()),
                    sector,
                },
                sector,
                len,
            );
            let mut model = orig.clone();
            for (off, n) in [
                (0u64, 1usize),
                (3, 700),
                (511, 2),
                (4095, 9000),
                (len - 5, 5),
                (17, 0),
            ] {
                let mut got = vec![0u8; n];
                io.read_at(off, &mut got).unwrap();
                assert_eq!(got, model[off as usize..off as usize + n]);
                let new: Vec<u8> = (0..n).map(|i| (i * 7 + off as usize) as u8).collect();
                io.write_at(off, &new).unwrap();
                model[off as usize..off as usize + n].copy_from_slice(&new);
            }
            // Past the end reads as zeros; writes are refused.
            let mut tail = vec![1u8; 10];
            io.read_at(len - 4, &mut tail).unwrap();
            assert_eq!(&tail[4..], &[0; 6]);
            assert!(io.write_at(len - 4, &[0; 8]).is_err());
            // Large transfers span several bounce-buffer loads.
            let big: Vec<u8> = (0..len as usize - 100).map(|i| (i % 13) as u8).collect();
            io.write_at(50, &big).unwrap();
            model[50..50 + big.len()].copy_from_slice(&big);
            assert_eq!(io.inner().data.get_ref(), &model);
        }
    }

    #[test]
    fn device_paths() {
        assert!(!is_device_path(Path::new("disk.img")));
        if cfg!(windows) {
            assert!(is_device_path(Path::new(r"\\.\PhysicalDrive1")));
            assert!(is_device_path(Path::new("//./A:")));
        }
    }

    #[test]
    fn listing_works_without_privileges() {
        // Listing only queries devices; it must not fail on a normal account.
        let drives = list_drives().unwrap();
        if cfg!(windows) {
            assert!(drives.iter().any(|d| d.system), "{drives:#?}");
        }
    }
}
