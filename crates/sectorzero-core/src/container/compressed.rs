//! Compressed images: a disk image packed with gzip, xz, zstd or bzip2 (`disk.img.gz`,
//! `disk.vhd.xz`, …), a .zip / .7z archive holding one image (Raspberry Pi style
//! downloads), or a tar archive such as an OVA (an OVF descriptor plus its VMDK disks). The image is unpacked into a temporary folder and opened from there as
//! whatever container it is (raw, VHD, QCOW2, …). Saving recompresses it to a temporary file
//! next to the original and swaps that in; other members of a .zip are copied unchanged.
//!
//! The temporary folder is `SECTORZERO_TMPDIR` if set, else the system temporary folder,
//! and it needs room for the whole unpacked image.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::{ContainerInfo, ContainerKind, Disk, replace_file};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    Gzip,
    Xz,
    Zstd,
    Bzip2,
    Zip,
    SevenZip,
    /// tar, including OVA (read-only: an OVA's manifest holds the disks' digests).
    Tar,
}

impl Codec {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Codec::Gzip => "gzip",
            Codec::Xz => "xz",
            Codec::Zstd => "zstd",
            Codec::Bzip2 => "bzip2",
            Codec::Zip => "zip",
            Codec::SevenZip => "7z",
            Codec::Tar => "tar",
        }
    }

    /// Compression recognised by its signature. ZIP (`PK`) is left to the caller, since
    /// IMZ/WLZ images are ZIP files too.
    pub(crate) fn from_magic(head: &[u8]) -> Option<Codec> {
        if head.starts_with(&[0x1f, 0x8b, 0x08]) {
            Some(Codec::Gzip)
        } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
            Some(Codec::Xz)
        } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Some(Codec::Zstd)
        } else if head.len() >= 4 && head.starts_with(b"BZh") && (b'1'..=b'9').contains(&head[3]) {
            Some(Codec::Bzip2)
        } else if head.starts_with(&[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c]) {
            Some(Codec::SevenZip)
        } else if head.len() >= 262 && &head[257..262] == b"ustar" {
            Some(Codec::Tar)
        } else {
            None
        }
    }

    /// Compression implied by a file name (for new files).
    pub(crate) fn from_ext(path: &Path) -> Option<Codec> {
        let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
        Some(match ext.as_str() {
            "gz" | "gzip" => Codec::Gzip,
            "xz" => Codec::Xz,
            "zst" | "zstd" => Codec::Zstd,
            "bz2" | "bzip2" => Codec::Bzip2,
            "zip" => Codec::Zip,
            "7z" => Codec::SevenZip,
            "ova" | "tar" => Codec::Tar,
            _ => return None,
        })
    }

    fn is_archive(self) -> bool {
        matches!(self, Codec::Zip | Codec::SevenZip | Codec::Tar)
    }
}

pub(crate) struct CompressedDisk {
    /// Declared first so it closes its file before the folder below is removed.
    inner: Box<dyn Disk>,
    path: PathBuf,
    codec: Codec,
    inner_path: PathBuf,
    /// .zip / .7z: the member that holds the image, and every image member.
    member: Option<String>,
    members: Vec<String>,
    /// The error to give when changes can't be saved.
    read_only: Option<&'static str>,
    /// The image was created here: there is no archive to copy other members from.
    fresh: bool,
    dirty: bool,
    _dir: tempfile::TempDir,
}

pub(super) fn scratch_dir() -> io::Result<tempfile::TempDir> {
    let base = std::env::var_os("SECTORZERO_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    tempfile::Builder::new()
        .prefix("sectorzero-")
        .tempdir_in(base)
}

/// A decoder for a gzip, xz, zstd or bzip2 stream (other codecs: `None`).
pub(super) fn stream_decoder(
    codec: Codec,
    from: Box<dyn Read + Send>,
) -> io::Result<Option<Box<dyn Read + Send>>> {
    let from = BufReader::with_capacity(1 << 20, from);
    Ok(Some(match codec {
        Codec::Gzip => Box::new(flate2::read::MultiGzDecoder::new(from)),
        Codec::Xz => Box::new(lzma_rust2::XzReader::new(from, true)),
        Codec::Zstd => Box::new(zstd::stream::read::Decoder::with_buffer(from)?),
        Codec::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(from)),
        _ => return Ok(None),
    }))
}

/// The name the unpacked image gets: `disk.img.gz` holds `disk.img`.
fn stem_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "image".into())
}

