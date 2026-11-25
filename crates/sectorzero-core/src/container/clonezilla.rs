//! Clonezilla and Rescuezilla image folders, read-only.
//!
//! A folder holds the partition list (`parts`), for whole-disk backups the disk list
//! (`disk`), and per disk its partition table as an `sfdisk` dump (`sda-pt.sf`), its first
//! sector (`sda-mbr`), the gap after it (`sda-hidden-data-after-mbr`) and on GPT disks both
//! GPT copies (`sda-gpt-1st`, `sda-gpt-2nd`). Each partition is one image, compressed and
//! split into pieces: `sda1.ext4-ptcl-img.gz.aa`, `.ab`, … (partclone, see
//! [`super::partclone`]), `sda1.dd-img.zst.aa` (every sector), and in old backups
//! `sda1.ntfs-img.*` (ntfsclone) or `sda1.aa` (partimage). Extended partitions keep their
//! boot record in `sda4-ebr`.
//!
//! Whole-disk backups open as the disk, put back together the way a restore would: the
//! saved sectors at the start and end, then each partition's image at its offset; free file
//! system blocks read as zeros. Backups of single partitions open as that partition. Each
//! disk (or partition) is a member. Compressed partitions are unpacked to a temporary file
//! the first time they are read.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::partclone::{Partclone, Stream, looks_like_partclone};
use super::{ContainerInfo, ContainerKind, Disk};
use crate::error::{Error, Result};

fn read_text(p: &Path) -> String {
    std::fs::read(p)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

fn words(p: &Path) -> Vec<String> {
    read_text(p)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

pub(crate) fn looks_like_clonezilla(dir: &Path) -> bool {
    if !dir.join("parts").is_file() {
        return false;
    }
    dir.join("clonezilla-img").is_file()
        || std::fs::read_dir(dir).is_ok_and(|r| {
            r.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.contains("-ptcl-img") || n.ends_with("-pt.sf") || n.contains(".ntfs-img")
            })
        })
}

/// A partition line of an sfdisk dump: (name, start sector, sectors).
fn sfdisk_parts(text: &str) -> Vec<(String, u64, u64)> {
    let mut v = vec![];
    for line in text.lines() {
        let Some((dev, rest)) = line.split_once(" : ") else {
            continue;
        };
        let name = dev.trim().rsplit('/').next().unwrap_or("").to_string();
        let mut start = None;
        let mut size = None;
        for f in rest.split(',') {
            if let Some((k, val)) = f.split_once('=') {
                let val = val.trim().parse::<u64>().ok();
                match k.trim() {
                    "start" => start = val,
                    "size" => size = val,
                    _ => {}
                }
            }
        }
        if let (Some(s), Some(n)) = (start, size)
            && !name.is_empty()
        {
            v.push((name, s, n));
        }
    }
    v
}

fn sfdisk_value(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim().parse().ok())?
    })
}

/// `.aa`, `.ab`, … (two or more letters, not a compressor's extension).
fn split_suffix(name: &str) -> Option<(&str, &str)> {
    let (base, ext) = name.rsplit_once('.')?;
    let known = [
        "gz", "xz", "zst", "bz2", "lz4", "lzo", "lz", "uncomp", "txt", "sf", "info", "log",
    ];
    (ext.len() >= 2 && ext.bytes().all(|b| b.is_ascii_lowercase()) && !known.contains(&ext))
        .then_some((base, ext))
}

/// The pieces of each image of partition `part`, keyed by the image's name.
fn images_of(dir: &Path, part: &str) -> Vec<(String, Vec<PathBuf>)> {
    let prefix = format!("{part}.");
    let mut groups: HashMap<String, Vec<(String, PathBuf)>> = HashMap::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) || !e.path().is_file() {
            continue;
        }
        let (base, piece) = match split_suffix(&name) {
            Some((b, s)) => (b.to_string(), s.to_string()),
            None => (name.clone(), String::new()),
        };
        groups.entry(base).or_default().push((piece, e.path()));
    }
    let mut out: Vec<(String, Vec<PathBuf>)> = groups
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_by(|a, b| (a.0.len(), &a.0).cmp(&(b.0.len(), &b.0)));
            (k, v.into_iter().map(|(_, p)| p).collect())
        })
        .collect();
    out.sort();
    out
}

enum Image {
    Partclone(Partclone, Stream),
    Raw(Stream),
    /// Not readable (the reason).
    Unsupported(String),
    /// No image (swap, extended): zeros.
    Empty,
}

struct Part {
    image: Image,
    size: u64,
}

