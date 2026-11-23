//! A mounted FAT volume inside an image file, and the WinImage-style operations on it.
//!
//! Paths inside the image use `/` (a `\` is accepted too) and are relative to the root;
//! `""` or `"/"` is the root. Name matching is case-insensitive, as on FAT.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use chrono::{Datelike, Local, TimeZone};
use fatfs::{FatType, FileAttributes, FileSystem, FormatVolumeOptions, FsOptions};
use serde::Serialize;

use crate::disk::{Layout, PartitionInfo, Window, layout_of, read_sector0};
use crate::error::{Error, Result, display};
use crate::format::{FloppyFormat, NewImage};

type FatDir<'a> = fatfs::Dir<'a, Window>;
type FatEntry<'a> = fatfs::DirEntry<'a, Window>;

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    /// Open for modification. Without it every write is refused before touching the file.
    pub writable: bool,
    /// 1-based MBR partition to open. Default: the first FAT partition (or the bare volume).
    pub partition: Option<usize>,
}

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
    /// Local time, `YYYY-MM-DD HH:MM:SS`.
    pub modified: String,
    pub attributes: Attributes,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeInfo {
    pub fat_type: &'static str,
    pub label: String,
    pub volume_id: String,
    pub oem_name: String,
    pub bytes_per_sector: u16,
    pub cluster_size: u32,
    pub total_clusters: u32,
    pub free_clusters: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Byte offset of the volume inside the image file (non-zero for a partition).
    pub volume_offset: u64,
    pub volume_size: u64,
    pub layout: Layout,
    pub partition: Option<PartitionInfo>,
    /// The standard floppy geometry this matches, if any.
    pub floppy_format: Option<&'static str>,
    pub writable: bool,
}

pub struct Volume {
    fs: FileSystem<Window>,
    boot: [u8; 512],
    layout: Layout,
    partition: Option<PartitionInfo>,
    offset: u64,
    size: u64,
    writable: bool,
}

fn le16(s: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([s[at], s[at + 1]])
}
fn le32(s: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([s[at], s[at + 1], s[at + 2], s[at + 3]])
}

/// Volume size declared by a FAT boot sector, in bytes.
fn bpb_volume_bytes(s: &[u8; 512]) -> u64 {
    let total16 = le16(s, 19);
    let total = if total16 != 0 {
        u32::from(total16)
    } else {
        le32(s, 32)
    };
    u64::from(total) * u64::from(le16(s, 11))
}

impl Volume {
    /// Open the FAT volume in an image file.
    pub fn open(path: &Path, opts: OpenOptions) -> Result<Volume> {
        let mut file = File::options().read(true).write(opts.writable).open(path)?;
        let file_len = file.metadata()?.len();
        let sector0 = read_sector0(&mut file)?;
        let layout = layout_of(&sector0);

        let (offset, size, partition) = match &layout {
            Layout::BareVolume => {
                if let Some(n) = opts.partition.filter(|&n| n != 1) {
                    return Err(Error::Invalid(format!(
                        "image has no partition table, so there is no partition {n}"
                    )));
                }
                (0, file_len.max(bpb_volume_bytes(&sector0)), None)
            }
            Layout::Mbr(parts) => {
                let part = match opts.partition {
                    Some(n) => parts
                        .iter()
                        .find(|p| p.index == n)
                        .ok_or_else(|| Error::NotFound(format!("partition {n}")))?,
                    None => parts
                        .iter()
                        .find(|p| p.is_fat)
                        .ok_or_else(|| Error::Unsupported("the MBR has no FAT partition".into()))?,
                };
                (part.offset(), part.size_bytes(), Some(part.clone()))
            }
            Layout::Gpt => {
                return Err(Error::Unsupported(
                    "GPT partitioned images (coming later)".into(),
                ));
            }
            Layout::Unknown => {
                return Err(Error::NotFat(
                    "sector 0 is neither a FAT boot sector nor an MBR partition table".into(),
                ));
            }
        };

        let boot = if offset == 0 {
            sector0
        } else {
            let mut s = [0u8; 512];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut s)?;
            s
        };