/// Last path component of an archive member, safe as a file name.
fn member_file_name(member: &str) -> String {
    let base = member.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .map(|c| match c {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    if clean.is_empty() || clean == "." || clean == ".." {
        "image".into()
    } else {
        clean
    }
}

/// Archive members that are documentation or checksums, not disk images.
fn is_image_member(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with('/') {
        return false;
    }
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    !matches!(
        ext,
        "wil"
            | "ovf"
            | "mf"
            | "cert"
            | "txt"
            | "md"
            | "nfo"
            | "diz"
            | "pdf"
            | "htm"
            | "html"
            | "url"
            | "json"
            | "sha1"
            | "sha256"
            | "sha512"
            | "md5"
            | "sfv"
            | "sig"
            | "asc"
            | "jpg"
            | "jpeg"
            | "png"
            | "gif"
    )
}

/// Pick the member `wanted` names (1-based number or name), or the first image.
fn pick_member(images: &[String], wanted: Option<&str>) -> Result<usize> {
    if images.is_empty() {
        return Err(Error::Invalid("the archive contains no disk image".into()));
    }
    match wanted {
        None => Ok(0),
        Some(m) => match m.parse::<usize>() {
            Ok(n) if n >= 1 && n <= images.len() => Ok(n - 1),
            _ => images
                .iter()
                .position(|n| n.eq_ignore_ascii_case(m))
                .ok_or_else(|| Error::NotFound(format!("archive member {m}"))),
        },
    }
}

fn unpack_error(name: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("cannot decompress {name}: {e}"))
}

fn zip_err(e: zip::result::ZipError) -> Error {
    Error::Invalid(format!("not a readable ZIP archive: {e}"))
}

/// Whether a .zip should be opened as an IMZ/WLZ instead (password-encrypted members, or a
/// WLZ list), since those keep their own features.
pub(crate) fn zip_wants_imz(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return true;
    };
    let Ok(mut zip) = ZipArchive::new(file) else {
        return true;
    };
    for i in 0..zip.len() {
        match zip.by_index_raw(i) {
            Ok(f) => {
                if f.encrypted()
                    || f.name()
                        .is_ok_and(|n| n.to_ascii_lowercase().ends_with(".wil"))
                {
                    return true;
                }
            }
            Err(_) => return true,
        }
    }
    false
}

/// Regular files in a tar archive: (name, data offset, size). Handles ustar prefixes, GNU
/// long names and pax `path` records.
fn tar_members(path: &Path) -> Result<Vec<(String, u64, u64)>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut out = Vec::new();
    let mut pos = 0u64;
    let mut long_name: Option<String> = None;
    let bad = || Error::Invalid("damaged tar archive".into());
    let cstr = |b: &[u8]| -> String {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).into_owned()
    };
    while pos + 512 <= len {
        let mut h = [0u8; 512];
        super::read_up_to(&mut f, pos, &mut h)?;
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let size = if h[124] & 0x80 != 0 {
            h[125..136]
                .iter()
                .fold(0u64, |a, &b| (a << 8) | u64::from(b))
        } else {
            let field = cstr(&h[124..136]);
            u64::from_str_radix(field.trim(), 8).map_err(|_| bad())?
        };
        let data = pos + 512;
        if data + size > len {
            return Err(bad());
        }
        let read_text = |f: &mut File| -> Result<String> {
            let mut t = vec![0u8; size.min(1 << 20) as usize];
            super::read_up_to(f, data, &mut t)?;
            Ok(cstr(&t))
        };
        match h[156] {
            b'L' => long_name = Some(read_text(&mut f)?),
            b'x' => {
                let text = read_text(&mut f)?;
                long_name = text
                    .lines()
                    .find_map(|l| l.split_once(" path=").map(|(_, p)| p.to_string()))
                    .or(long_name);
            }
            b'0' | 0 | b'7' => {
                let mut name = cstr(&h[0..100]);
                if &h[257..262] == b"ustar" && h[345] != 0 {
                    name = format!("{}/{}", cstr(&h[345..500]), name);
                }
                out.push((long_name.take().unwrap_or(name), data, size));
            }
            _ => long_name = None,
        }
        pos = data + size.div_ceil(512) * 512;
    }
    Ok(out)
}

fn copy_into(mut from: impl Read, to: &Path, name: &str) -> Result<()> {
    let mut out = BufWriter::with_capacity(1 << 20, File::create(to)?);
    io::copy(&mut from, &mut out).map_err(|e| unpack_error(name, e))?;
    out.flush()?;
    Ok(())
}

