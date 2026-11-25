//! EnCase / Expert Witness evidence files (EWF), read-only: `.E01` images (EnCase, FTK Imager,
//! ewfacquire, linen) and SMART `.s01` images, with their further segments `.E02` … `.E99`,
//! `.EAA` ….
//!
//! Written from the libyal format document "Expert Witness Compression Format (EWF)"
//! (documentation only; no libewf code was read).
//!
//! Each segment file starts with a 13-byte header (signature, segment number) and is a chain
//! of sections, each with a 76-byte descriptor: type, offset of the next section, size and an
//! Adler-32. The `volume` (or `disk`) section gives the chunk size, sector size and sector
//! count. `table` sections list the chunks of the `sectors` data before them: 32-bit offsets
//! (relative to the table's base offset in EnCase 6 and later), the top bit marking a zlib
//! compressed chunk; a stored chunk carries its own Adler-32. `table2` is a copy of the table
//! before it. A segment ends with `next` (more segments follow) or `done` (the last one).

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::{ContainerInfo, ContainerKind, Disk, file_len, read_up_to};
use crate::error::{Error, Result};

/// EnCase / FTK / ewfacquire / SMART segment files.
const EVF: &[u8; 8] = b"EVF\x09\x0d\x0a\xff\x00";
/// EnCase logical evidence files (`.L01`): files, not a disk.
const LVF: &[u8; 8] = b"LVF\x09\x0d\x0a\xff\x00";
/// EnCase 7 `.Ex01` and `.Lx01` (EWF version 2).
const EVF2: &[u8; 8] = b"EVF2\x0d\x0a\x81\x00";
const LEF2: &[u8; 8] = b"LEF2\x0d\x0a\x81\x00";

/// Section descriptor size.
const DESC: u64 = 76;
/// Largest chunk accepted (EnCase allows up to 32768 sectors of 512 bytes; leave room for
/// 4096-byte sectors).
const MAX_CHUNK: u64 = 128 << 20;
/// Most segment files a set can have (`.E01` … `.ZZZ`).
const MAX_SEGMENTS: u32 = 14295;

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn adler(data: &[u8]) -> u32 {
    let mut a = adler2::Adler32::new();
    a.write_slice(data);
    a.checksum()
}

/// A stored checksum matches, or is 0 (some Logicube writers leave it out).
fn checksum_ok(data: &[u8], stored: u32) -> bool {
    stored == 0 || adler(data) == stored
}

fn damaged() -> Error {
    Error::Invalid("damaged EnCase image".into())
}

fn damaged_chunk() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "damaged chunk in the EnCase image",
    )
}

pub(crate) fn looks_like_ewf(head: &[u8]) -> bool {
    [EVF, LVF, EVF2, LEF2].iter().any(|s| head.starts_with(*s))
}

/// The name of segment `n` (1-based) of the set whose first file is `first`: `.E01` … `.E99`,
/// then `.EAA` … `.EZZ`, `.FAA` … (the case of the first extension is kept).
fn segment_path(first: &Path, n: u32) -> Option<PathBuf> {
    let ext = first.extension()?.to_str()?;
    let c0 = *ext.as_bytes().first()?;
    let lower = c0.is_ascii_lowercase();
    let name = if n <= 99 {
        format!("{}{n:02}", c0 as char)
    } else {
        let i = n - 100;
        let lead = c0.to_ascii_uppercase() + (i / 676) as u8;
        if lead > b'Z' {
            return None;
        }
        let s: String = [lead, b'A' + (i / 26 % 26) as u8, b'A' + (i % 26) as u8]
            .iter()
            .map(|&c| c as char)
            .collect();
        if lower { s.to_ascii_lowercase() } else { s }
    };
    Some(first.with_extension(name))
}

#[derive(Clone, Copy)]
struct Chunk {
    /// Index into the segment files.
    segment: u32,
    offset: u64,
    /// Bytes stored (a stored chunk includes its 4-byte checksum).
    stored: u64,
    compressed: bool,
}

