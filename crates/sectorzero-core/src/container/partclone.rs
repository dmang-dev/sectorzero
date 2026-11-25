//! Partclone images (the partition images inside Clonezilla and Rescuezilla backups),
//! read-only, from partclone's published description of its image formats
//! (IMAGE_FORMATS.md). No partclone code was used.
//!
//! An image is a header, a bitmap of the file system's used blocks, then the used blocks
//! in order. Format 0001 (partclone before 0.3): a 4160-byte header, one byte per block
//! in the bitmap, the bitmap signature `BiTmAgIc`, and a CRC32 after every block.
//! Format 0002: a 110-byte header (an endianness mark, the checksum mode, the checksum
//! size and how many blocks one checksum covers), one bit per block, the bitmap's CRC32,
//! then the blocks with a checksum after every `blocks per checksum` of them. Free blocks
//! read as zeros.
//!
//! The stream is usually compressed and split into pieces (`.aa`, `.ab`, …): [`Stream`]
//! joins the pieces and, for a compressed stream, unpacks it to a temporary file the
//! first time a block is read.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::compressed::{Codec, scratch_dir, stream_decoder};
use super::{ContainerInfo, ContainerKind, Disk};
use crate::error::{Error, Result};

const MAGIC: &[u8] = b"partclone-image";
const V1_HEADER: usize = 4160;
const V2_HEADER: usize = 110;
const BITMAP_MAGIC: &[u8; 8] = b"BiTmAgIc";

pub(crate) fn looks_like_partclone(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ---------------------------------------------------------------------------------------
// The byte stream: split pieces, maybe compressed

/// Pieces of one file read one after the other.
struct Joined {
    files: std::vec::IntoIter<PathBuf>,
    cur: Option<File>,
}

impl Read for Joined {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.cur.is_none() {
                match self.files.next() {
                    Some(p) => self.cur = Some(File::open(p)?),
                    None => return Ok(0),
                }
            }
            let n = self.cur.as_mut().unwrap().read(buf)?;
            if n > 0 || buf.is_empty() {
                return Ok(n);
            }
            self.cur = None;
        }
    }
}

enum State {
    /// Uncompressed pieces: (file, start in the stream, length).
    Plain(Vec<(File, u64, u64)>),
    /// Compressed, not unpacked yet.
    Packed,
    Unpacked {
        file: File,
        _dir: tempfile::TempDir,
    },
}

/// One logical stream stored as split, maybe compressed, files.
pub(crate) struct Stream {
    parts: Vec<PathBuf>,
    codec: Option<Codec>,
    state: State,
}

