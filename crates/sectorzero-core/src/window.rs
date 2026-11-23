//! A byte range of a virtual disk presented as a seekable stream (a partition, or the
//! whole disk for a bare volume), which is what the file system code mounts.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::container::SharedDisk;

const SECTOR: u64 = 512;
/// Read cache. Reads are served from it; writes go straight to the container
/// (write-through) so a failed write is reported to the caller instead of lost later.
const CACHE_BYTES: usize = 64 * 1024;

pub(crate) struct Window {
    disk: SharedDisk,
    start: u64,
    len: u64,
    pos: u64,
    writable: bool,
    cache: Vec<u8>,
    cache_start: u64,
    cache_len: usize,
}

impl Window {
    pub(crate) fn new(disk: SharedDisk, start: u64, len: u64, writable: bool) -> Self {
        Window {
            disk,
            start,
            len,
            pos: 0,
            writable,
            cache: vec![0; CACHE_BYTES],
            cache_start: 0,
            cache_len: 0,
        }
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// A second window on the same byte range, with its own (empty) cache.
    pub(crate) fn reopen(&self) -> Window {
        Window::new(self.disk.clone(), self.start, self.len, self.writable)
    }

    fn fill_cache(&mut self, at: u64) -> io::Result<()> {
        let aligned = at - at % SECTOR;
        let want = (self.len - aligned).min(CACHE_BYTES as u64) as usize;
        lock(&self.disk)?.read_at(self.start + aligned, &mut self.cache[..want])?;
        self.cache_start = aligned;
        self.cache_len = want;
        Ok(())
    }

    /// Positional read (bypasses and does not disturb the stream position).
    pub(crate) fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(offset)) as usize;
        let (inside, outside) = buf.split_at_mut(n);
        outside.fill(0);
        if n == 0 {
            return Ok(());
        }
        // Serve from the cache when it covers the range.
        let c_end = self.cache_start + self.cache_len as u64;
        if offset >= self.cache_start && offset + n as u64 <= c_end {
            let o = (offset - self.cache_start) as usize;
            inside.copy_from_slice(&self.cache[o..o + n]);
            return Ok(());
        }
        lock(&self.disk)?.read_at(self.start + offset, inside)
    }

    /// Positional write (bypasses and does not disturb the stream position).
    pub(crate) fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "image is open read-only",
            ));
        }
        if offset + buf.len() as u64 > self.len {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the volume",
            ));
        }
        lock(&self.disk)?.write_at(self.start + offset, buf)?;
        self.update_cache(offset, buf);
        Ok(())
    }

    fn update_cache(&mut self, w_start: u64, buf: &[u8]) {
        let w_end = w_start + buf.len() as u64;
        let (c_start, c_end) = (self.cache_start, self.cache_start + self.cache_len as u64);
        let (o_start, o_end) = (w_start.max(c_start), w_end.min(c_end));
        if o_start < o_end {
            let dst = (o_start - c_start) as usize;
            let src = (o_start - w_start) as usize;
            let len = (o_end - o_start) as usize;
            self.cache[dst..dst + len].copy_from_slice(&buf[src..src + len]);
        }
    }
}

pub(crate) fn lock(
    disk: &SharedDisk,
) -> io::Result<std::sync::MutexGuard<'_, Box<dyn crate::container::Disk>>> {
    disk.lock()
        .map_err(|_| io::Error::other("image state is poisoned after an earlier failure"))
}

impl Read for Window {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let cache_end = self.cache_start + self.cache_len as u64;
        if self.pos < self.cache_start || self.pos >= cache_end {
            self.fill_cache(self.pos)?;
        }
        let offset = (self.pos - self.cache_start) as usize;
        let n = buf.len().min(self.cache_len - offset);
        buf[..n].copy_from_slice(&self.cache[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for Window {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the volume",
            ));
        }
        self.write_at(self.pos, &buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }

    /// Cheap: container metadata is persisted by `Image::flush` / `Volume::close`, so that
    /// an IMZ is not re-packed after every file.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for Window {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(o) => i128::from(o),
            SeekFrom::End(o) => i128::from(self.len) + i128::from(o),
            SeekFrom::Current(o) => i128::from(self.pos) + i128::from(o),
        };
        if target < 0 || target > i128::from(self.len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek outside the volume",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}