impl Part {
    fn open(dir: &Path, name: &str, len: Option<u64>) -> Result<Part> {
        let images = images_of(dir, name);
        let image = match images.into_iter().next() {
            None => Image::Empty,
            Some((base, pieces)) => {
                let what = |e: io::Error| super::partclone::open_error(&base, e);
                let stream = Stream::new(pieces).map_err(what)?;
                let mut head = vec![0u8; 512];
                let mut r = stream.reader().map_err(what)?;
                let mut got = 0;
                while got < head.len() {
                    match r.read(&mut head[got..]).map_err(what)? {
                        0 => break,
                        n => got += n,
                    }
                }
                head.truncate(got);
                drop(r);
                if looks_like_partclone(&head) {
                    let pc = Partclone::parse(&mut stream.reader().map_err(what)?).map_err(what)?;
                    Image::Partclone(pc, stream)
                } else if head.starts_with(b"\0ntfsclone-image") || base.contains(".ntfs-img") {
                    Image::Unsupported(format!(
                        "{name}: ntfsclone images (in old Clonezilla backups) are not supported yet"
                    ))
                } else if head.starts_with(b"PaRtImAgE-VoLuMe") || base == name {
                    Image::Unsupported(format!(
                        "{name}: partimage images (in old Clonezilla backups) are not supported yet"
                    ))
                } else if base.contains(".dd-img") {
                    Image::Raw(stream)
                } else {
                    Image::Unsupported(format!("{name}: unknown image type {base}"))
                }
            }
        };
        let mut image = image;
        let size = match (&mut image, len) {
            (_, Some(n)) => n,
            (Image::Partclone(pc, _), None) => pc.device_size,
            (Image::Raw(s), None) => s.len().map_err(|e| super::partclone::open_error(name, e))?,
            (Image::Unsupported(why), None) => return Err(Error::Unsupported(why.clone())),
            (Image::Empty, None) => 0,
        };
        Ok(Part { image, size })
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        match &mut self.image {
            Image::Partclone(pc, s) => pc.read(s, offset, buf),
            Image::Raw(s) => s.read_at(offset, buf),
            Image::Unsupported(why) => Err(io::Error::new(io::ErrorKind::Unsupported, why.clone())),
            Image::Empty => {
                buf.fill(0);
                Ok(())
            }
        }
    }

    fn allocated(&self, offset: u64, len: u64) -> bool {
        match &self.image {
            Image::Partclone(pc, _) => pc.any_used(offset, len),
            Image::Empty => false,
            _ => true,
        }
    }

    fn compression(&self) -> Option<&'static str> {
        match &self.image {
            Image::Partclone(_, s) | Image::Raw(s) => s.compression(),
            _ => None,
        }
    }
}

fn bytes_layer(start: u64, path: &Path) -> Option<Layer> {
    let data = std::fs::read(path).ok().filter(|d| !d.is_empty())?;
    Some(Layer {
        start,
        len: data.len() as u64,
        what: What::Bytes(data),
    })
}

enum What {
    Bytes(Vec<u8>),
    Part(usize),
}

/// Something restored onto the disk; later ones win where they overlap.
struct Layer {
    start: u64,
    len: u64,
    what: What,
}

pub(crate) struct ClonezillaDisk {
    size: u64,
    sector: u32,
    parts: Vec<Part>,
    layers: Vec<Layer>,
    variant: &'static str,
    members: Vec<String>,
    member: String,
}

impl ClonezillaDisk {
    pub(crate) fn open(dir: &Path, member: Option<&str>) -> Result<Self> {
        let disks = words(&dir.join("disk"));
        let parts = words(&dir.join("parts"));
        let members = if disks.is_empty() {
            parts.clone()
        } else {
            disks.clone()
        };
        if members.is_empty() {
            return Err(Error::Invalid(
                "this Clonezilla image lists no disks or partitions".into(),
            ));
        }
        let pick = match member {
            None => 0,
            Some(m) => match m.parse::<usize>() {
                Ok(n) if n >= 1 && n <= members.len() => n - 1,
                _ => members
                    .iter()
                    .position(|x| x == m || x.eq_ignore_ascii_case(m))
                    .ok_or_else(|| Error::NotFound(format!("archive member {m}")))?,
            },
        };
        let name = members[pick].clone();
        let mut disk = ClonezillaDisk {
            size: 0,
            sector: 512,
            parts: vec![],
            layers: vec![],
            variant: if disks.is_empty() {
                "partition"
            } else {
                "disk"
            },
            members: members.clone(),
            member: name.clone(),
        };
        if disks.is_empty() {
            let p = Part::open(dir, &name, None)?;
            disk.size = p.size();
            disk.layers.push(Layer {
                start: 0,
                len: disk.size,
                what: What::Part(0),
            });
            disk.parts.push(p);
            return Ok(disk);
        }

        let file = |suffix: &str| dir.join(format!("{name}{suffix}"));
        let sf = read_text(&file("-pt.sf"));
        let sector = sfdisk_value(&sf, "sector-size")
            .filter(|s| [512, 1024, 2048, 4096].contains(s))
            .unwrap_or(512);
        disk.sector = sector as u32;
        let table = sfdisk_parts(&sf);
        let mut end = 0u64;
        let mut add = |disk: &mut ClonezillaDisk, start: u64, path: PathBuf| {
            if let Some(l) = bytes_layer(start, &path) {
                end = end.max(l.start + l.len);
                disk.layers.push(l);
            }
        };
        add(&mut disk, 0, file("-mbr"));
        add(&mut disk, sector, file("-hidden-data-after-mbr"));
        add(&mut disk, 0, file("-gpt-1st"));
        for (pname, start, _) in &table {
            add(&mut disk, start * sector, dir.join(format!("{pname}-ebr")));
        }
        for (pname, start, count) in &table {
            if !parts.contains(pname) {
                continue;
            }
            let p = Part::open(dir, pname, Some(count * sector))?;
            let i = disk.parts.len();
            end = end.max(start * sector + p.size());
            disk.layers.push(Layer {
                start: start * sector,
                len: p.size(),
                what: What::Part(i),
            });
            disk.parts.push(p);
        }

        // The disk's size: from the GPT header's backup location, the parted listing, the
        // sfdisk dump, or what the pieces cover.
        let gpt1 = std::fs::read(file("-gpt-1st")).unwrap_or_default();
        let s = sector as usize;
        let from_gpt = (gpt1.len() >= s + 48 && &gpt1[s..s + 8] == b"EFI PART")
            .then(|| (u64::from_le_bytes(gpt1[s + 32..s + 40].try_into().unwrap()) + 1) * sector);
        let from_parted = read_text(&file("-pt.parted")).lines().find_map(|l| {
            let rest = l.strip_prefix("Disk")?.trim_start().strip_prefix("/dev/")?;
            let n = rest.split_once(": ")?.1.trim().strip_suffix('s')?;
            n.parse::<u64>().ok().map(|n| n * sector)
        });
        let from_sf = sfdisk_value(&sf, "last-lba").map(|l| (l + 34) * sector);
        let gpt2 = std::fs::read(file("-gpt-2nd")).unwrap_or_default();
        disk.size = from_gpt
            .or(from_parted)
            .or(from_sf)
            .unwrap_or(end + gpt2.len() as u64)
            .max(end);
        if let Some(l) = bytes_layer(
            disk.size.saturating_sub(gpt2.len() as u64),
            &file("-gpt-2nd"),
        ) {
            disk.layers.push(l);
        }
        Ok(disk)
    }

