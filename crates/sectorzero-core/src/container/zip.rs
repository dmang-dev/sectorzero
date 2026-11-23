//! WinImage IMZ (a ZIP holding one raw image) and WLZ (a ZIP holding a .wil list and
//! several raw images). The selected image is held in memory; on sync the archive is
//! rewritten to a temporary file next to the original and swapped in, copying the other
//! members unchanged and keeping the archive comment (WinImage's image comment).

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{Datelike, Local, Timelike};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::{ContainerInfo, ContainerKind, Disk, replace_file};
use crate::error::{Error, Result};

pub(crate) struct ZipDisk {
    path: PathBuf,
    data: Vec<u8>,
    /// Index of the open member in the archive (None for an archive not yet written).
    index: Option<usize>,
    member: String,
    members: Vec<String>,
    comment: String,
    dirty: bool,
    wlz: bool,
}

fn is_image_member(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !lower.ends_with('/') && !lower.ends_with(".wil")
}

impl ZipDisk {
    pub(crate) fn open(path: &Path, _writable: bool, member: Option<&str>) -> Result<Self> {
        let mut zip = ZipArchive::new(File::open(path)?)
            .map_err(|e| Error::Invalid(format!("not a readable IMZ/WLZ archive: {e}")))?;
        let mut names = Vec::new();
        for i in 0..zip.len() {
            let f = zip.by_index_raw(i).map_err(zip_err)?;
            names.push((i, f.name().map_err(zip_err)?.into_owned()));
        }
        let images: Vec<&(usize, String)> =
            names.iter().filter(|(_, n)| is_image_member(n)).collect();
        if images.is_empty() {
            return Err(Error::Invalid("the archive contains no disk image".into()));
        }
        let wlz = images.len() > 1
            || names
                .iter()
                .any(|(_, n)| n.to_ascii_lowercase().ends_with(".wil"));
        let (index, name) = match member {
            None => images[0].clone(),
            Some(m) => match m.parse::<usize>() {
                Ok(n) if n >= 1 && n <= images.len() => images[n - 1].clone(),
                _ => images
                    .iter()
                    .find(|(_, n)| n.eq_ignore_ascii_case(m))
                    .map(|x| (*x).clone())
                    .ok_or_else(|| Error::NotFound(format!("archive member {m}")))?,
            },
        };
        let comment = String::from_utf8_lossy(zip.comment()).into_owned();
        let mut f = zip.by_index(index).map_err(|e| match e {
            zip::result::ZipError::UnsupportedArchive(_)
            | zip::result::ZipError::InvalidPassword => {
                Error::Unsupported("password-protected IMZ/WLZ images".into())
            }
            other => zip_err(other),
        })?;
        if f.encrypted() {
            return Err(Error::Unsupported(
                "password-protected IMZ/WLZ images".into(),
            ));
        }
        let mut data = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut data).map_err(|e| {
            Error::Invalid(format!(
                "cannot decompress {name} (encrypted or damaged): {e}"
            ))
        })?;
        Ok(ZipDisk {
            path: path.to_path_buf(),
            data,
            index: Some(index),
            member: name,
            members: images.iter().map(|(_, n)| n.clone()).collect(),
            comment,
            dirty: false,
            wlz,
        })
    }

    pub(crate) fn create(path: &Path, size: u64) -> Result<Self> {
        Self::create_named(path, size, None, String::new())
    }

    pub(crate) fn create_named(
        path: &Path,
        size: u64,
        member: Option<String>,
        comment: String,
    ) -> Result<Self> {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "IMAGE".into());
        let member = member.unwrap_or_else(|| format!("{}.IMA", stem.to_uppercase()));
        let mut d = ZipDisk {
            path: path.to_path_buf(),
            data: vec![0; size as usize],
            index: None,
            member: member.clone(),
            members: vec![member],
            comment,
            dirty: true,
            wlz: false,
        };
        d.sync()?;
        Ok(d)
    }

    fn write_archive(&self) -> io::Result<()> {
        let tmp = tmp_path(&self.path);
        let result = (|| -> zip::result::ZipResult<()> {
            let mut out = ZipWriter::new(File::create(&tmp)?);
            let opts = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .last_modified_time(zip_now())
                .large_file(self.data.len() as u64 >= 0xFFFF_FFFF);
            match self.index {
                None => {
                    out.start_file(self.member.as_str(), opts)?;
                    out.write_all(&self.data)?;
                }
                Some(selected) => {
                    let mut src = ZipArchive::new(File::open(&self.path)?)?;
                    for i in 0..src.len() {
                        if i == selected {
                            out.start_file(self.member.as_str(), opts)?;
                            out.write_all(&self.data)?;
                        } else {
                            out.raw_copy_file(src.by_index_raw(i)?)?;
                        }
                    }
                }
            }
            if !self.comment.is_empty() {
                out.set_comment(self.comment.as_str())?;
            }
            out.finish()?.sync_all()?;
            Ok(())
        })();
        match result {
            Ok(()) => replace_file(&tmp, &self.path),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(io::Error::other(e))
            }
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".s0-tmp");
    path.with_file_name(name)
}

fn zip_now() -> zip::DateTime {
    let n = Local::now();
    zip::DateTime::from_date_and_time(
        n.year().clamp(1980, 2107) as u16,
        n.month() as u8,
        n.day() as u8,
        n.hour() as u8,
        n.minute() as u8,
        n.second() as u8,
    )
    .unwrap_or_default()
}

fn zip_err(e: zip::result::ZipError) -> Error {
    Error::Invalid(format!("IMZ/WLZ archive: {e}"))
}

impl Disk for ZipDisk {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let len = self.data.len() as u64;
        let start = offset.min(len) as usize;
        let n = (buf.len() as u64).min(len.saturating_sub(offset)) as usize;
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        buf[n..].fill(0);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let end = offset as usize + buf.len();
        if end > self.data.len() {
            self.data.resize(end, 0);
        }
        self.data[offset as usize..end].copy_from_slice(buf);
        self.dirty = true;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        if self.dirty {
            self.write_archive()?;
            if self.index.is_none() {
                self.index = Some(0);
            }
            self.dirty = false;
        }
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        ContainerInfo {
            kind: if self.wlz {
                ContainerKind::Wlz
            } else {
                ContainerKind::Imz
            },
            variant: "zip".into(),
            virtual_size: self.data.len() as u64,
            members: self.members.clone(),
            member: Some(self.member.clone()),
            comment: (!self.comment.is_empty()).then(|| self.comment.clone()),
        }
    }
}