/// One parsed `table` / `table2`: chunk offsets and compression flags.
fn parse_table(file: &mut File, at: u64, size: u64) -> Result<Option<Vec<(u64, bool)>>> {
    let mut h = [0u8; 24];
    if read_up_to(file, at + DESC, &mut h)? != h.len() {
        return Ok(None);
    }
    if !checksum_ok(&h[..20], le32(&h, 20)) {
        return Ok(None);
    }
    let count = le32(&h, 0) as u64;
    let base = le64(&h, 8);
    if count > 1 << 24 || (size >= DESC + 24 && DESC + 24 + count * 4 > size) {
        return Ok(None);
    }
    let mut raw = vec![0u8; (count * 4 + 4) as usize];
    let got = read_up_to(file, at + DESC + 24, &mut raw)?;
    if got < count as usize * 4 {
        return Ok(None);
    }
    let (entries, footer) = raw.split_at(count as usize * 4);
    let footer_ok = got == raw.len() && checksum_ok(entries, le32(footer, 0));
    let mut out = Vec::with_capacity(count as usize);
    // EnCase 6.7.1 let offsets past 2 GiB run into the compression bit. Once the 31-bit
    // offsets go backwards, read all 32 bits as the offset (and the chunks as stored).
    let mut overflow = false;
    let mut prev = 0u64;
    for e in entries
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
    {
        let low = base + (e & 0x7fff_ffff) as u64;
        if !overflow && low < prev && base + e as u64 >= prev {
            overflow = true;
        }
        let (offset, compressed) = if overflow {
            (base + e as u64, false)
        } else {
            (low, e & 0x8000_0000 != 0)
        };
        out.push((offset, compressed));
        prev = offset;
    }
    // SMART tables have no footer: their chunks start right after the entries.
    let entries_end = at + DESC + 24 + count * 4;
    if !footer_ok && out.first().is_none_or(|c| c.0 != entries_end) {
        return Ok(None);
    }
    Ok(Some(out))
}

/// Media geometry from a `volume`, `disk` or `data` section.
struct Media {
    chunk_size: u64,
    sector_size: u32,
    size: u64,
    smart: bool,
}

fn parse_volume(d: &[u8]) -> Result<Media> {
    if d.len() < 94 {
        return Err(damaged());
    }
    // The 94-byte SMART layout ends in "SMART" and has a 32-bit sector count; the 1052-byte
    // EnCase layout starts with the media type and has a 64-bit one.
    let smart = d.len() < 1052 || &d[85..90] == b"SMART";
    if !smart && d[0] == 0x0e {
        return Err(logical());
    }
    let (spc, bps) = (le32(d, 8) as u64, le32(d, 12));
    let sectors = if smart {
        le32(d, 16) as u64
    } else {
        le64(d, 16)
    };
    let chunk_size = spc * bps as u64;
    if bps == 0 || !bps.is_power_of_two() || chunk_size == 0 || chunk_size > MAX_CHUNK {
        return Err(damaged());
    }
    Ok(Media {
        chunk_size,
        sector_size: bps,
        size: sectors * bps as u64,
        smart,
    })
}

fn logical() -> Error {
    Error::Unsupported(
        "EnCase logical evidence files (.L01), which hold files rather than a disk".into(),
    )
}

pub(crate) struct EwfDisk {
    files: Vec<File>,
    chunks: Vec<Chunk>,
    chunk_size: u64,
    sector_size: u32,
    size: u64,
    smart: bool,
    /// The last chunk decoded (number, data).
    cache: Option<(usize, Vec<u8>)>,
}

