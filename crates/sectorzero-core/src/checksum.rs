//! Checksums of image files, virtual disks and files inside images (WinImage's "Checksum",
//! plus SHA-256). All five are computed in one pass.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use md5::Digest;
use serde::Serialize;

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Checksums {
    pub bytes: u64,
    pub crc32: String,
    pub adler32: String,
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
}

/// Accumulates every checksum at once. Also a `Write` sink, so `io::copy` can feed it.
pub(crate) struct Hasher {
    bytes: u64,
    crc32: crc32fast::Hasher,
    adler: adler2::Adler32,
    md5: md5::Md5,
    sha1: sha1::Sha1,
    sha256: sha2::Sha256,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Hasher {
    pub(crate) fn new() -> Hasher {
        Hasher {
            bytes: 0,
            crc32: crc32fast::Hasher::new(),
            adler: adler2::Adler32::new(),
            md5: md5::Md5::new(),
            sha1: sha1::Sha1::new(),
            sha256: sha2::Sha256::new(),
        }
    }

    pub(crate) fn update(&mut self, b: &[u8]) {
        self.bytes += b.len() as u64;
        self.crc32.update(b);
        self.adler.write_slice(b);
        self.md5.update(b);
        self.sha1.update(b);
        self.sha256.update(b);
    }

    pub(crate) fn finish(self) -> Checksums {
        Checksums {
            bytes: self.bytes,
            crc32: format!("{:08x}", self.crc32.finalize()),
            adler32: format!("{:08x}", self.adler.checksum()),
            md5: hex(&self.md5.finalize()),
            sha1: hex(&self.sha1.finalize()),
            sha256: hex(&self.sha256.finalize()),
        }
    }
}

impl Write for Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Checksums of a file on this computer, byte for byte as stored (e.g. the `.imz` itself).
pub fn checksum_file(path: &Path) -> Result<Checksums> {
    let mut h = Hasher::new();
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        let mut h = Hasher::new();
        h.update(b"abc");
        let c = h.finish();
        assert_eq!(c.bytes, 3);
        assert_eq!(c.crc32, "352441c2");
        assert_eq!(c.adler32, "024d0127");
        assert_eq!(c.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(c.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            c.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