impl Stream {
    pub(crate) fn new(parts: Vec<PathBuf>) -> io::Result<Stream> {
        let mut head = [0u8; 16];
        let n = File::open(&parts[0])?.read(&mut head)?;
        let head = &head[..n];
        if head.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "lz4-compressed Clonezilla images (recompress them with gzip or zstd)",
            ));
        }
        if head.starts_with(&[0x89, b'L', b'Z', b'O']) || head.starts_with(b"LZIP") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "lzop- or lzip-compressed Clonezilla images (recompress them with gzip or zstd)",
            ));
        }
        let codec = Codec::from_magic(head)
            .filter(|c| matches!(c, Codec::Gzip | Codec::Xz | Codec::Zstd | Codec::Bzip2));
        let state = match codec {
            Some(_) => State::Packed,
            None => {
                let mut v = vec![];
                let mut at = 0;
                for p in &parts {
                    let f = File::open(p)?;
                    let len = f.metadata()?.len();
                    v.push((f, at, len));
                    at += len;
                }
                State::Plain(v)
            }
        };
        Ok(Stream {
            parts,
            codec,
            state,
        })
    }

    pub(crate) fn compression(&self) -> Option<&'static str> {
        self.codec.map(Codec::name)
    }

    /// The stream from its start, decoded.
    pub(crate) fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        let joined: Box<dyn Read + Send> = Box::new(Joined {
            files: self.parts.clone().into_iter(),
            cur: None,
        });
        Ok(match self.codec {
            Some(c) => stream_decoder(c, joined)?.unwrap(),
            None => joined,
        })
    }

    /// Length of the decoded stream (unpacks a compressed one).
    pub(crate) fn len(&mut self) -> io::Result<u64> {
        self.unpack()?;
        Ok(match &self.state {
            State::Plain(files) => files.iter().map(|f| f.2).sum(),
            State::Unpacked { file, .. } => file.metadata()?.len(),
            State::Packed => 0,
        })
    }

    fn unpack(&mut self) -> io::Result<()> {
        if matches!(self.state, State::Packed) {
            let dir = scratch_dir()?;
            let path = dir.path().join("stream");
            let mut out = io::BufWriter::with_capacity(1 << 20, File::create(&path)?);
            io::copy(&mut self.reader()?, &mut out)
                .map_err(|e| bad(format!("damaged compressed image: {e}")))?;
            drop(out);
            self.state = State::Unpacked {
                file: File::open(&path)?,
                _dir: dir,
            };
        }
        Ok(())
    }

    /// Bytes past the end read as zeros.
    pub(crate) fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.unpack()?;
        buf.fill(0);
        match &mut self.state {
            State::Unpacked { file: f, .. } => {
                f.seek(SeekFrom::Start(offset))?;
                let mut got = 0;
                while got < buf.len() {
                    match f.read(&mut buf[got..])? {
                        0 => break,
                        n => got += n,
                    }
                }
            }
            State::Plain(files) => {
                let end = offset + buf.len() as u64;
                for (f, start, len) in files.iter_mut() {
                    let (s, e) = ((*start).max(offset), (*start + *len).min(end));
                    if s >= e {
                        continue;
                    }
                    f.seek(SeekFrom::Start(s - *start))?;
                    f.read_exact(&mut buf[(s - offset) as usize..(e - offset) as usize])?;
                }
            }
            State::Packed => unreachable!(),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// The partclone format

pub(crate) struct Partclone {
    pub(crate) fs: String,
    pub(crate) version: u8,
    /// Size of the file system in bytes.
    pub(crate) device_size: u64,
    pub(crate) block_size: u64,
    total: u64,
    /// Used-block bitmap, bit `i % 64` of word `i / 64` for block `i`.
    words: Vec<u64>,
    /// Used blocks before each group of 8 words.
    ranks: Vec<u64>,
    data_start: u64,
    /// Bytes after each block (format 0001: its CRC32).
    per_block: u64,
    /// Format 0002: one checksum of this size after every `per_sum` blocks.
    sum_size: u64,
    per_sum: u64,
}

fn text(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

impl Partclone {
    /// Read the header and bitmap from the start of the stream.
    pub(crate) fn parse(r: &mut dyn Read) -> io::Result<Partclone> {
        let mut h = vec![0u8; V2_HEADER];
        r.read_exact(&mut h)
            .map_err(|_| bad("not a partclone image (too short)"))?;
        if !looks_like_partclone(&h) {
            return Err(bad("not a partclone image"));
        }
        let u16_at = |o: usize| u16::from_le_bytes(h[o..o + 2].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
        let version = &h[30..34];
        let mut pc = if version == b"0002" {
            if u16_at(34) != 0xC0DE {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "big-endian partclone images",
                ));
            }
            let mode = u16_at(96);
            Partclone {
                fs: text(&h[36..52]),
                version: 2,
                device_size: u64_at(52),
                total: u64_at(60),
                block_size: u32_at(84) as u64,
                words: vec![],
                ranks: vec![],
                data_start: 0,
                per_block: 0,
                sum_size: if mode == 0 { 0 } else { u16_at(98) as u64 },
                per_sum: (u32_at(100) as u64).max(1),
            }
        } else if version == b"0001" {
            let mut rest = vec![0u8; V1_HEADER - V2_HEADER];
            r.read_exact(&mut rest)
                .map_err(|_| bad("damaged partclone image: header cut short"))?;
            Partclone {
                fs: text(&h[15..30]),
                version: 1,
                block_size: u32_at(36) as u64,
                device_size: u64_at(40),
                total: u64_at(48),
                words: vec![],
                ranks: vec![],
                data_start: 0,
                per_block: 4,
                sum_size: 0,
                per_sum: 1,
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("partclone image format {}", text(version)),
            ));
        };
        if !(512..=1 << 26).contains(&pc.block_size)
            || pc.total > (1 << 40)
            || pc.total.saturating_mul(pc.block_size) < pc.device_size.saturating_sub(pc.block_size)
        {
            return Err(bad("damaged partclone image header"));
        }
        let cut = || bad("damaged partclone image: bitmap cut short");
        let nwords = pc.total.div_ceil(64) as usize;
        let mut words = vec![0u64; nwords];
        if pc.version == 2 {
            let len = pc.total.div_ceil(8) as usize;
            let mut chunk = vec![0u8; 1 << 20];
            let mut done = 0;
            while done < len {
                let n = chunk.len().min(len - done);
                r.read_exact(&mut chunk[..n]).map_err(|_| cut())?;
                for (i, &b) in chunk[..n].iter().enumerate() {
                    let at = done + i;
                    words[at / 8] |= (b as u64) << ((at % 8) * 8);
                }
                done += n;
            }
            let mut crc = [0u8; 4];
            r.read_exact(&mut crc).map_err(|_| cut())?;
            pc.data_start = (V2_HEADER + len + 4) as u64;
        } else {
            let mut chunk = vec![0u8; 1 << 20];
            let mut done = 0u64;
            while done < pc.total {
                let n = (chunk.len() as u64).min(pc.total - done) as usize;
                r.read_exact(&mut chunk[..n]).map_err(|_| cut())?;
                for (i, &b) in chunk[..n].iter().enumerate() {
                    if b == 1 {
                        let at = done as usize + i;
                        words[at / 64] |= 1 << (at % 64);
                    }
                }
                done += n as u64;
            }
            let mut m = [0u8; 8];
            r.read_exact(&mut m).map_err(|_| cut())?;
            if &m != BITMAP_MAGIC {
                return Err(bad("damaged partclone image: no bitmap signature"));
            }
            pc.data_start = V1_HEADER as u64 + pc.total + 8;
        }
        // Bits past the last block are not blocks.
        if pc.total % 64 != 0
            && let Some(last) = words.last_mut()
        {
            *last &= (1u64 << (pc.total % 64)) - 1;
        }
        let mut ranks = Vec::with_capacity(nwords / 8 + 1);
        let mut n = 0u64;
        for g in words.chunks(8) {
            ranks.push(n);
            n += g.iter().map(|w| w.count_ones() as u64).sum::<u64>();
        }
        pc.words = words;
        pc.ranks = ranks;
        Ok(pc)
    }

    fn used(&self, block: u64) -> bool {
        block < self.total && self.words[(block / 64) as usize] >> (block % 64) & 1 != 0
    }

    /// Where a used block is in the stream.
    fn locate(&self, block: u64) -> Option<u64> {
        if !self.used(block) {
            return None;
        }
        let w = (block / 64) as usize;
        let group = w / 8;
        let before = self.ranks[group]
            + self.words[group * 8..w]
                .iter()
                .map(|x| x.count_ones() as u64)
                .sum::<u64>()
            + (self.words[w] & ((1u64 << (block % 64)) - 1)).count_ones() as u64;
        Some(
            self.data_start
                + before * (self.block_size + self.per_block)
                + (before / self.per_sum) * self.sum_size,
        )
    }

    /// Read the file system's bytes at `offset`; free blocks are zeros.
    pub(crate) fn read(&self, s: &mut Stream, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let bs = self.block_size;
        let end = offset + buf.len() as u64;
        let mut pos = offset;
        while pos < end {
            let block = pos / bs;
            let in_block = pos % bs;
            let n = (bs - in_block).min(end - pos);
            let dst = &mut buf[(pos - offset) as usize..(pos - offset + n) as usize];
            match self.locate(block) {
                Some(at) => s.read_at(at + in_block, dst)?,
                None => dst.fill(0),
            }
            pos += n;
        }
        Ok(())
    }

    /// Whether any block in the range holds data.
    pub(crate) fn any_used(&self, offset: u64, len: u64) -> bool {
        let bs = self.block_size;
        let first = offset / bs;
        let last = (offset + len.max(1) - 1) / bs;
        (first..=last.min(self.total.saturating_sub(1))).any(|b| self.used(b))
    }
}

// ---------------------------------------------------------------------------------------
// A partclone image opened on its own

pub(crate) struct PartcloneDisk {
    pc: Partclone,
    stream: Stream,
}

impl PartcloneDisk {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let name = path.display().to_string();
        let what = |e| open_error(&name, e);
        let stream = Stream::new(vec![path.to_path_buf()]).map_err(what)?;
        let pc = Partclone::parse(&mut stream.reader().map_err(what)?).map_err(what)?;
        Ok(PartcloneDisk { pc, stream })
    }
}

impl Disk for PartcloneDisk {
    fn size(&self) -> u64 {
        self.pc.device_size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.pc.read(&mut self.stream, offset, buf)
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
        ContainerInfo::simple(
            ContainerKind::Partclone,
            &format!("v{} {}", self.pc.version, self.pc.fs),
            self.pc.device_size,
        )
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        Ok(self.pc.any_used(offset, len))
    }
}

/// `Error` for a partclone stream that could not be opened.
pub(crate) fn open_error(name: &str, e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::Unsupported => Error::Unsupported(e.to_string()),
        _ => Error::Invalid([name, ": ", &e.to_string()].concat()),
    }
}
