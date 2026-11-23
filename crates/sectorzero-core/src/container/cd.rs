//! CD images, read-only: BIN/CUE, raw 2352-byte BIN without a cue sheet, Nero NRG and Alcohol
//! 120% MDF/MDS. Each exposes the 2048-byte user data of its first data track as the virtual
//! disk, so the ISO 9660 reader works on it unchanged.
//!
//! Stored sector layouts: cooked 2048 (MODE1/2048), MODE2/2336 (8-byte subheader, then the
//! user data), and raw 2352 (12-byte sync, 4-byte header, then user data for Mode 1, or an
//! 8-byte subheader first for Mode 2 form 1), optionally followed by 96 bytes of subchannel
//! (2448). Audio tracks are skipped.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::{ContainerInfo, ContainerKind, Disk, read_up_to};
use crate::error::{Error, Result};
use crate::util::*;

const USER: u64 = 2048;
const SYNC: [u8; 12] = [
    0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0,
];
/// Frames of the standard 2-second pregap some images store before track 1.
const PREGAP: u64 = 150;

pub(crate) fn has_sync(head: &[u8]) -> bool {
    head.len() >= 16 && head[..12] == SYNC && matches!(head[15], 1 | 2)
}

/// NRG footer: `NER5` + u64 (v2) or `NERO` + u32 (v1) chunk-list offset, big-endian.
pub(crate) fn nrg_chunk_offset(tail: &[u8], file_len: u64) -> Option<u64> {
    if tail.len() < 12 {
        return None;
    }
    let n = tail.len();
    let off = if &tail[n - 12..n - 8] == b"NER5" {
        be64(tail, n - 8)
    } else if &tail[n - 8..n - 4] == b"NERO" {
        u64::from(be32(tail, n - 4))
    } else {
        return None;
    };
    (off > 0 && off < file_len).then_some(off)
}

pub(crate) fn looks_like_mds(head: &[u8]) -> bool {
    head.starts_with(b"MEDIA DESCRIPTOR")
}

/// Where a data track's sectors live in its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Track {
    start: u64,
    stride: u64,
    user: u64,
    sectors: u64,
}

fn mode_name(t: &Track) -> &'static str {
    match (t.stride, t.user) {
        (2048, _) => "MODE1/2048",
        (2336, _) => "MODE2/2336",
        (2352, 16) => "MODE1/2352",
        (2352, _) => "MODE2/2352",
        (_, 16) => "MODE1/2448",
        _ => "MODE2/2448",
    }
}

pub(crate) struct CdDisk {
    file: File,
    kind: ContainerKind,
    track: Track,
}

fn read_only() -> Error {
    Error::Unsupported("writing CD images (BIN/CUE, NRG and MDF/MDS open read-only)".into())
}

/// The user-data offset of raw sectors at `at`, from the sync pattern and mode byte.
fn raw_user_offset(file: &mut File, at: u64) -> io::Result<Option<u64>> {
    let mut h = [0u8; 16];
    if read_up_to(file, at, &mut h)? < 16 || !has_sync(&h) {
        return Ok(None);
    }
    Ok(Some(if h[15] == 1 { 16 } else { 24 }))
}

/// A volume descriptor (ISO 9660, High Sierra-style CD001, or UDF's recognition sequence)
/// at logical sector 16 of the track.
fn has_volume_descriptor(file: &mut File, t: &Track) -> io::Result<bool> {
    let mut d = [0u8; 8];
    read_up_to(file, t.start + 16 * t.stride + t.user, &mut d)?;
    Ok(matches!(
        &d[1..6],
        b"CD001" | b"BEA01" | b"NSR02" | b"NSR03" | b"BOOT2" | b"CDW02" | b"TEA01"
    ))
}