impl EwfDisk {
    pub(crate) fn open(path: &Path) -> Result<EwfDisk> {
        let mut h = [0u8; 13];
        let mut first = File::open(path)?;
        read_up_to(&mut first, 0, &mut h)?;
        if h.starts_with(LVF) || h.starts_with(LEF2) {
            return Err(logical());
        }
        if h.starts_with(EVF2) {
            return Err(Error::Unsupported(
                "EnCase 7 Ex01 evidence files (EWF version 2)".into(),
            ));
        }
        if !h.starts_with(EVF) {
            return Err(damaged());
        }
        if u16::from_le_bytes([h[9], h[10]]) != 1 {
            return Err(Error::Invalid(
                "this is a later segment of an EnCase image; open the .E01 file".into(),
            ));
        }

        let mut files = Vec::new();
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut media: Option<Media> = None;
        let mut file = first;
        let mut n = 1u32;
        loop {
            let len = file_len(&mut file)?;
            let segment = files.len() as u32;
            // Every section start, to find where the last chunk before a section ends.
            let mut starts = vec![len];
            // Chunk offsets of this segment, in table order: (offset, compressed, table).
            let mut entries: Vec<(u64, bool, usize)> = Vec::new();
            let mut tables = 0usize;
            // Whether the last `table` could not be read (then its `table2` copy is used).
            let mut want_copy = false;
            let mut at = 13u64;
            let ends = loop {
                let mut d = [0u8; DESC as usize];
                if read_up_to(&mut file, at, &mut d)? != d.len()
                    || !checksum_ok(&d[..72], le32(&d, 72))
                {
                    return Err(damaged());
                }
                starts.push(at);
                let kind = d[..16].split(|&b| b == 0).next().unwrap_or_default();
                let (next, size) = (le64(&d, 16), le64(&d, 24));
                match kind {
                    b"volume" | b"disk" | b"data" if media.is_none() => {
                        let mut v = vec![0u8; size.saturating_sub(DESC).clamp(94, 1052) as usize];
                        let got = read_up_to(&mut file, at + DESC, &mut v)?;
                        v.truncate(got);
                        media = Some(parse_volume(&v)?);
                    }
                    b"table" | b"table2" => {
                        let copy = kind == b"table2";
                        if !copy || want_copy {
                            match parse_table(&mut file, at, size)? {
                                Some(t) => {
                                    entries.extend(t.into_iter().map(|(o, c)| (o, c, tables)));
                                    tables += 1;
                                    want_copy = false;
                                }
                                None if copy => return Err(damaged()),
                                None => want_copy = true,
                            }
                        }
                    }
                    b"next" => break false,
                    b"done" => break true,
                    _ => {
                        if want_copy && kind != b"table2" {
                            // A damaged table without a copy.
                            return Err(damaged());
                        }
                    }
                }
                if next <= at || next >= len {
                    return Err(damaged());
                }
                at = next;
            };
            if want_copy {
                return Err(damaged());
            }
            starts.sort_unstable();
            starts.dedup();
            for (i, &(offset, compressed, table)) in entries.iter().enumerate() {
                // A chunk ends at the next chunk of its table, or else at the next section.
                let section_end = starts[starts
                    .partition_point(|&s| s <= offset)
                    .min(starts.len() - 1)];
                let end = match entries.get(i + 1) {
                    Some(&(o, _, t)) if t == table && o > offset => o.min(section_end),
                    _ => section_end,
                };
                if end <= offset {
                    return Err(damaged());
                }
                chunks.push(Chunk {
                    segment,
                    offset,
                    stored: end - offset,
                    compressed,
                });
            }
            files.push(file);
            if ends {
                break;
            }
            n += 1;
            if n > MAX_SEGMENTS {
                return Err(damaged());
            }
            let next_path = segment_path(path, n).ok_or_else(damaged)?;
            let name = next_path.display().to_string();
            file = File::open(&next_path)
                .map_err(|e| Error::Invalid(format!("segment {name}: {e}")))?;
            read_up_to(&mut file, 0, &mut h)?;
            if !h.starts_with(EVF) || u16::from_le_bytes([h[9], h[10]]) as u32 != n {
                return Err(Error::Invalid(format!(
                    "{name} is not segment {n} of this EnCase image"
                )));
            }
        }

        let media = media.ok_or_else(damaged)?;
        let needed = media.size.div_ceil(media.chunk_size) as usize;
        if chunks.len() < needed {
            return Err(Error::Invalid(
                "the EnCase image is missing chunks (is a segment file missing?)".into(),
            ));
        }
        chunks.truncate(needed);
        Ok(EwfDisk {
            files,
            chunks,
            chunk_size: media.chunk_size,
            sector_size: media.sector_size,
            size: media.size,
            smart: media.smart,
            cache: None,
        })
    }

