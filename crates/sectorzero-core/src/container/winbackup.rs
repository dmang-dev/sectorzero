//! Windows Backup system images ("Create a system image", `wbadmin start backup`):
//!
//! ```text
//! WindowsImageBackup\<computer>\Backup 2026-11-24 101500\{GUID}.vhdx   (one per disk)
//!                                                       \BackupSpecs.xml, *_Components.xml, …
//!                   \<computer>\Catalog\, SPPMetadataCache\, MediaId
//! ```
//!
//! Each VHDX (VHD before Windows 8) holds one whole disk: its partition table, with the
//! backed-up volumes filled in. Any of these folders opens as an image: the
//! `WindowsImageBackup` folder, a computer's folder, one `Backup …` folder, or the drive or
//! folder that holds `WindowsImageBackup`. Every disk of every backup is a member; without
//! `--member`, the largest disk of the newest backup opens (usually the system disk).
//! Backups open read-only.

use std::io;
use std::path::{Path, PathBuf};

use super::{ContainerInfo, Disk};
use crate::error::{Error, Result};

/// One disk of one backup: a label like `PC/Backup 2026-11-24 101500/{GUID}.vhdx`.
struct BackupDisk {
    label: String,
    path: PathBuf,
    /// "Backup …" folder name, for picking the newest.
    backup: String,
    /// Virtual disk size.
    size: u64,
}

fn is_disk_file(p: &Path) -> bool {
    p.is_file()
        && p.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("vhdx") || e.eq_ignore_ascii_case("vhd"))
}

fn sub_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_backup_folder(p: &Path) -> bool {
    name_of(p).to_ascii_lowercase().starts_with("backup ")
        && std::fs::read_dir(p).is_ok_and(|r| r.flatten().any(|e| is_disk_file(&e.path())))
}

/// The disks of one `Backup …` folder.
fn disks_in(backup: &Path, prefix: &str, out: &mut Vec<BackupDisk>) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(backup)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter(|p| is_disk_file(p))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let backup_name = name_of(backup);
    for f in files {
        out.push(BackupDisk {
            label: format!("{prefix}{backup_name}/{}", name_of(&f)),
            // The disk's size, not the file's: dynamic VHDX files only hold what is used.
            size: super::open(&f, false, None).map(|d| d.size()).unwrap_or(0),
            path: f,
            backup: backup_name.clone(),
        });
    }
}

/// A computer's folder: its `Backup …` folders.
fn disks_of_computer(pc: &Path, prefix: &str, out: &mut Vec<BackupDisk>) {
    for b in sub_dirs(pc).into_iter().filter(|b| is_backup_folder(b)) {
        disks_in(&b, prefix, out);
    }
}

/// Every disk of every backup under `dir`, or `None` when `dir` is not a Windows backup.
fn find_disks(dir: &Path) -> Option<Vec<BackupDisk>> {
    let mut out = Vec::new();
    let name = name_of(dir).to_ascii_lowercase();
    let root = if name == "windowsimagebackup" {
        Some(dir.to_path_buf())
    } else {
        sub_dirs(dir)
            .into_iter()
            .find(|d| name_of(d).eq_ignore_ascii_case("WindowsImageBackup"))
    };
    if let Some(root) = root {
        for pc in sub_dirs(&root) {
            disks_of_computer(&pc, &format!("{}/", name_of(&pc)), &mut out);
        }
    } else if is_backup_folder(dir) {
        disks_in(dir, "", &mut out);
    } else if sub_dirs(dir).iter().any(|d| is_backup_folder(d)) {
        disks_of_computer(dir, "", &mut out);
    } else {
        return None;
    }
    Some(out)
}

pub(crate) fn looks_like_backup(dir: &Path) -> bool {
    find_disks(dir).is_some_and(|d| !d.is_empty())
}

pub(crate) struct WinBackupDisk {
    inner: Box<dyn Disk>,
    members: Vec<String>,
    member: String,
}

impl WinBackupDisk {
    pub(crate) fn open(dir: &Path, member: Option<&str>) -> Result<Self> {
        let disks = find_disks(dir)
            .filter(|d| !d.is_empty())
            .ok_or_else(|| Error::Invalid("no disk images in this Windows backup".into()))?;
        let pick = match member {
            Some(m) => match m.parse::<usize>() {
                Ok(n) if n >= 1 && n <= disks.len() => n - 1,
                _ => disks
                    .iter()
                    .position(|d| {
                        d.label.eq_ignore_ascii_case(m) || name_of(&d.path).eq_ignore_ascii_case(m)
                    })
                    .ok_or_else(|| Error::NotFound(format!("archive member {m}")))?,
            },
            None => {
                let newest = disks.iter().map(|d| d.backup.as_str()).max().unwrap_or("");
                disks
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| d.backup == newest)
                    .max_by_key(|(_, d)| d.size)
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            }
        };
        let inner = super::open(&disks[pick].path, false, None)?;
        Ok(WinBackupDisk {
            inner,
            member: disks[pick].label.clone(),
            members: disks.into_iter().map(|d| d.label).collect(),
        })
    }
}

impl Disk for WinBackupDisk {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_at(offset, buf)
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows backups open read-only (convert the disk to change it)",
        ))
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        let mut info = self.inner.info();
        info.members = self.members.clone();
        info.member = Some(self.member.clone());
        info
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        self.inner.is_allocated(offset, len)
    }
}