/// Work out the layout of a track at `start` spanning `len` bytes, trying each stored sector
/// size in `strides`. A `trusted` table (cue, NRG, MDS) is believed as long as raw sectors
/// carry a sync pattern; a guess is accepted only with a volume descriptor where it belongs.
fn probe_track(
    file: &mut File,
    start: u64,
    len: u64,
    strides: &[u64],
    trusted: bool,
) -> io::Result<Option<Track>> {
    for &s in strides {
        let user = match s {
            2048 => Some(0),
            2336 => Some(8),
            2352 | 2448 => match raw_user_offset(file, start + 16 * s)? {
                Some(u) => Some(u),
                None => raw_user_offset(file, start)?,
            },
            _ => None,
        };
        let Some(user) = user else { continue };
        let t = Track {
            start,
            stride: s,
            user,
            sectors: len / s,
        };
        if t.sectors > 0 && (trusted || has_volume_descriptor(file, &t)?) {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

const ALL_STRIDES: [u64; 4] = [2048, 2352, 2336, 2448];

/// No usable table: try the whole data area, with and without a stored 2-second pregap.
fn guess_track(file: &mut File, len: u64) -> io::Result<Option<Track>> {
    for stride in ALL_STRIDES {
        for start in [0, PREGAP * stride] {
            if start < len
                && let Some(t) = probe_track(file, start, len - start, &[stride], false)?
            {
                return Ok(Some(t));
            }
        }
    }
    Ok(None)
}

// --- CUE ---------------------------------------------------------------------------------

/// Split a cue sheet line into words, keeping "quoted strings" whole.
fn cue_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut chars = line.trim().chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            words.push(chars.by_ref().take_while(|&c| c != '"').collect());
        } else {
            let mut w = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                w.push(c);
                chars.next();
            }
            words.push(w);
        }
    }
    words
}

fn msf_frames(s: &str) -> Option<u64> {
    let mut p = s.split(':').map(|x| x.parse::<u64>().ok());
    let (m, s, f) = (p.next()??, p.next()??, p.next()??);
    Some((m * 60 + s) * 75 + f)
}

/// Stride and user-data offset of a cue TRACK mode, `None` for audio and unknown modes.
fn cue_mode(mode: &str) -> Option<(u64, u64)> {
    Some(match mode.to_ascii_uppercase().as_str() {
        "MODE1/2048" | "MODE2/2048" => (2048, 0),
        "MODE2/2336" | "CDI/2336" => (2336, 8),
        "MODE1/2352" => (2352, 16),
        "MODE2/2352" | "CDI/2352" => (2352, 24),
        "MODE1/2448" => (2448, 16),
        "MODE2/2448" => (2448, 24),
        _ => return None,
    })
}

#[derive(Debug)]
struct CueTrack {
    file: String,
    file_type: String,
    mode: String,
    index0: Option<u64>,
    index1: Option<u64>,
}

