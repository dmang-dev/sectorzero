//! Split raw images: `disk.001`, `disk.002`, … (as written by `split`, FTK Imager, 7-Zip and
//! others) read as one virtual disk, the pieces concatenated in order. Existing sets can be
//! read and written in place; the set cannot grow.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::{ContainerInfo, ContainerKind, Disk, read_up_to};
use crate::error::{Error, Result};

/// The path of piece `n` (1-based) of the set `first` belongs to.
fn part_path(first: &Path, n: usize) -> PathBuf {
    first.with_extension(format!("{n:03}"))
}

/// `path` is the first piece (`.001`) of a set with at least a second piece.
pub(crate) fn is_split_set(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "001") && part_path(path, 2).is_file()
}

struct Part {
    file: File,
    start: u64,
    len: u64,
}

pub(crate) struct SplitDisk {
    parts: Vec<Part>,
    size: u64,
}

impl SplitDisk {
    pub(crate) fn open(path: &Path, writable: bool) -> Result<Self> {
        let mut parts = Vec::new();
        let mut start = 0;
        for n in 1.. {
            let p = part_path(path, n);
            if !p.is_file() {
                break;
            }
            let file = File::options().read(true).write(writable).open(&p)?;
            let len = file.metadata()?.len();
            parts.push(Part { file, start, len });
            start += len;
        }
        if parts.is_empty() {
            return Err(Error::Invalid(format!(
                "{} is not the first piece of a split image",
                path.display()
            )));
        }
        Ok(SplitDisk { parts, size: start })
    }

    /// The piece holding byte `offset` (which must be below the size).
    fn part(&mut self, offset: u64) -> &mut Part {
        let i = self.parts.partition_point(|p| p.start + p.len <= offset);
        &mut self.parts[i]
    }
}

impl Disk for SplitDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            if offset >= self.size {
                buf.fill(0);
                return Ok(());
            }
            let p = self.part(offset);
            let within = offset - p.start;
            let n = (buf.len() as u64).min(p.len - within) as usize;
            let got = read_up_to(&mut p.file, within, &mut buf[..n])?;
            buf[got..n].fill(0);
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    fn write_at(&mut self, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the split image",
            ));
        }
        while !buf.is_empty() {
            let p = self.part(offset);
            let within = offset - p.start;
            let n = (buf.len() as u64).min(p.len - within) as usize;
            p.file.seek(SeekFrom::Start(within))?;
            p.file.write_all(&buf[..n])?;
            offset += n as u64;
            buf = &buf[n..];
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        for p in &mut self.parts {
            p.file.flush()?;
        }
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        ContainerInfo::simple(
            ContainerKind::Split,
            &format!("{} pieces", self.parts.len()),
            self.size,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_read_and_write_as_one_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("disk.001");
        let all: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&first, &all[..4096]).unwrap();
        std::fs::write(tmp.path().join("disk.002"), &all[4096..5000]).unwrap();
        std::fs::write(tmp.path().join("disk.003"), &all[5000..]).unwrap();
        std::fs::write(tmp.path().join("disk.005"), b"not part of the set").unwrap();
        assert!(is_split_set(&first));
        assert!(!is_split_set(&tmp.path().join("disk.002")));

        let mut d = SplitDisk::open(&first, true).unwrap();
        assert_eq!(d.size(), 10_000);
        assert_eq!(d.info().variant, "3 pieces");
        let mut got = vec![0u8; 10_000];
        d.read_at(0, &mut got).unwrap();
        assert_eq!(got, all);
        d.write_at(4000, &[0xEE; 1200]).unwrap();
        assert!(d.write_at(9990, &[1; 20]).is_err());
        d.sync().unwrap();
        drop(d);
        let mut expect = all.clone();
        expect[4000..5200].fill(0xEE);
        let joined: Vec<u8> = (1..=3)
            .flat_map(|n| std::fs::read(part_path(&first, n)).unwrap())
            .collect();
        assert_eq!(joined, expect);
        assert_eq!(std::fs::metadata(&first).unwrap().len(), 4096);
    }
}