        let window = Window::new(file, offset, size, opts.writable);
        let fs = FileSystem::new(window, FsOptions::new().update_accessed_date(false))
            .map_err(|e| Error::NotFat(e.to_string()))?;
        Ok(Volume {
            fs,
            boot,
            layout,
            partition,
            offset,
            size,
            writable: opts.writable,
        })
    }

    /// Create a new, empty, formatted image file.
    pub fn create(path: &Path, spec: NewImage, label: Option<&str>, overwrite: bool) -> Result<()> {
        let size = spec.size_bytes();
        let sectors = size / 512;
        if sectors < 64 {
            return Err(Error::Invalid("image is too small".into()));
        }
        let total_sectors = u32::try_from(sectors)
            .map_err(|_| Error::Invalid("image is too large for FAT".into()))?;
        let label = label.map(encode_label).transpose()?;

        let mut o = File::options();
        o.read(true).write(true);
        if overwrite {
            o.create(true).truncate(true);
        } else {
            o.create_new(true);
        }
        let file = o.open(path).map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => Error::Exists(path.display().to_string()),
            _ => Error::Io(e),
        })?;
        file.set_len(size)?;

        let mut fmt = FormatVolumeOptions::new()
            .bytes_per_sector(512)
            .total_sectors(total_sectors)
            .fats(2)
            .volume_id(new_volume_id());
        fmt = match spec {
            NewImage::Floppy(f) => fmt
                .fat_type(FatType::Fat12)
                .bytes_per_cluster(f.bytes_per_cluster)
                .max_root_dir_entries(f.root_entries)
                .media(f.media)
                .sectors_per_track(f.sectors_per_track)
                .heads(f.heads)
                .drive_num(0x00),
            // WinImage's defaults for hard disk / removable partition images.
            NewImage::Volume { .. } => fmt
                .media(0xF8)
                .sectors_per_track(32)
                .heads(64)
                .drive_num(0x80),
        };
        if let Some(l) = label {
            fmt = fmt.volume_label(l);
        }
        let result = fatfs::format_volume(Window::new(file, 0, size, true), fmt);
        if let Err(e) = result {
            let _ = fs::remove_file(path);
            return Err(Error::Invalid(format!("cannot format: {e}")));
        }
        Ok(())
    }

    pub fn info(&self) -> Result<VolumeInfo> {
        let stats = self.fs.stats()?;
        let s = &self.boot;
        let bytes_per_sector = le16(s, 11);
        let total16 = le16(s, 19);
        let total_sectors = if total16 != 0 {
            u32::from(total16)
        } else {
            le32(s, 32)
        };
        let id = self.fs.volume_id();
        let cluster = u64::from(stats.cluster_size());
        Ok(VolumeInfo {
            fat_type: match self.fs.fat_type() {
                FatType::Fat12 => "FAT12",
                FatType::Fat16 => "FAT16",
                FatType::Fat32 => "FAT32",
            },
            label: self.label()?,
            volume_id: format!("{:04X}-{:04X}", id >> 16, id & 0xFFFF),
            oem_name: String::from_utf8_lossy(&s[3..11]).trim_end().to_string(),
            bytes_per_sector,
            cluster_size: stats.cluster_size(),
            total_clusters: stats.total_clusters(),
            free_clusters: stats.free_clusters(),
            total_bytes: u64::from(stats.total_clusters()) * cluster,
            free_bytes: u64::from(stats.free_clusters()) * cluster,
            volume_offset: self.offset,
            volume_size: self.size,
            layout: self.layout.clone(),
            partition: self.partition.clone(),
            floppy_format: FloppyFormat::matching(
                total_sectors,
                le16(s, 24),
                le16(s, 26),
                s[21],
                u32::from(s[13]) * u32::from(bytes_per_sector),
                le16(s, 17),
            )
            .map(|f| f.name),
            writable: self.writable,
        })
    }

    /// Volume label: the root-directory label entry if present, else the boot sector's.
    pub fn label(&self) -> Result<String> {
        let label = match self.fs.read_volume_label_from_root_dir()? {
            Some(l) => l,
            None => self.fs.volume_label(),
        };
        let label = label.trim_end().to_string();
        Ok(if label == "NO NAME" {
            String::new()
        } else {
            label
        })
    }

    /// Entries of one folder (not recursive), folders first, then by name.
    pub fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let dir = norm(dir)?;
        let mut out = Vec::new();
        for e in self.dir(&dir)?.iter() {
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
        let path = norm(path)?;
        let (parent, _) = split_parent(&path);
        Ok(self.find(&path)?.map(|e| to_entry(&e, parent)))
    }

    /// Copy a file or folder out of the image into `dest` (a host folder, created if missing).
    /// `src` of `""` extracts the whole root. Folders keep their structure.
    pub fn extract(&self, src: &str, dest: &Path, overwrite: bool) -> Result<Stats> {
        let src = norm(src)?;
        fs::create_dir_all(dest)?;
        let mut stats = Stats::default();
        if src.is_empty() {
            self.extract_dir(self.fs.root_dir(), dest, overwrite, &mut stats)?;
        } else {
            let e = self
                .find(&src)?
                .ok_or_else(|| Error::NotFound(display(&src)))?;
            self.extract_entry(&e, dest, overwrite, &mut stats)?;
        }
        Ok(stats)
    }

    fn extract_entry(
        &self,
        e: &FatEntry<'_>,
        dest: &Path,
        overwrite: bool,
        stats: &mut Stats,
    ) -> Result<()> {
        let target = dest.join(host_safe_name(&e.file_name()));
        if e.is_dir() {
            fs::create_dir_all(&target)?;
            stats.dirs += 1;
            self.extract_dir(e.to_dir(), &target, overwrite, stats)
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

    fn extract_dir(
        &self,
        dir: FatDir<'_>,
        dest: &Path,
        overwrite: bool,
        stats: &mut Stats,
    ) -> Result<()> {
        for e in dir.iter() {
            let e = e?;
            if !is_dot(&e) {
                self.extract_entry(&e, dest, overwrite, stats)?;
            }
        }
        Ok(())
    }

    /// Copy a host file or folder into folder `dest_dir` of the image (created if missing).
    pub fn inject(&self, host: &Path, dest_dir: &str, overwrite: bool) -> Result<Stats> {
        self.require_writable()?;
        let dest_dir = norm(dest_dir)?;
        let mut stats = Stats::default();
        self.inject_into(host, &dest_dir, overwrite, &mut stats)?;
        Ok(stats)
    }

    fn inject_into(
        &self,
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
            self.ensure_dir(&img_path)?;
            stats.dirs += 1;
            let mut children: Vec<_> = fs::read_dir(host)?.collect::<io::Result<_>>()?;
            children.sort_by_key(|c| c.file_name());
            for c in children {
                self.inject_into(&c.path(), &img_path, overwrite, stats)?;
            }
            return Ok(());
        }

        let dir = self.ensure_dir(dest_dir)?;
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
                dst.flush_entry()?;
                stats.files += 1;
                stats.bytes += n;
                Ok(())
            }
            Err(e) => {
                drop(dst);
                let _ = dir.remove(&name);
                if e.kind() == io::ErrorKind::Other && e.to_string().contains("No space") {
                    return Err(Error::Invalid(format!(
                        "not enough free space in the image for {}",
                        display(&img_path)
                    )));
                }
                Err(Error::Io(e))
            }
        }
    }

    /// Create a folder, including missing parents. Succeeds if it already exists.
    pub fn mkdir(&self, path: &str) -> Result<()> {
        self.require_writable()?;
        let path = norm(path)?;
        if path.is_empty() {
            return Err(Error::InvalidPath("/".into()));
        }
        self.ensure_dir(&path).map(|_| ())
    }

    /// Delete a file, or a folder (`recursive` needed when it is not empty).
    pub fn remove(&self, path: &str, recursive: bool) -> Result<Stats> {
        self.require_writable()?;
        let path = norm(path)?;
        if path.is_empty() {
            return Err(Error::InvalidPath("cannot remove the root folder".into()));
        }
        let mut stats = Stats::default();
        self.remove_path(&path, recursive, &mut stats)?;
        Ok(stats)
    }

    fn remove_path(&self, path: &str, recursive: bool, stats: &mut Stats) -> Result<()> {
        let e = self
            .find(path)?
            .ok_or_else(|| Error::NotFound(display(path)))?;
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
        self.dir(parent)?
            .remove(&name)
            .map_err(|err| Error::at(path, err))
    }

    /// Rename or move. If `to` is an existing folder, `from` is moved into it.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.require_writable()?;
        let from = norm(from)?;
        let to = norm(to)?;
        let src = self
            .find(&from)?
            .ok_or_else(|| Error::NotFound(display(&from)))?;
        let (src_parent, _) = split_parent(&from);
        let src_name = src.file_name();
        let (dst_dir, dst_name) = match self.find(&to)? {
            Some(t) if t.is_dir() => (to.clone(), src_name.clone()),
            Some(_) => return Err(Error::Exists(display(&to))),
            None if to.is_empty() => (String::new(), src_name.clone()),
            None => {
                let (p, n) = split_parent(&to);
                (p.to_string(), n.to_string())
            }
        };
        let target_dir = self.dir(&dst_dir)?;
        self.dir(src_parent)?
            .rename(&src_name, &target_dir, &dst_name)
            .map_err(|e| Error::at(&from, e))
    }

    /// Flush and unmount, reporting any error (dropping a `Volume` also unmounts, silently).
    pub fn close(self) -> Result<()> {
        self.fs.unmount()?;
        Ok(())
    }

    // --- helpers -------------------------------------------------------------------------

    fn require_writable(&self) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(Error::Invalid("the image is open read-only".into()))
        }
    }

    fn dir(&self, path: &str) -> Result<FatDir<'_>> {
        let root = self.fs.root_dir();
        if path.is_empty() {
            return Ok(root);
        }
        match self.find(path)? {
            Some(e) if e.is_dir() => Ok(e.to_dir()),
            Some(_) => Err(Error::Invalid(format!("not a folder: {}", display(path)))),
            None => Err(Error::NotFound(display(path))),
        }
    }

    fn find(&self, path: &str) -> Result<Option<FatEntry<'_>>> {
        if path.is_empty() {
            return Ok(None);
        }
        let mut dir = self.fs.root_dir();
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

    fn ensure_dir(&self, path: &str) -> Result<FatDir<'_>> {
        let mut dir = self.fs.root_dir();
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
}