impl CompressedDisk {
    pub(crate) fn open(
        path: &Path,
        codec: Codec,
        writable: bool,
        member: Option<&str>,
    ) -> Result<Self> {
        let dir = scratch_dir()?;
        let display = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut chosen = None;
        let mut members = Vec::new();
        let mut read_only = None;
        let inner_path = match codec {
            Codec::Gzip | Codec::Xz | Codec::Zstd | Codec::Bzip2 => {
                if member.is_some() {
                    return Err(Error::Invalid(
                        "--member only applies to archives (IMZ, WLZ, ZIP, 7z)".into(),
                    ));
                }
                let to = dir.path().join(stem_name(path));
                let file = BufReader::with_capacity(1 << 20, File::open(path)?);
                match codec {
                    Codec::Gzip => {
                        copy_into(flate2::read::MultiGzDecoder::new(file), &to, &display)?
                    }
                    Codec::Xz => copy_into(lzma_rust2::XzReader::new(file, true), &to, &display)?,
                    Codec::Zstd => copy_into(
                        zstd::stream::read::Decoder::with_buffer(file)
                            .map_err(|e| unpack_error(&display, e))?,
                        &to,
                        &display,
                    )?,
                    _ => copy_into(bzip2::read::MultiBzDecoder::new(file), &to, &display)?,
                }
                to
            }
            Codec::Zip => {
                let mut zip = ZipArchive::new(File::open(path)?).map_err(zip_err)?;
                let mut images = Vec::new();
                for i in 0..zip.len() {
                    let f = zip.by_index_raw(i).map_err(zip_err)?;
                    let name = f.name().map_err(zip_err)?.into_owned();
                    if f.is_file() && is_image_member(&name) {
                        images.push(name);
                    }
                }
                let pick = pick_member(&images, member)?;
                let name = images[pick].clone();
                let to = dir.path().join(member_file_name(&name));
                let f = zip.by_name(&name).map_err(zip_err)?;
                copy_into(f, &to, &name)?;
                chosen = Some(name);
                members = images;
                to
            }
            Codec::Tar => {
                let files = tar_members(path)?;
                let images: Vec<String> = files
                    .iter()
                    .map(|f| f.0.clone())
                    .filter(|n| is_image_member(n))
                    .collect();
                let pick = pick_member(&images, member)?;
                let name = images[pick].clone();
                let (_, at, size) = files.iter().find(|f| f.0 == name).unwrap();
                let to = dir.path().join(member_file_name(&name));
                let mut file = File::open(path)?;
                std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(*at))?;
                copy_into(BufReader::new(file).take(*size), &to, &name)?;
                read_only =
                    Some("saving changes to an OVA or tar archive (convert the image instead)");
                chosen = Some(name);
                members = images;
                to
            }
            Codec::SevenZip => {
                let mut sz =
                    sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::new(""))
                        .map_err(|e| unpack_error(&display, e))?;
                let files: Vec<String> = sz
                    .archive()
                    .files
                    .iter()
                    .filter(|f| !f.is_directory)
                    .map(|f| f.name().to_string())
                    .collect();
                let images: Vec<String> = files
                    .iter()
                    .filter(|n| is_image_member(n))
                    .cloned()
                    .collect();
                let pick = pick_member(&images, member)?;
                let name = images[pick].clone();
                if files.len() > 1 {
                    read_only = Some(
                        "saving changes to a 7z archive that holds other files too (convert the image instead)",
                    );
                }
                let to = dir.path().join(member_file_name(&name));
                let mut out = Some(BufWriter::with_capacity(1 << 20, File::create(&to)?));
                sz.for_each_entries_with_decision(|entry, reader| {
                    if entry.name() != name {
                        return Ok(sevenz_rust2::EntryDecision::Continue);
                    }
                    if let Some(mut w) = out.take() {
                        io::copy(reader, &mut w)?;
                        w.flush()?;
                    }
                    Ok(sevenz_rust2::EntryDecision::Stop)
                })
                .map_err(|e| unpack_error(&name, e))?;
                chosen = Some(name);
                members = images;
                to
            }
        };
        let inner = super::open(&inner_path, writable, None)?;
        Ok(CompressedDisk {
            inner,
            path: path.to_path_buf(),
            codec,
            inner_path,
            member: chosen,
            members,
            read_only,
            fresh: false,
            dirty: false,
            _dir: dir,
        })
    }

    /// A new, zero-filled image of `kind` packed with `codec`.
    pub(crate) fn create(
        path: &Path,
        codec: Codec,
        kind: ContainerKind,
        size: u64,
        dynamic: bool,
    ) -> Result<Self> {
        if codec == Codec::Tar {
            return Err(Error::Unsupported(
                "creating OVA or tar archives (convert to a .vmdk instead)".into(),
            ));
        }
        let dir = scratch_dir()?;
        let mut name = stem_name(path);
        if codec.is_archive() && Path::new(&name).extension().is_none() {
            name.push_str(".img");
        }
        let inner_path = dir.path().join(&name);
        let inner = super::create(&inner_path, kind, size, dynamic, true)?;
        let mut d = CompressedDisk {
            inner,
            path: path.to_path_buf(),
            codec,
            inner_path,
            member: codec.is_archive().then(|| name.clone()),
            members: if codec.is_archive() {
                vec![name]
            } else {
                vec![]
            },
            read_only: None,
            fresh: true,
            dirty: true,
            _dir: dir,
        };
        d.sync()?;
        Ok(d)
    }

    fn tmp_path(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".s0-tmp");
        self.path.with_file_name(name)
    }

    fn pack(&self, tmp: &Path) -> Result<()> {
        let src = || -> io::Result<BufReader<File>> {
            Ok(BufReader::with_capacity(
                1 << 20,
                File::open(&self.inner_path)?,
            ))
        };
        let out = || -> io::Result<BufWriter<File>> {
            Ok(BufWriter::with_capacity(1 << 20, File::create(tmp)?))
        };
        let name = self
            .inner_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match self.codec {
            Codec::Gzip => {
                let mut w = flate2::GzBuilder::new()
                    .filename(name.as_bytes())
                    .write(out()?, flate2::Compression::default());
                io::copy(&mut src()?, &mut w)?;
                w.finish()?.flush()?;
            }
            Codec::Xz => {
                let mut opts = lzma_rust2::XzOptions::with_preset(6);
                let workers = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
                    .min(16) as u32;
                opts.set_block_size(std::num::NonZeroU64::new(16 << 20));
                let mut w = lzma_rust2::XzWriterMt::new(out()?, opts, workers)?;
                io::copy(&mut src()?, &mut w)?;
                w.finish()?.flush()?;
            }
            Codec::Zstd => {
                let mut w = zstd::stream::write::Encoder::new(out()?, 3)?;
                io::copy(&mut src()?, &mut w)?;
                w.finish()?.flush()?;
            }
            Codec::Bzip2 => {
                let mut w = bzip2::write::BzEncoder::new(out()?, bzip2::Compression::best());
                io::copy(&mut src()?, &mut w)?;
                w.finish()?.flush()?;
            }
            Codec::Zip => self.pack_zip(tmp)?,
            Codec::Tar => unreachable!("tar archives open read-only"),
            Codec::SevenZip => {
                let member = self.member.clone().unwrap_or(name);
                let mut w = sevenz_rust2::ArchiveWriter::create(tmp)
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(&member), Some(src()?))
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                w.finish()?;
            }
        }
        Ok(())
    }

    /// Write the .zip with the image member replaced and every other member copied as is.
    fn pack_zip(&self, tmp: &Path) -> Result<()> {
        let member = self.member.clone().unwrap_or_default();
        let size = std::fs::metadata(&self.inner_path)?.len();
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .large_file(size >= u32::MAX as u64);
        let mut out = ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(tmp)?));
        let put_image = |out: &mut ZipWriter<BufWriter<File>>| -> Result<()> {
            out.start_file(member.as_str(), opts).map_err(zip_err)?;
            io::copy(&mut BufReader::new(File::open(&self.inner_path)?), out)?;
            Ok(())
        };
        if self.fresh {
            put_image(&mut out)?;
        } else {
            let mut old = ZipArchive::new(File::open(&self.path)?).map_err(zip_err)?;
            out.set_raw_comment(old.comment().to_vec().into_boxed_slice())
                .map_err(zip_err)?;
            for i in 0..old.len() {
                let f = old.by_index_raw(i).map_err(zip_err)?;
                if f.name().map_err(zip_err)? == member {
                    drop(f);
                    put_image(&mut out)?;
                } else {
                    out.raw_copy_file(f).map_err(zip_err)?;
                }
            }
        }
        out.finish().map_err(zip_err)?.flush()?;
        Ok(())
    }
}

impl Disk for CompressedDisk {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_at(offset, buf)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if let Some(why) = self.read_only {
            return Err(io::Error::new(io::ErrorKind::Unsupported, why));
        }
        self.dirty = true;
        self.inner.write_at(offset, buf)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.inner.sync()?;
        if !self.dirty {
            return Ok(());
        }
        let tmp = self.tmp_path();
        if let Err(e) = self.pack(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io::Error::other(e.to_string()));
        }
        replace_file(&tmp, &self.path)?;
        self.dirty = false;
        self.fresh = false;
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        let mut info = self.inner.info();
        info.compression = Some(self.codec.name().into());
        if self.codec.is_archive() {
            info.member = self.member.clone();
            info.members = self.members.clone();
        }
        info
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        self.inner.is_allocated(offset, len)
    }
}