fn parse_cue(text: &str) -> Vec<CueTrack> {
    let mut tracks: Vec<CueTrack> = Vec::new();
    let (mut file, mut file_type) = (String::new(), String::new());
    for line in text.lines() {
        let w = cue_words(line);
        let Some(cmd) = w.first() else { continue };
        match cmd.to_ascii_uppercase().as_str() {
            "FILE" if w.len() >= 2 => {
                file = w[1].clone();
                file_type = w.get(2).cloned().unwrap_or_default().to_ascii_uppercase();
            }
            "TRACK" if w.len() >= 3 => tracks.push(CueTrack {
                file: file.clone(),
                file_type: file_type.clone(),
                mode: w[2].clone(),
                index0: None,
                index1: None,
            }),
            "INDEX" if w.len() >= 3 => {
                if let Some(t) = tracks.last_mut() {
                    let at = msf_frames(&w[2]);
                    match w[1].parse::<u32>() {
                        Ok(0) => t.index0 = at,
                        Ok(1) => t.index1 = at,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    tracks
}

fn resolve_bin(cue: &Path, name: &str) -> Option<PathBuf> {
    let dir = cue
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let named = dir.join(name.replace('\\', std::path::MAIN_SEPARATOR_STR));
    if named.is_file() {
        return Some(named);
    }
    // Cue sheets often outlive a rename of their BIN, or name it with another case.
    let base = Path::new(&name.replace('\\', "/"))
        .file_name()
        .map(|b| b.to_string_lossy().to_lowercase());
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if base.as_deref() == Some(&e.file_name().to_string_lossy().to_lowercase()) {
                return Some(e.path());
            }
        }
    }
    [cue.with_extension("bin"), cue.with_extension("BIN")]
        .into_iter()
        .find(|p| p.is_file())
}

impl CdDisk {
    fn new(file: File, kind: ContainerKind, track: Track) -> Self {
        CdDisk { file, kind, track }
    }

    pub(crate) fn open_cue(path: &Path, writable: bool) -> Result<Self> {
        if writable {
            return Err(read_only());
        }
        let raw = std::fs::read(path)?;
        let text = String::from_utf8_lossy(raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&raw));
        let tracks = parse_cue(&text);
        let Some(i) = tracks.iter().position(|t| cue_mode(&t.mode).is_some()) else {
            return Err(Error::Unsupported(if tracks.is_empty() {
                "cue sheet lists no tracks".into()
            } else {
                "cue sheet has no data track (audio CDs have no file system)".into()
            }));
        };
        let t = &tracks[i];
        if !matches!(t.file_type.as_str(), "BINARY" | "") {
            return Err(Error::Unsupported(format!(
                "{} track files in cue sheets (BINARY only)",
                t.file_type
            )));
        }
        let (stride, user) = cue_mode(&t.mode).unwrap();
        let bin = resolve_bin(path, &t.file).ok_or_else(|| {
            Error::Invalid(format!(
                "the cue sheet's track file \"{}\" was not found",
                t.file
            ))
        })?;
        let mut file = File::open(&bin)?;
        let len = file.metadata()?.len();
        let start = t.index1.or(t.index0).unwrap_or(0) * stride;
        if start >= len {
            return Err(Error::Invalid(
                "cue sheet track starts past the end of its file".into(),
            ));
        }
        // The track ends where the next track in the same file begins (its pregap included).
        let end = tracks[i + 1..]
            .iter()
            .take_while(|n| n.file == t.file)
            .find_map(|n| n.index0.or(n.index1))
            .map_or(len, |f| (f * stride).min(len));
        let mut track = Track {
            start,
            stride,
            user,
            sectors: end.saturating_sub(start) / stride,
        };
        // Trust the cue's mode, but correct Mode 1/Mode 2 from the sector header when raw.
        if stride >= 2352
            && let Some(u) = raw_user_offset(&mut file, start + 16 * stride)?
        {
            track.user = u;
        }
        Ok(Self::new(file, ContainerKind::Cue, track))
    }

    /// A raw 2352/2448-byte BIN (or CloneCD IMG) opened without its cue sheet.
    pub(crate) fn open_raw(path: &Path, writable: bool) -> Result<Self> {
        if writable {
            return Err(read_only());
        }
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut s = [0u8; 16];
        read_up_to(&mut file, 2448, &mut s)?;
        let stride = if has_sync(&s) { 2448 } else { 2352 };
        let track = probe_track(&mut file, 0, len, &[stride], true)?
            .ok_or_else(|| Error::Invalid("raw CD sectors not recognised".into()))?;
        Ok(Self::new(file, ContainerKind::Cue, track))
    }

    // --- NRG -----------------------------------------------------------------------------

    pub(crate) fn open_nrg(path: &Path, writable: bool) -> Result<Self> {
        if writable {
            return Err(read_only());
        }
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut tail = [0u8; 12];
        read_up_to(&mut file, len.saturating_sub(12), &mut tail)?;
        let chunks_at = nrg_chunk_offset(&tail, len)
            .ok_or_else(|| Error::Invalid("Nero image footer not found".into()))?;
        let mut chunks = vec![0u8; (len - chunks_at).min(16 << 20) as usize];
        let got = read_up_to(&mut file, chunks_at, &mut chunks)?;
        chunks.truncate(got);
        let mut track = None;
        for (start, length, stride) in nrg_tracks(&chunks, chunks_at) {
            let found = match stride {
                Some(s) => probe_track(&mut file, start, length, &[s], true)?,
                None => probe_track(&mut file, start, length, &ALL_STRIDES, false)?,
            };
            if let Some(t) = found {
                track = Some(t);
                break;
            }
        }
        let track = match track {
            Some(t) => t,
            None => guess_track(&mut file, chunks_at)?
                .ok_or_else(|| Error::Unsupported("Nero image without a data track".into()))?,
        };
        Ok(Self::new(file, ContainerKind::Nrg, track))
    }

    // --- MDF/MDS -------------------------------------------------------------------------

    pub(crate) fn open_mds(path: &Path, writable: bool) -> Result<Self> {
        if writable {
            return Err(read_only());
        }
        let mds = std::fs::read(path)?;
        if !looks_like_mds(&mds) {
            return Err(Error::Invalid("MDS header not found".into()));
        }
        let (name, data_track) = mds_track(&mds)
            .ok_or_else(|| Error::Unsupported("MDS descriptor without a data track".into()))?;
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mdf = match name.as_deref() {
            Some(n) if n.starts_with("*.") => path.with_extension(&n[2..]),
            Some(n) if !n.is_empty() => dir.join(n),
            _ => path.with_extension("mdf"),
        };
        let mdf = if mdf.is_file() {
            mdf
        } else {
            [path.with_extension("mdf"), path.with_extension("MDF")]
                .into_iter()
                .find(|p| p.is_file())
                .ok_or_else(|| {
                    Error::Invalid(format!("MDF data file {} not found", mdf.display()))
                })?
        };
        let mut file = File::open(&mdf)?;
        let len = file.metadata()?.len();
        let (start, stride, sectors) = data_track;
        let length = match sectors {
            Some(n) => (n * stride).min(len.saturating_sub(start)),
            None => len.saturating_sub(start),
        };
        let track = probe_track(&mut file, start, length, &[stride], true)?
            .ok_or_else(|| Error::Invalid("MDS track layout does not match the MDF".into()))?;
        Ok(Self::new(file, ContainerKind::Mds, track))
    }

    /// An MDF opened on its own: use the MDS next to it, or recognise the sectors.
    pub(crate) fn open_mdf(path: &Path, writable: bool) -> Result<Option<Self>> {
        for mds in [path.with_extension("mds"), path.with_extension("MDS")] {
            if mds.is_file() {
                return Self::open_mds(&mds, writable).map(Some);
            }
        }
        if writable {
            return Err(read_only());
        }
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(guess_track(&mut file, len)?.map(|t| Self::new(file, ContainerKind::Mds, t)))
    }
}

/// Candidate tracks (start, length, stride if known) from an NRG chunk list: disc-at-once
/// tables (DAOX with 64-bit, DAOI with 32-bit offsets) and track-at-once ones (ETN2, ETNF).
fn nrg_tracks(chunks: &[u8], limit: u64) -> Vec<(u64, u64, Option<u64>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= chunks.len() {
        let id = &chunks[at..at + 4];
        let size = be32(chunks, at + 4) as usize;
        let data = &chunks[at + 8..(at + 8 + size).min(chunks.len())];
        match id {
            b"DAOX" | b"DAOI" => {
                let wide = id == b"DAOX";
                let entry = if wide { 42 } else { 30 };
                let mut e = 22;
                while e + entry <= data.len() {
                    let t = &data[e..e + entry];
                    let stride = u64::from(u16::from_be_bytes([t[12], t[13]]));
                    let (index1, end) = if wide {
                        (be64(t, 26), be64(t, 34))
                    } else {
                        (u64::from(be32(t, 22)), u64::from(be32(t, 26)))
                    };
                    if matches!(stride, 2048 | 2336 | 2352 | 2448) && index1 < end && end <= limit {
                        out.push((index1, end - index1, Some(stride)));
                    }
                    e += entry;
                }
            }
            b"ETN2" | b"ETNF" => {
                let wide = id == b"ETN2";
                let entry = if wide { 32 } else { 20 };
                for t in data.chunks_exact(entry) {
                    let (start, length) = if wide {
                        (be64(t, 0), be64(t, 8))
                    } else {
                        (u64::from(be32(t, 0)), u64::from(be32(t, 4)))
                    };
                    if length > 0 && start + length <= limit {
                        out.push((start, length, None));
                    }
                }
            }
            b"END!" => break,
            _ => {}
        }
        at += 8 + size;
    }
    out
}

/// The first data track of an MDS descriptor: its MDF file name (if the descriptor names
/// one) and (start offset, stored sector size, sector count if known).
#[allow(clippy::type_complexity)]
fn mds_track(mds: &[u8]) -> Option<(Option<String>, (u64, u64, Option<u64>))> {
    let field = |at: usize, n: usize| mds.get(at..at + n);
    let sessions = usize::from(u16::from_le_bytes(field(0x14, 2)?.try_into().ok()?));
    let session_at = le32(field(0x50, 4)?, 0) as usize;
    for s in 0..sessions.max(1) {
        let sb = field(session_at + s * 24, 24)?;
        let blocks = usize::from(sb[10]);
        let tracks_at = le32(sb, 20) as usize;
        let entries: Vec<&[u8]> = (0..blocks)
            .filter_map(|i| field(tracks_at + i * 80, 80))
            .filter(|t| (1..=99).contains(&t[4]))
            .collect();
        for (i, t) in entries.iter().enumerate() {
            let mode = t[0] & 0x0F;
            if !matches!(mode, 0xA..=0xC) {
                continue; // audio, Mode 2 form 2, or no data
            }
            let stride = u64::from(le16(t, 0x10));
            let start_sector = u64::from(le32(t, 0x24));
            let start = le64(t, 0x28);
            let sectors = entries
                .get(i + 1)
                .map(|n| u64::from(le32(n, 0x24)).saturating_sub(start_sector));
            let name = mds_file_name(mds, le32(t, 0x34) as usize);
            return Some((name, (start, stride, sectors.filter(|&n| n > 0))));
        }
    }
    None
}

fn mds_file_name(mds: &[u8], block: usize) -> Option<String> {
    if block == 0 {
        return None;
    }
    let b = mds.get(block..block + 16)?;
    let at = le32(b, 0) as usize;
    let wide = b[4] != 0;
    let rest = mds.get(at..)?;
    Some(if wide {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(rest.split(|&c| c == 0).next().unwrap_or(&[])).into_owned()
    })
}

impl Disk for CdDisk {
    fn size(&self) -> u64 {
        self.track.sectors * USER
    }

    fn sector_size(&self) -> u32 {
        2048
    }

    fn read_at(&mut self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        let t = self.track;
        while !buf.is_empty() {
            if offset >= self.size() {
                buf.fill(0);
                return Ok(());
            }
            let lba = offset / USER;
            let within = offset % USER;
            let n = (buf.len() as u64).min(USER - within) as usize;
            if t.stride == USER {
                // Cooked sectors are contiguous: read the whole run at once.
                let n = (buf.len() as u64).min(self.size() - offset) as usize;
                let got = read_up_to(&mut self.file, t.start + offset, &mut buf[..n])?;
                buf[got..n].fill(0);
                offset += n as u64;
                buf = &mut buf[n..];
                continue;
            }
            let at = t.start + lba * t.stride + t.user + within;
            let got = read_up_to(&mut self.file, at, &mut buf[..n])?;
            buf[got..n].fill(0);
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "CD images are read-only",
        ))
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        ContainerInfo::simple(self.kind, mode_name(&self.track), self.size())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Wrap cooked 2048-byte sectors as raw 2352-byte ones (sync, header, user data; EDC/ECC
    /// left zero, which readers of the user data ignore).
    pub(crate) fn to_raw(iso: &[u8], mode: u8) -> Vec<u8> {
        let mut out = Vec::new();
        for (lba, s) in iso.chunks(2048).enumerate() {
            let mut sec = vec![0u8; 2352];
            sec[..12].copy_from_slice(&SYNC);
            let abs = lba as u64 + PREGAP;
            let bcd = |v: u64| ((v / 10) << 4 | (v % 10)) as u8;
            sec[12] = bcd(abs / 75 / 60);
            sec[13] = bcd(abs / 75 % 60);
            sec[14] = bcd(abs % 75);
            sec[15] = mode;
            let user = if mode == 1 { 16 } else { 24 };
            sec[user..user + s.len()].copy_from_slice(s);
            out.extend_from_slice(&sec);
        }
        out
    }

    fn fake_iso(sectors: usize) -> Vec<u8> {
        let mut iso: Vec<u8> = (0..sectors * 2048).map(|i| (i / 2048) as u8).collect();
        iso[16 * 2048..16 * 2048 + 6].copy_from_slice(b"\x01CD001");
        iso
    }

    fn read_all(d: &mut CdDisk) -> Vec<u8> {
        let mut v = vec![0u8; d.size() as usize];
        d.read_at(0, &mut v).unwrap();
        v
    }

    #[test]
    fn cue_with_raw_and_cooked_tracks() {
        let tmp = tempfile::tempdir().unwrap();
        let iso = fake_iso(40);
        for (mode, data) in [
            ("MODE1/2048", iso.clone()),
            ("MODE1/2352", to_raw(&iso, 1)),
            ("MODE2/2352", to_raw(&iso, 2)),
        ] {
            let bin = tmp.path().join("Game Disc.bin");
            let mut file = data.clone();
            file.extend(vec![0x77u8; 2352 * 10]); // an audio track after the data
            std::fs::write(&bin, &file).unwrap();
            let cue = tmp.path().join("game.cue");
            let audio_at = data.len() as u64 / if mode == "MODE1/2048" { 2048 } else { 2352 };
            let f = audio_at;
            std::fs::write(
                &cue,
                format!(
                    "FILE \"Game Disc.bin\" BINARY\r\n  TRACK 01 {mode}\r\n    INDEX 01 00:00:00\r\n  \
                     TRACK 02 AUDIO\r\n    INDEX 00 {:02}:{:02}:{:02}\r\n    INDEX 01 00:59:00\r\n",
                    f / 75 / 60,
                    f / 75 % 60,
                    f % 75
                ),
            )
            .unwrap();
            let mut d = CdDisk::open_cue(&cue, false).unwrap();
            assert_eq!(d.info().variant, mode);
            assert_eq!(read_all(&mut d), iso, "{mode}");
            assert!(d.write_at(0, &[0]).is_err());
            assert!(matches!(
                CdDisk::open_cue(&cue, true),
                Err(Error::Unsupported(_))
            ));
        }
        // The raw BIN alone.
        let bin = tmp.path().join("alone.bin");
        std::fs::write(&bin, to_raw(&iso, 2)).unwrap();
        assert!(has_sync(&std::fs::read(&bin).unwrap()));
        let mut d = CdDisk::open_raw(&bin, false).unwrap();
        assert_eq!(read_all(&mut d), iso);
    }

    #[test]
    fn cue_parsing() {
        let t = parse_cue(
            "REM GENRE Game\nFILE \"a b.bin\" BINARY\n TRACK 01 MODE2/2352\n  INDEX 01 00:02:10\n",
        );
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].file, "a b.bin");
        assert_eq!(t[0].index1, Some(2 * 75 + 10));
    }

    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend_from_slice(&(data.len() as u32).to_be_bytes());
        c.extend_from_slice(data);
        c
    }

    #[test]
    fn nrg_dao_and_tao() {
        let tmp = tempfile::tempdir().unwrap();
        let iso = fake_iso(40);
        // DAO, v2 footer, raw Mode 1 track after a stored pregap.
        let raw = to_raw(&iso, 1);
        let pregap = 150 * 2352;
        let mut img = vec![0u8; pregap];
        img.extend_from_slice(&raw);
        let chunks_at = img.len() as u64;
        let mut dao = vec![0u8; 22];
        let mut t = vec![0u8; 42];
        t[12..14].copy_from_slice(&2352u16.to_be_bytes());
        t[18..26].copy_from_slice(&0u64.to_be_bytes());
        t[26..34].copy_from_slice(&(pregap as u64).to_be_bytes());
        t[34..42].copy_from_slice(&chunks_at.to_be_bytes());
        dao.extend_from_slice(&t);
        img.extend(chunk(b"DAOX", &dao));
        img.extend(chunk(b"END!", &[]));
        img.extend_from_slice(b"NER5");
        img.extend_from_slice(&chunks_at.to_be_bytes());
        let path = tmp.path().join("dao.nrg");
        std::fs::write(&path, &img).unwrap();
        let tail = &img[img.len() - 12..];
        assert_eq!(nrg_chunk_offset(tail, img.len() as u64), Some(chunks_at));
        let mut d = CdDisk::open_nrg(&path, false).unwrap();
        assert_eq!(d.info().variant, "MODE1/2352");
        assert_eq!(read_all(&mut d), iso);

        // TAO, v1 footer, cooked track.
        let mut img = iso.clone();
        let chunks_at = img.len() as u32;
        let mut etn = vec![0u8; 20];
        etn[4..8].copy_from_slice(&(iso.len() as u32).to_be_bytes());
        img.extend(chunk(b"ETNF", &etn));
        img.extend(chunk(b"END!", &[]));
        img.extend_from_slice(b"NERO");
        img.extend_from_slice(&chunks_at.to_be_bytes());
        let path = tmp.path().join("tao.nrg");
        std::fs::write(&path, &img).unwrap();
        let mut d = CdDisk::open_nrg(&path, false).unwrap();
        assert_eq!(d.info().variant, "MODE1/2048");
        assert_eq!(read_all(&mut d), iso);
    }

    #[test]
    fn mds_and_mdf() {
        let tmp = tempfile::tempdir().unwrap();
        let iso = fake_iso(40);
        // MDF: raw sectors with 96 bytes of subchannel each.
        let mut mdf = Vec::new();
        for s in to_raw(&iso, 1).chunks(2352) {
            mdf.extend_from_slice(s);
            mdf.extend_from_slice(&[0u8; 96]);
        }
        std::fs::write(tmp.path().join("disc.mdf"), &mdf).unwrap();
        let mut mds = vec![0u8; 0x200];
        mds[..16].copy_from_slice(b"MEDIA DESCRIPTOR");
        mds[0x10] = 1;
        mds[0x11] = 3;
        put_le16(&mut mds, 0x14, 1);
        put_le32(&mut mds, 0x50, 0x58);
        // Session block at 0x58: 3 track blocks (lead-in point 0xA0, track 1) at 0x70.
        mds[0x58 + 10] = 2;
        put_le32(&mut mds, 0x58 + 20, 0x70);
        mds[0x70 + 4] = 0xA0;
        let t = 0x70 + 80;
        mds[t] = 0xAA; // Mode 1
        mds[t + 4] = 1;
        put_le16(&mut mds, t + 0x10, 2448);
        put_le32(&mut mds, t + 0x34, 0x180);
        put_le32(&mut mds, 0x180, 0x190);
        mds[0x190..0x195].copy_from_slice(b"*.mdf");
        let path = tmp.path().join("disc.mds");
        std::fs::write(&path, &mds).unwrap();
        let mut d = CdDisk::open_mds(&path, false).unwrap();
        assert_eq!(d.info().variant, "MODE1/2448");
        assert_eq!(read_all(&mut d), iso);
        let mut d = CdDisk::open_mdf(&tmp.path().join("disc.mdf"), false)
            .unwrap()
            .unwrap();
        assert_eq!(read_all(&mut d), iso);
        // Without the descriptor the layout is recognised from the sectors.
        std::fs::remove_file(&path).unwrap();
        let mut d = CdDisk::open_mdf(&tmp.path().join("disc.mdf"), false)
            .unwrap()
            .unwrap();
        assert_eq!(read_all(&mut d), iso);
    }
}
