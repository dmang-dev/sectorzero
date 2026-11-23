//! Plain sector images.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use super::{ContainerInfo, ContainerKind, Disk, read_up_to};

pub(crate) struct RawDisk {
    file: File,
    len: u64,
}

impl RawDisk {
    pub(crate) fn open(path: &Path, writable: bool) -> io::Result<Self> {
        let file = File::options().read(true).write(writable).open(path)?;
        let len = file.metadata()?.len();
        Ok(RawDisk { file, len })
    }

    pub(crate) fn create(path: &Path, size: u64) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(size)?;
        Ok(RawDisk { file, len: size })
    }
}

impl Disk for RawDisk {
    fn size(&self) -> u64 {
        self.len
    }

    /// Reads past the end of the file return zeros: WinImage can save images with unused
    /// space truncated, and they must still open at their full size.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let got = if offset < self.len {
            read_up_to(&mut self.file, offset, buf)?
        } else {
            0
        };
        buf[got..].fill(0);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)?;
        self.len = self.len.max(offset + buf.len() as u64);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn info(&self) -> ContainerInfo {
        ContainerInfo::simple(ContainerKind::Raw, "raw", self.len)
    }
}