    /// The layer at `pos` and where its run ends, or the next layer start.
    fn find(&self, pos: u64) -> (Option<usize>, u64) {
        let mut limit = u64::MAX;
        for (i, l) in self.layers.iter().enumerate().rev() {
            if pos >= l.start && pos < l.start + l.len {
                return (Some(i), (l.start + l.len).min(limit));
            }
            if l.start > pos {
                limit = limit.min(l.start);
            }
        }
        (None, limit)
    }
}

impl Disk for ClonezillaDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset + buf.len() as u64;
        let mut pos = offset;
        while pos < end {
            let (hit, stop) = self.find(pos);
            let stop = stop.min(end);
            let dst = &mut buf[(pos - offset) as usize..(stop - offset) as usize];
            match hit {
                None => dst.fill(0),
                Some(i) => {
                    let l = &self.layers[i];
                    let rel = pos - l.start;
                    match &l.what {
                        What::Bytes(d) => {
                            dst.copy_from_slice(&d[rel as usize..rel as usize + dst.len()])
                        }
                        &What::Part(p) => self.parts[p].read(rel, dst)?,
                    }
                }
            }
            pos = stop;
        }
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "partclone and Clonezilla images open read-only (convert the image to change it)",
        ))
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        let mut info = ContainerInfo::simple(ContainerKind::Clonezilla, self.variant, self.size);
        if self.members.len() > 1 {
            info.members = self.members.clone();
        }
        info.member = Some(self.member.clone());
        info.compression = self
            .parts
            .iter()
            .find_map(Part::compression)
            .map(str::to_string);
        info
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let end = offset.saturating_add(len).min(self.size);
        let mut pos = offset;
        while pos < end {
            let (hit, stop) = self.find(pos);
            let stop = stop.min(end);
            if let Some(i) = hit {
                let l = &self.layers[i];
                match l.what {
                    What::Bytes(_) => return Ok(true),
                    What::Part(p) => {
                        if self.parts[p].allocated(pos - l.start, stop - pos) {
                            return Ok(true);
                        }
                    }
                }
            }
            pos = stop.max(pos + 1);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sfdisk_dump_and_split_names() {
        let sf = "label: gpt\nlabel-id: X\ndevice: /dev/nvme0n1\nunit: sectors\nfirst-lba: 34\nlast-lba: 41943006\nsector-size: 512\n\n/dev/nvme0n1p1 : start=        2048, size=      204800, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, uuid=1, name=\"EFI\"\n/dev/nvme0n1p2 : start=      206848, size=    41736159, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4\n";
        assert_eq!(
            sfdisk_parts(sf),
            vec![
                ("nvme0n1p1".into(), 2048, 204800),
                ("nvme0n1p2".into(), 206848, 41736159)
            ]
        );
        assert_eq!(sfdisk_value(sf, "last-lba"), Some(41943006));
        assert_eq!(
            split_suffix("sda1.ext4-ptcl-img.gz.ab"),
            Some(("sda1.ext4-ptcl-img.gz", "ab"))
        );
        assert_eq!(split_suffix("sda1.ext4-ptcl-img.gz"), None);
        assert_eq!(split_suffix("sda1.aa"), Some(("sda1", "aa")));
    }
}
