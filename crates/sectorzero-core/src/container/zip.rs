//! WinImage IMZ (a ZIP holding one raw image) and WLZ (a ZIP holding a .wil list and
//! several raw images). The selected image is held in memory; on sync the archive is
//! rewritten to a temporary file next to the original and swapped in, copying the other
//! members unchanged and keeping the archive comment (WinImage's image comment).
//!
//! Password-encrypted members (WinImage's own scheme, see `wicrypt`) are read with the
//! password registered for the file (`set_password`). An archive that holds one, or whose
//! image is to be saved encrypted, is written by `write_by_hand`, which copies the other
//! members byte for byte.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{Datelike, Local, Timelike};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::{
    ContainerInfo, ContainerKind, Disk, open_password_for, password_for, replace_file, wicrypt,
};
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
    /// Save the image encrypted with this password.
    password: Option<String>,
    /// Some member of the source archive is encrypted, so the archive is written by hand.
    has_encrypted: bool,
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
        let mut has_encrypted = false;
        for i in 0..zip.len() {
            has_encrypted |= zip.by_index_raw(i).map_err(zip_err)?.encrypted();
        }
        let (encrypted, header, csize, crc, size) = {
            let f = zip.by_index_raw(index).map_err(zip_err)?;
            (
                f.encrypted(),
                f.header_start(),
                f.compressed_size(),
                f.crc32(),
                f.size(),
            )
        };
        if encrypted {
            let mut file = File::open(path)?;
            let (extra, data_at) = local_header(&mut file, header)?;
            if !wicrypt::has_marker(&extra) {
                return Err(Error::Unsupported(
                    "password-protected archives other than WinImage's own encryption \
                     (ZipCrypto, WinZip AES)"
                        .into(),
                ));
            }
            let password = open_password_for(path).ok_or_else(|| {
                Error::PasswordNeeded(
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| name.clone()),
                )
            })?;
            let mut stored = vec![0u8; csize as usize];
            file.seek(SeekFrom::Start(data_at))?;
            file.read_exact(&mut stored)?;
            let data = wicrypt::decrypt(&stored, &password, crc, size)?;
            return Ok(ZipDisk {
                path: path.to_path_buf(),
                data,
                index: Some(index),
                member: name,
                members: images.iter().map(|(_, n)| n.clone()).collect(),
                comment,
                dirty: false,
                wlz,
                password: Some(password),
                has_encrypted,
            });
        }
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
            password: None,
            has_encrypted,
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
            password: password_for(path),
            has_encrypted: false,
        };
        if d.password.is_some() && size > u32::MAX as u64 {
            return Err(Error::Unsupported(
                "encrypted IMZ images of 4 GiB or more".into(),
            ));
        }
        d.sync()?;
        Ok(d)
    }

    fn write_archive(&self) -> io::Result<()> {
        if self.password.is_some() || self.has_encrypted {
            return self.write_by_hand();
        }
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

impl ZipDisk {
    /// Write the archive without the zip crate: the image member is compressed (and
    /// encrypted when a password is set); every other member is copied byte for byte, with
    /// its central directory record.
    fn write_by_hand(&self) -> io::Result<()> {
        let too_big =
            || io::Error::other("images and archives of 4 GiB or more cannot be stored encrypted");
        let image_size = u32::try_from(self.data.len()).map_err(|_| too_big())?;
        let crc = wicrypt::crc32fast_hash(&self.data);
        let (stored, flags, extra) = match &self.password {
            Some(pw) => (
                wicrypt::encrypt(&self.data, pw)?,
                1u16,
                wicrypt::extra_fields(self.data.len() as u64),
            ),
            None => {
                let mut enc =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                enc.write_all(&self.data)?;
                (enc.finish()?, 0u16, Vec::new())
            }
        };
        let stored_len = u32::try_from(stored.len()).map_err(|_| too_big())?;
        let name = self.member.as_bytes();
        let flags = flags | if self.member.is_ascii() { 0 } else { 0x0800 };
        let (time, date) = dos_now();
        // The fields the local and the central header share, from "version needed" on.
        let mut common = Vec::with_capacity(26);
        for v in [10u16, flags, 8, time, date] {
            common.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, stored_len, image_size] {
            common.extend_from_slice(&v.to_le_bytes());
        }
        common.extend_from_slice(&(name.len() as u16).to_le_bytes());
        common.extend_from_slice(&(extra.len() as u16).to_le_bytes());

        let write_member =
            |out: &mut BufWriter<File>, central: &mut Vec<u8>, pos: &mut u64| -> io::Result<()> {
                let offset = u32::try_from(*pos).map_err(|_| too_big())?;
                out.write_all(&0x0403_4b50u32.to_le_bytes())?;
                out.write_all(&common)?;
                out.write_all(name)?;
                out.write_all(&extra)?;
                out.write_all(&stored)?;
                *pos += 30 + name.len() as u64 + extra.len() as u64 + stored.len() as u64;
                central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
                central.extend_from_slice(&10u16.to_le_bytes()); // made by MS-DOS, version 1.0
                central.extend_from_slice(&common);
                central.extend_from_slice(&[0; 6]); // comment length, disk, internal attributes
                central.extend_from_slice(&0x20u32.to_le_bytes()); // archive attribute
                central.extend_from_slice(&offset.to_le_bytes());
                central.extend_from_slice(name);
                central.extend_from_slice(&extra);
                Ok(())
            };

        let tmp = tmp_path(&self.path);
        let result = (|| -> io::Result<()> {
            let mut out = BufWriter::new(File::create(&tmp)?);
            let mut central = Vec::new();
            let mut pos = 0u64;
            let mut count = 0u16;
            match self.index {
                None => {
                    write_member(&mut out, &mut central, &mut pos)?;
                    count = 1;
                }
                Some(selected) => {
                    let mut src = File::open(&self.path)?;
                    let mut zip =
                        ZipArchive::new(File::open(&self.path)?).map_err(io::Error::other)?;
                    let cd_start = zip.central_directory_start();
                    let mut spans = Vec::new();
                    for i in 0..zip.len() {
                        let f = zip.by_index_raw(i).map_err(io::Error::other)?;
                        spans.push((f.header_start(), f.central_header_start()));
                    }
                    let mut starts: Vec<u64> = spans.iter().map(|s| s.0).collect();
                    starts.push(cd_start);
                    starts.sort_unstable();
                    for (i, &(local, cdh)) in spans.iter().enumerate() {
                        count += 1;
                        if i == selected {
                            write_member(&mut out, &mut central, &mut pos)?;
                            continue;
                        }
                        // The local record runs to the next one (data descriptor included).
                        let end = starts
                            .iter()
                            .copied()
                            .find(|&s| s > local)
                            .unwrap_or(cd_start);
                        let offset = u32::try_from(pos).map_err(|_| too_big())?;
                        src.seek(SeekFrom::Start(local))?;
                        pos += io::copy(&mut (&mut src).take(end - local), &mut out)?;
                        let mut head = [0u8; 46];
                        src.seek(SeekFrom::Start(cdh))?;
                        src.read_exact(&mut head)?;
                        if head[42..46] == [0xFF; 4] {
                            return Err(io::Error::other("ZIP64 members cannot be copied"));
                        }
                        let len = [28, 30, 32]
                            .iter()
                            .map(|&o| u16::from_le_bytes([head[o], head[o + 1]]) as usize)
                            .sum();
                        let mut rest = vec![0u8; len];
                        src.read_exact(&mut rest)?;
                        head[42..46].copy_from_slice(&offset.to_le_bytes());
                        central.extend_from_slice(&head);
                        central.extend_from_slice(&rest);
                    }
                }
            }
            let cd_offset = u32::try_from(pos).map_err(|_| too_big())?;
            out.write_all(&central)?;
            let comment = &self.comment.as_bytes()[..self.comment.len().min(0xFFFF)];
            out.write_all(&0x0605_4b50u32.to_le_bytes())?;
            out.write_all(&[0; 4])?;
            out.write_all(&count.to_le_bytes())?;
            out.write_all(&count.to_le_bytes())?;
            out.write_all(&(central.len() as u32).to_le_bytes())?;
            out.write_all(&cd_offset.to_le_bytes())?;
            out.write_all(&(comment.len() as u16).to_le_bytes())?;
            out.write_all(comment)?;
            out.into_inner().map_err(|e| e.into_error())?.sync_all()
        })();
        match result {
            Ok(()) => replace_file(&tmp, &self.path),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

/// A member's local header: its extra field and where its data starts.
fn local_header(file: &mut File, at: u64) -> Result<(Vec<u8>, u64)> {
    let mut h = [0u8; 30];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut h)?;
    if h[..4] != [0x50, 0x4B, 0x03, 0x04] {
        return Err(Error::Invalid("IMZ/WLZ archive: bad local header".into()));
    }
    let n = u16::from_le_bytes([h[26], h[27]]) as u64;
    let m = u16::from_le_bytes([h[28], h[29]]) as usize;
    let mut extra = vec![0u8; m];
    file.seek(SeekFrom::Start(at + 30 + n))?;
    file.read_exact(&mut extra)?;
    Ok((extra, at + 30 + n + m as u64))
}

/// The local time as MS-DOS time and date words.
fn dos_now() -> (u16, u16) {
    let n = Local::now();
    let time = ((n.hour() << 11) | (n.minute() << 5) | (n.second() / 2)) as u16;
    let year = (n.year().clamp(1980, 2107) - 1980) as u32;
    let date = (year << 9 | n.month() << 5 | n.day()) as u16;
    (time, date)
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
            parent: None,
            encrypted: self.password.is_some(),
            compression: None,
        }
    }

    fn set_password(&mut self, password: Option<&str>) -> Result<()> {
        if password.is_some_and(str::is_empty) {
            return Err(Error::Invalid("the password is empty".into()));
        }
        if password.is_some() && self.data.len() as u64 > u32::MAX as u64 {
            return Err(Error::Unsupported(
                "encrypted IMZ images of 4 GiB or more".into(),
            ));
        }
        self.password = password.map(str::to_string);
        super::set_password(&self.path, password);
        self.dirty = true;
        Ok(())
    }
}