trait FlushEntry {
    fn flush_entry(&mut self) -> io::Result<()>;
}
impl FlushEntry for fatfs::File<'_, Window> {
    /// Write the directory entry (size, timestamps) now, so errors surface here.
    fn flush_entry(&mut self) -> io::Result<()> {
        io::Write::flush(self)
    }
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
        attributes: Attributes {
            read_only: a.contains(FileAttributes::READ_ONLY),
            hidden: a.contains(FileAttributes::HIDDEN),
            system: a.contains(FileAttributes::SYSTEM),
            archive: a.contains(FileAttributes::ARCHIVE),
        },
        name,
    }
}

/// Normalise an in-image path: `\` → `/`, drop empty and `.` parts, refuse `..`.
fn norm(p: &str) -> Result<String> {
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

fn split_parent(p: &str) -> (&str, &str) {
    match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// A name read from an image must not escape the destination folder on the host.
fn host_safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '\0') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    match cleaned.trim() {
        "" | "." | ".." => "_".to_string(),
        _ => cleaned,
    }
}

fn encode_label(label: &str) -> Result<[u8; 11]> {
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

fn new_volume_id() -> u32 {
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() as u32).rotate_left(16) ^ d.subsec_nanos()
}

fn format_time(t: fatfs::DateTime) -> String {
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