    fn chunk(&mut self, i: usize) -> io::Result<&[u8]> {
        if self.cache.as_ref().is_none_or(|c| c.0 != i) {
            let c = self.chunks[i];
            let want = self.chunk_size.min(self.size - i as u64 * self.chunk_size) as usize;
            let file = &mut self.files[c.segment as usize];
            let data = if c.compressed {
                // A compressed chunk is never larger than the data (else it is stored), but
                // allow for a stored deflate block's few bytes of framing.
                let limit = c.stored.min(self.chunk_size + self.chunk_size / 256 + 64);
                let mut src = vec![0u8; limit as usize];
                let got = read_up_to(file, c.offset, &mut src)?;
                src.truncate(got);
                let mut out = Vec::with_capacity(self.chunk_size as usize);
                flate2::read::ZlibDecoder::new(&src[..])
                    .take(self.chunk_size + 1)
                    .read_to_end(&mut out)
                    .map_err(|_| damaged_chunk())?;
                // The last chunk may be stored padded to the full chunk size.
                if out.len() < want || out.len() as u64 > self.chunk_size {
                    return Err(damaged_chunk());
                }
                out.truncate(want);
                out
            } else {
                let mut src = vec![0u8; want + 4];
                if c.stored < src.len() as u64 || read_up_to(file, c.offset, &mut src)? != src.len()
                {
                    return Err(damaged_chunk());
                }
                if !checksum_ok(&src[..want], le32(&src, want)) {
                    return Err(damaged_chunk());
                }
                src.truncate(want);
                src
            };
            self.cache = Some((i, data));
        }
        Ok(&self.cache.as_ref().unwrap().1)
    }
}

fn read_only() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "EnCase images open read-only (convert the image to change it)",
    )
}

impl Disk for EwfDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let left = buf.len() - done;
            if at >= self.size {
                buf[done..].fill(0);
                break;
            }
            let (i, within) = (
                (at / self.chunk_size) as usize,
                (at % self.chunk_size) as usize,
            );
            let data = self.chunk(i)?;
            let n = (data.len() - within).min(left);
            buf[done..done + n].copy_from_slice(&data[within..within + n]);
            done += n;
        }
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(read_only())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        ContainerInfo::simple(
            ContainerKind::Ewf,
            if self.smart { "SMART" } else { "EnCase" },
            self.size,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_names() {
        let p = Path::new("dir/disk.E01");
        let name = |n| {
            segment_path(p, n)
                .unwrap()
                .display()
                .to_string()
                .replace('\\', "/")
        };
        assert_eq!(name(2), "dir/disk.E02");
        assert_eq!(name(99), "dir/disk.E99");
        assert_eq!(name(100), "dir/disk.EAA");
        assert_eq!(name(125), "dir/disk.EAZ");
        assert_eq!(name(126), "dir/disk.EBA");
        assert_eq!(name(775), "dir/disk.EZZ");
        assert_eq!(name(776), "dir/disk.FAA");
        assert_eq!(
            segment_path(Path::new("x.s01"), 100).unwrap(),
            Path::new("x.saa")
        );
        assert!(segment_path(p, 100 + 676 * 22).is_none());
    }
}
