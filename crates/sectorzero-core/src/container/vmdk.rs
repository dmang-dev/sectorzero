//! VMware virtual disks (VMDK), per VMware's published "Virtual Disk Format 1.1".
//!
//! A disk is a text descriptor plus extents. Hosted sparse extents start with a `KDMV`
//! header and map grains (usually 64 KiB) through a grain directory of grain tables, with a
//! redundant copy of both. `monolithicSparse` embeds the descriptor in the sparse file;
//! `monolithicFlat` keeps it in a small text file next to a `-flat.vmdk` raw extent.
//! Supported extents: SPARSE, FLAT/VMFS and ZERO.
//!
//! Snapshots and linked clones (a `parentCID` other than ffffffff) read unwritten grains from
//! the disk `parentFileNameHint` names, after checking that its CID is still `parentCID`.
//! Writing a grain the snapshot does not hold yet copies the parent's grain in first. The
//! first write to any VMDK gives it a new CID, as VMware does, so its snapshots can tell.
//!
//! Stream-optimized disks (OVF/OVA exports) store each grain deflate-compressed behind a
//! small header, with the grain directory in a footer at the end. They are read-only here.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::vhd::resolve_name;
use super::{ContainerInfo, ContainerKind, Disk, open_parent, read_up_to};
use crate::error::{Error, Result};
use crate::util::*;

const SECTOR: u64 = 512;
const GRAIN_SECTORS: u64 = 128;
const GTES: u64 = 512;
const DESCRIPTOR_SECTORS: u64 = 20;

struct Sparse {
    file: File,
    grain: u64,
    gtes: u64,
    gd_offset: u64,
    rgd_offset: Option<u64>,
    gd: Vec<u32>,
    rgd: Vec<u32>,
    tables: HashMap<usize, Vec<u32>>,
    rtables: HashMap<usize, Vec<u32>>,
    file_len: u64,
    /// Stream-optimized: grains are deflate-compressed behind a 12-byte header.
    compressed: bool,
    /// The last grain decompressed (grain number, data).
    cache: Option<(u64, Vec<u8>)>,
}

enum Kind {
    Flat { file: File, base: u64 },
    Sparse(Box<Sparse>),
    Zero,
}

struct Extent {
    start: u64,
    len: u64,
    writable: bool,
    kind: Kind,
}

pub(crate) struct VmdkDisk {
    extents: Vec<Extent>,
    size: u64,
    create_type: String,
    /// Snapshots: the parent that supplies the grains the snapshot never wrote.
    parent: Option<Box<dyn Disk>>,
    parent_path: Option<PathBuf>,
    /// Where the descriptor text is (file, byte offset, text), to give it a new CID before
    /// the first change; `None` once that is done (or for a disk without a descriptor).
    cid_at: Option<(PathBuf, u64, Vec<u8>)>,
}

/// `text` with its `CID=` value replaced by a new random one of the same length.
fn with_new_cid(text: &[u8]) -> Option<Vec<u8>> {
    let mut at = 0;
    while let Some(i) = text[at..].windows(4).position(|w| w == b"CID=") {
        let pos = at + i;
        if pos == 0 || text[pos - 1] == b'\n' {
            let start = pos + 4;
            let len = text[start..]
                .iter()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
            if len == 0 || len > 8 {
                return None;
            }
            let fresh = format!(
                "{:08x}",
                u32::from_le_bytes(new_guid()[0..4].try_into().unwrap())
            );
            let mut out = text.to_vec();
            out[start..start + len].copy_from_slice(&fresh.as_bytes()[8 - len..]);
            return Some(out);
        }
        at = pos + 4;
    }
    None
}

/// The CID in the descriptor of the VMDK at `path` (embedded or a text file).
fn descriptor_cid(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let mut h = [0u8; 512];
    read_up_to(&mut f, 0, &mut h).ok()?;
    let text = if &h[0..4] == b"KDMV" {
        let (off, len) = (le64(&h, 28) * SECTOR, le64(&h, 36) * SECTOR);
        if off == 0 || len == 0 || len > 1 << 20 {
            return None;
        }
        let mut t = vec![0u8; len as usize];
        read_up_to(&mut f, off, &mut t).ok()?;
        t
    } else {
        let mut t = vec![0u8; 64 * 1024];
        let n = read_up_to(&mut f, 0, &mut t).ok()?;
        t.truncate(n);
        t
    };
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    String::from_utf8_lossy(&text[..end]).lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "CID").then(|| v.trim().trim_matches('"').to_ascii_lowercase())
    })
}

/// Whether the start of a file looks like a VMDK text descriptor.
pub(crate) fn looks_like_descriptor(head: &[u8]) -> bool {
    let text_len = head.iter().position(|&b| b == 0).unwrap_or(head.len());
    let text = &head[..text_len];
    if text.len() < 20 || text.iter().any(|&b| b < 9 || (b > 13 && b < 32 && b != 27)) {
        return false;
    }
    let s = String::from_utf8_lossy(text);
    s.contains("createType")
        && (s.contains("# Disk DescriptorFile") || s.contains("RW ") || s.contains("RDONLY "))
}

struct Descriptor {
    create_type: String,
    parent_cid: String,
    parent_hint: Option<String>,
    extents: Vec<(String, u64, String, Option<String>, u64)>,
}

fn parse_descriptor(text: &str) -> Result<Descriptor> {
    let mut d = Descriptor {
        create_type: String::new(),
        parent_cid: "ffffffff".into(),
        parent_hint: None,
        extents: vec![],
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or("");
        if matches!(first, "RW" | "RDONLY" | "NOACCESS") {
            let mut rest = line[first.len()..].trim_start();
            let sectors_tok = rest.split_whitespace().next().unwrap_or("");
            let sectors: u64 = sectors_tok
                .parse()
                .map_err(|_| Error::Invalid(format!("bad VMDK extent line: {line}")))?;
            rest = rest[sectors_tok.len()..].trim_start();
            let kind = rest.split_whitespace().next().unwrap_or("").to_string();
            rest = rest[kind.len()..].trim_start();
            let mut name = None;
            if let Some(r) = rest.strip_prefix('"') {
                let end = r
                    .find('"')
                    .ok_or_else(|| Error::Invalid(format!("bad VMDK extent line: {line}")))?;
                name = Some(r[..end].to_string());
                rest = r[end + 1..].trim_start();
            }
            let offset = rest
                .split_whitespace()
                .next()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            d.extents
                .push((first.to_string(), sectors, kind, name, offset));
        } else if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            match k.trim() {
                "createType" => d.create_type = v,
                "parentCID" => d.parent_cid = v.to_ascii_lowercase(),
                "parentFileNameHint" => d.parent_hint = Some(v),
                _ => {}
            }
        }
    }
    if d.extents.is_empty() {
        return Err(Error::Invalid("VMDK descriptor lists no extents".into()));
    }
    Ok(d)
}

fn read_u32s(file: &mut File, offset: u64, count: usize) -> io::Result<Vec<u32>> {
    let mut raw = vec![0u8; count * 4];
    let got = read_up_to(file, offset, &mut raw)?;
    raw[got..].fill(0);
    Ok(raw.chunks(4).map(|c| le32(c, 0)).collect())
}

fn write_at(file: &mut File, offset: u64, data: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)
}

/// Parse a KDMV header; returns the sparse extent plus (descriptor offset, size) in bytes.
fn open_sparse(mut file: File) -> Result<(Sparse, u64, u64, u64)> {
    let mut h = [0u8; 512];
    read_up_to(&mut file, 0, &mut h)?;
    if &h[0..4] != b"KDMV" {
        return Err(Error::Invalid("VMDK sparse header not found".into()));
    }
    let version = le32(&h, 4);
    let flags = le32(&h, 8);
    if version > 3 {
        return Err(Error::Unsupported(format!("VMDK sparse version {version}")));
    }
    let compressed = flags & (1 << 16) != 0;
    let method = le16(&h, 77);
    if method != u16::from(compressed) {
        return Err(Error::Unsupported(format!(
            "VMDK compression method {method}"
        )));
    }
    let capacity = le64(&h, 12);
    let grain = le64(&h, 20);
    let gtes = u64::from(le32(&h, 44));
    let mut gd_offset = le64(&h, 56);
    let mut rgd_offset = (flags & 2 != 0).then(|| le64(&h, 48));
    if gd_offset == u64::MAX {
        // Stream-optimized: the footer, a copy of the header with the real directory
        // offset, sits just before the end-of-stream marker.
        let len = file.metadata()?.len();
        let mut f = [0u8; 512];
        if len >= 1536 {
            read_up_to(&mut file, len - 1024, &mut f)?;
        }
        if &f[0..4] != b"KDMV" || le64(&f, 56) == u64::MAX {
            return Err(Error::Invalid("VMDK footer not found".into()));
        }
        gd_offset = le64(&f, 56);
        rgd_offset = (le32(&f, 8) & 2 != 0).then(|| le64(&f, 48));
    }
    if grain == 0 || gtes == 0 || !grain.is_power_of_two() {
        return Err(Error::Invalid("VMDK sparse header is invalid".into()));
    }
    let gd_count = capacity.div_ceil(grain * gtes) as usize;
    if rgd_offset == Some(0) {
        rgd_offset = None;
    }
    if gd_offset == 0 {
        // Only the redundant directory is valid in some old files.
        gd_offset = rgd_offset
            .take()
            .ok_or_else(|| Error::Invalid("VMDK has no grain directory".into()))?;
    }
    let gd = read_u32s(&mut file, gd_offset * SECTOR, gd_count)?;
    let rgd = match rgd_offset {
        Some(o) => read_u32s(&mut file, o * SECTOR, gd_count)?,
        None => vec![],
    };
    let file_len = file.metadata()?.len();
    let s = Sparse {
        file,
        grain: grain * SECTOR,
        gtes,
        gd_offset,
        rgd_offset,
        gd,
        rgd,
        tables: HashMap::new(),
        rtables: HashMap::new(),
        file_len,
        compressed,
        cache: None,
    };
    Ok((
        s,
        capacity * SECTOR,
        le64(&h, 28) * SECTOR,
        le64(&h, 36) * SECTOR,
    ))
}

impl Sparse {
    /// The grain table entry for virtual `offset`: 0 never written (a snapshot reads its
    /// parent there), 1 a zeroed grain, else the grain's sector.
    fn gte(&mut self, offset: u64) -> io::Result<u32> {
        let g = offset / self.grain;
        let (di, ti) = ((g / self.gtes) as usize, (g % self.gtes) as usize);
        let Some(&gde) = self.gd.get(di) else {
            return Ok(0);
        };
        if gde == 0 {
            return Ok(0);
        }
        if !self.tables.contains_key(&di) {
            let t = read_u32s(&mut self.file, u64::from(gde) * SECTOR, self.gtes as usize)?;
            self.tables.insert(di, t);
        }
        Ok(self.tables[&di][ti])
    }

    /// File offset of the grain holding virtual `offset`, or None when unallocated.
    fn lookup(&mut self, offset: u64) -> io::Result<Option<u64>> {
        let gte = self.gte(offset)?;
        Ok((gte > 1).then(|| u64::from(gte) * SECTOR))
    }

    /// The decompressed grain stored at `at` (stream-optimized disks).
    fn inflate(&mut self, at: u64, grain_no: u64) -> io::Result<&[u8]> {
        if self.cache.as_ref().map(|c| c.0) != Some(grain_no) {
            let mut hdr = [0u8; 12];
            read_up_to(&mut self.file, at, &mut hdr)?;
            let len = le32(&hdr, 8) as usize;
            if len > 2 * self.grain as usize + 4096 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "corrupt compressed VMDK grain",
                ));
            }
            let mut input = vec![0u8; len];
            let got = read_up_to(&mut self.file, at + 12, &mut input)?;
            input.truncate(got);
            let mut out = vec![0u8; self.grain as usize];
            let mut d = flate2::Decompress::new(true);
            if d.decompress(&input, &mut out, flate2::FlushDecompress::Finish)
                .is_err()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "corrupt compressed VMDK grain",
                ));
            }
            // The last grain of a disk may be stored shorter; the rest reads as zeros.
            self.cache = Some((grain_no, out));
        }
        Ok(&self.cache.as_ref().unwrap().1)
    }

    fn append(&mut self, len: u64) -> io::Result<u64> {
        let start = round_up(self.file_len, SECTOR);
        self.file_len = start + len;
        self.file.set_len(self.file_len)?;
        Ok(start)
    }

    fn table(&mut self, redundant: bool, di: usize) -> io::Result<&mut Vec<u32>> {
        let (dir, map) = if redundant {
            (&self.rgd, &mut self.rtables)
        } else {
            (&self.gd, &mut self.tables)
        };
        if let std::collections::hash_map::Entry::Vacant(e) = map.entry(di) {
            let t = read_u32s(
                &mut self.file,
                u64::from(dir[di]) * SECTOR,
                self.gtes as usize,
            )?;
            e.insert(t);
        }
        Ok(map.get_mut(&di).unwrap())
    }

    /// Allocate a zeroed grain for virtual `offset` and record it in both grain tables.
    fn allocate(&mut self, offset: u64) -> io::Result<u64> {
        let g = offset / self.grain;
        let (di, ti) = ((g / self.gtes) as usize, (g % self.gtes) as usize);
        let gt_bytes = round_up(self.gtes * 4, SECTOR);
        for redundant in [false, true] {
            if redundant && self.rgd_offset.is_none() {
                continue;
            }
            let dir_entry = if redundant { self.rgd[di] } else { self.gd[di] };
            if dir_entry == 0 {
                let at = self.append(gt_bytes)?;
                let sector = (at / SECTOR) as u32;
                let dir_off = if redundant {
                    self.rgd_offset.unwrap()
                } else {
                    self.gd_offset
                };
                write_at(
                    &mut self.file,
                    dir_off * SECTOR + di as u64 * 4,
                    &sector.to_le_bytes(),
                )?;
                if redundant {
                    self.rgd[di] = sector;
                    self.rtables.insert(di, vec![0; self.gtes as usize]);
                } else {
                    self.gd[di] = sector;
                    self.tables.insert(di, vec![0; self.gtes as usize]);
                }
            }
        }
        let start = self.append(self.grain)?;
        let sector = (start / SECTOR) as u32;
        for redundant in [false, true] {
            if redundant && self.rgd_offset.is_none() {
                continue;
            }
            let dir_entry = if redundant { self.rgd[di] } else { self.gd[di] };
            self.table(redundant, di)?[ti] = sector;
            write_at(
                &mut self.file,
                u64::from(dir_entry) * SECTOR + ti as u64 * 4,
                &sector.to_le_bytes(),
            )?;
        }
        Ok(start)
    }
}

impl VmdkDisk {
    pub(crate) fn open(path: &Path, writable: bool) -> Result<Self> {
        let mut file = File::options().read(true).write(writable).open(path)?;
        let mut magic = [0u8; 4];
        read_up_to(&mut file, 0, &mut magic)?;
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let self_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();

        let mut own_sparse = None;
        let mut cid_at = None;
        let descriptor_text = if &magic == b"KDMV" {
            let (sparse, capacity, d_off, d_len) = open_sparse(file)?;
            let text = if d_off > 0 && d_len > 0 {
                let mut f = sparse.file.try_clone()?;
                let mut t = vec![0u8; d_len as usize];
                read_up_to(&mut f, d_off, &mut t)?;
                let end = t.iter().position(|&b| b == 0).unwrap_or(t.len());
                cid_at = Some((path.to_path_buf(), d_off, t[..end].to_vec()));
                Some(String::from_utf8_lossy(&t[..end]).into_owned())
            } else {
                None
            };
            own_sparse = Some((sparse, capacity));
            text
        } else {
            let len = file.metadata()?.len();
            if len > 1024 * 1024 {
                return Err(Error::Invalid("VMDK descriptor is too large".into()));
            }
            let mut t = vec![0u8; len as usize];
            read_up_to(&mut file, 0, &mut t)?;
            cid_at = Some((path.to_path_buf(), 0, t.clone()));
            Some(String::from_utf8_lossy(&t).into_owned())
        };

        let Some(text) = descriptor_text else {
            // A bare sparse extent without a descriptor.
            let (sparse, capacity) = own_sparse.unwrap();
            return Ok(VmdkDisk {
                extents: vec![Extent {
                    start: 0,
                    len: capacity,
                    writable,
                    kind: Kind::Sparse(Box::new(sparse)),
                }],
                size: capacity,
                create_type: "monolithicSparse".into(),
                parent: None,
                parent_path: None,
                cid_at: None,
            });
        };
        let d = parse_descriptor(&text)?;
        let (parent, parent_path) = if d.parent_cid == "ffffffff" {
            (None, None)
        } else {
            let hint = d
                .parent_hint
                .clone()
                .ok_or_else(|| Error::Invalid("VMDK snapshot has no parentFileNameHint".into()))?;
            let base = Path::new(&hint.replace('\\', "/"))
                .file_name()
                .map(|b| dir.join(b));
            let found = [Some(resolve_name(&dir, &hint)), base]
                .into_iter()
                .flatten()
                .find(|p| p.is_file())
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "parent disk of the VMDK snapshot not found (looked for {hint})"
                    ))
                })?;
            if let Some(cid) = descriptor_cid(&found)
                && cid != d.parent_cid
            {
                return Err(Error::Invalid(format!(
                    "parent disk {} has changed since this VMDK snapshot was made from it \
                     (CID {} expected, found {})",
                    found.display(),
                    d.parent_cid,
                    cid
                )));
            }
            (Some(open_parent(&found)?), Some(found))
        };
        let single_sparse = own_sparse.is_some()
            && d.extents
                .iter()
                .filter(|e| e.2.eq_ignore_ascii_case("SPARSE"))
                .count()
                == 1;
        let mut extents = Vec::new();
        let mut start = 0u64;
        for (access, sectors, kind, name, offset) in d.extents {
            let len = sectors * SECTOR;
            let ext_writable = writable && access == "RW";
            if access == "NOACCESS" {
                return Err(Error::Unsupported("VMDK NOACCESS extents".into()));
            }
            let resolve = |name: &Option<String>| -> Result<PathBuf> {
                let n = name
                    .as_ref()
                    .ok_or_else(|| Error::Invalid("VMDK extent without a file name".into()))?;
                Ok(dir.join(n))
            };
            let kind = match kind.to_ascii_uppercase().as_str() {
                "ZERO" => Kind::Zero,
                "FLAT" | "VMFS" => {
                    let p = resolve(&name)?;
                    let f = File::options()
                        .read(true)
                        .write(ext_writable)
                        .open(&p)
                        .map_err(|e| {
                            Error::Invalid(format!("cannot open VMDK extent {}: {e}", p.display()))
                        })?;
                    Kind::Flat {
                        file: f,
                        base: offset * SECTOR,
                    }
                }
                "SPARSE" => {
                    // An embedded descriptor's single SPARSE extent is the file itself, even
                    // if the file was renamed since.
                    let is_self = single_sparse
                        || name.as_ref().is_some_and(|n| {
                            Path::new(n)
                                .file_name()
                                .map(|x| x.to_string_lossy().to_ascii_lowercase())
                                == Some(self_name.clone())
                        });
                    let sparse = match (is_self, own_sparse.take()) {
                        (true, Some((s, _))) => s,
                        (_, other) => {
                            own_sparse = other;
                            let p = resolve(&name)?;
                            let f = File::options()
                                .read(true)
                                .write(ext_writable)
                                .open(&p)
                                .map_err(|e| {
                                    Error::Invalid(format!(
                                        "cannot open VMDK extent {}: {e}",
                                        p.display()
                                    ))
                                })?;
                            open_sparse(f)?.0
                        }
                    };
                    Kind::Sparse(Box::new(sparse))
                }
                other => return Err(Error::Unsupported(format!("VMDK {other} extents"))),
            };
            extents.push(Extent {
                start,
                len,
                writable: ext_writable,
                kind,
            });
            start += len;
        }
        Ok(VmdkDisk {
            extents,
            size: start,
            create_type: d.create_type,
            parent,
            parent_path,
            cid_at: if writable { cid_at } else { None },
        })
    }

    pub(crate) fn create(path: &Path, size: u64, dynamic: bool) -> Result<Self> {
        let sectors = size / SECTOR;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "disk".into());
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if dynamic {
            let desc = descriptor("monolithicSparse", sectors, "SPARSE", &file_name, None);
            let gd_count = sectors.div_ceil(GRAIN_SECTORS * GTES);
            let gd_sectors = (gd_count * 4).div_ceil(SECTOR);
            let gt_sectors = (GTES * 4).div_ceil(SECTOR);
            let rgd = 1 + DESCRIPTOR_SECTORS;
            let rgts = rgd + gd_sectors;
            let gd = rgts + gd_count * gt_sectors;
            let gts = gd + gd_sectors;
            let overhead = round_up(gts + gd_count * gt_sectors, GRAIN_SECTORS);

            let mut h = [0u8; 512];
            h[0..4].copy_from_slice(b"KDMV");
            put_le32(&mut h, 4, 1);
            put_le32(&mut h, 8, 3);
            put_le64(&mut h, 12, sectors);
            put_le64(&mut h, 20, GRAIN_SECTORS);
            put_le64(&mut h, 28, 1);
            put_le64(&mut h, 36, DESCRIPTOR_SECTORS);
            put_le32(&mut h, 44, GTES as u32);
            put_le64(&mut h, 48, rgd);
            put_le64(&mut h, 56, gd);
            put_le64(&mut h, 64, overhead);
            h[73] = b'\n';
            h[74] = b' ';
            h[75] = b'\r';
            h[76] = b'\n';

            let mut meta = vec![0u8; (overhead * SECTOR) as usize];
            meta[..512].copy_from_slice(&h);
            if desc.len() > (DESCRIPTOR_SECTORS * SECTOR) as usize {
                return Err(Error::Invalid("VMDK descriptor too long".into()));
            }
            meta[512..512 + desc.len()].copy_from_slice(desc.as_bytes());
            for (dir, tables) in [(rgd, rgts), (gd, gts)] {
                for i in 0..gd_count {
                    put_le32(
                        &mut meta,
                        (dir * SECTOR + i * 4) as usize,
                        (tables + i * gt_sectors) as u32,
                    );
                }
            }
            let mut file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)?;
            file.write_all(&meta)?;
            file.sync_all()?;
        } else {
            let flat_name = format!("{stem}-flat.vmdk");
            let flat = path.with_file_name(&flat_name);
            let f = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&flat)?;
            f.set_len(size)?;
            let desc = descriptor("monolithicFlat", sectors, "FLAT", &flat_name, Some(0));
            std::fs::write(path, desc)?;
        }
        let mut d = Self::open(path, true)?;
        d.cid_at = None;
        Ok(d)
    }

    /// Create an empty snapshot (a monolithicSparse delta) over the VMDK `parent_path`,
    /// naming it by a relative path when it is on the same drive.
    pub(crate) fn create_differencing(path: &Path, parent_path: &Path) -> Result<Self> {
        let cid = descriptor_cid(parent_path).ok_or_else(|| {
            Error::Invalid(format!(
                "parent {} is not a VMDK with a descriptor",
                parent_path.display()
            ))
        })?;
        let size = VmdkDisk::open(parent_path, false)?.size;
        drop(Self::create(path, size, true)?);
        let abs_parent = std::path::absolute(parent_path)?;
        let abs_child = std::path::absolute(path)?;
        let child_dir = abs_child.parent().unwrap_or(Path::new("."));
        let hint = super::vhd::relative_windows_path(child_dir, &abs_parent)
            .map(|r| r.trim_start_matches(".\\").to_string())
            .unwrap_or_else(|| super::vhd::windows_path_string(&abs_parent));
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let desc = descriptor_with_parent(
            "monolithicSparse",
            size / SECTOR,
            "SPARSE",
            &name,
            None,
            Some((&cid, &hint)),
        );
        if desc.len() > (DESCRIPTOR_SECTORS * SECTOR) as usize {
            return Err(Error::Invalid("VMDK descriptor too long".into()));
        }
        let mut area = vec![0u8; (DESCRIPTOR_SECTORS * SECTOR) as usize];
        area[..desc.len()].copy_from_slice(desc.as_bytes());
        let mut f = File::options().write(true).open(path)?;
        write_at(&mut f, SECTOR, &area)?;
        drop(f);
        let mut d = Self::open(path, true)?;
        d.cid_at = None;
        Ok(d)
    }

    fn extent_index(&self, offset: u64) -> Option<usize> {
        self.extents
            .iter()
            .position(|e| offset >= e.start && offset < e.start + e.len)
    }

    /// Before the first change: a new CID in the descriptor.
    fn new_cid(&mut self) -> io::Result<()> {
        if let Some((path, off, text)) = self.cid_at.take()
            && let Some(new) = with_new_cid(&text)
        {
            let mut f = File::options().write(true).open(&path)?;
            write_at(&mut f, off, &new)?;
        }
        Ok(())
    }
}

fn descriptor(
    create_type: &str,
    sectors: u64,
    kind: &str,
    name: &str,
    offset: Option<u64>,
) -> String {
    descriptor_with_parent(create_type, sectors, kind, name, offset, None)
}

/// A descriptor; `parent` is (parent CID, parentFileNameHint) for a snapshot.
fn descriptor_with_parent(
    create_type: &str,
    sectors: u64,
    kind: &str,
    name: &str,
    offset: Option<u64>,
    parent: Option<(&str, &str)>,
) -> String {
    let cid = u32::from_le_bytes(new_guid()[0..4].try_into().unwrap());
    let cylinders = (sectors / (16 * 63)).clamp(1, 16383);
    let offset = offset.map(|o| format!(" {o}")).unwrap_or_default();
    let (parent_cid, hint) = match parent {
        Some((cid, hint)) => (cid.to_string(), format!("parentFileNameHint=\"{hint}\"\n")),
        None => ("ffffffff".to_string(), String::new()),
    };
    format!(
        "# Disk DescriptorFile\nversion=1\nencoding=\"UTF-8\"\nCID={cid:08x}\nparentCID={parent_cid}\n\
         createType=\"{create_type}\"\n{hint}\n# Extent description\nRW {sectors} {kind} \"{name}\"{offset}\n\n\
         # The Disk Data Base\n#DDB\n\nddb.virtualHWVersion = \"4\"\nddb.adapterType = \"ide\"\n\
         ddb.geometry.cylinders = \"{cylinders}\"\nddb.geometry.heads = \"16\"\nddb.geometry.sectors = \"63\"\n\
         ddb.toolsVersion = \"0\"\n"
    )
}

impl Disk for VmdkDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let Some(i) = self.extent_index(offset) else {
                buf.fill(0);
                break;
            };
            let e = &mut self.extents[i];
            let within = offset - e.start;
            let mut n = (buf.len() as u64).min(e.len - within) as usize;
            match &mut e.kind {
                Kind::Zero => buf[..n].fill(0),
                Kind::Flat { file, base } => {
                    let got = read_up_to(file, *base + within, &mut buf[..n])?;
                    buf[got..n].fill(0);
                }
                Kind::Sparse(s) => {
                    let in_grain = within % s.grain;
                    n = n.min((s.grain - in_grain) as usize);
                    match s.gte(within)? {
                        0 if self.parent.is_some() => {
                            self.parent
                                .as_mut()
                                .unwrap()
                                .read_at(offset, &mut buf[..n])?;
                        }
                        0 | 1 => buf[..n].fill(0),
                        gte if s.compressed => {
                            let g = within / s.grain;
                            let data = s.inflate(u64::from(gte) * SECTOR, g)?;
                            let a = in_grain as usize;
                            buf[..n].copy_from_slice(&data[a..a + n]);
                        }
                        gte => {
                            let at = u64::from(gte) * SECTOR + in_grain;
                            let got = read_up_to(&mut s.file, at, &mut buf[..n])?;
                            buf[got..n].fill(0);
                        }
                    }
                }
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    fn write_at(&mut self, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the VMDK",
            ));
        }
        if self
            .extents
            .iter()
            .any(|e| matches!(&e.kind, Kind::Sparse(s) if s.compressed))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stream-optimized VMDK images are read-only (convert them to another format first)",
            ));
        }
        if !buf.is_empty() && self.extents.iter().any(|e| e.writable) {
            self.new_cid()?;
        }
        while !buf.is_empty() {
            let i = self.extent_index(offset).expect("offset within the disk");
            let e = &mut self.extents[i];
            if !e.writable {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "VMDK extent is read-only",
                ));
            }
            let within = offset - e.start;
            let mut n = (buf.len() as u64).min(e.len - within) as usize;
            match &mut e.kind {
                Kind::Zero => {
                    if !is_zero(&buf[..n]) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "VMDK ZERO extents are read-only",
                        ));
                    }
                }
                Kind::Flat { file, base } => write_at(file, *base + within, &buf[..n])?,
                Kind::Sparse(s) => {
                    n = n.min((s.grain - within % s.grain) as usize);
                    let at = match s.lookup(within)? {
                        Some(at) => Some(at),
                        None if self.parent.is_some() && s.gte(within)? == 0 => {
                            // Copy the parent's grain in before part of it is overwritten.
                            let grain_start = offset - within % s.grain;
                            let mut data = vec![0u8; s.grain as usize];
                            self.parent
                                .as_mut()
                                .unwrap()
                                .read_at(grain_start, &mut data)?;
                            let at = s.allocate(within)?;
                            write_at(&mut s.file, at, &data)?;
                            Some(at)
                        }
                        None if is_zero(&buf[..n]) => None,
                        None => Some(s.allocate(within)?),
                    };
                    if let Some(at) = at {
                        write_at(&mut s.file, at + within % s.grain, &buf[..n])?;
                    }
                }
            }
            offset += n as u64;
            buf = &buf[n..];
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        for e in &mut self.extents {
            match &mut e.kind {
                Kind::Flat { file, .. } => file.flush()?,
                Kind::Sparse(s) => s.file.flush()?,
                Kind::Zero => {}
            }
        }
        Ok(())
    }

    fn info(&self) -> ContainerInfo {
        let mut info = ContainerInfo::simple(ContainerKind::Vmdk, &self.create_type, self.size);
        info.parent = self.parent_path.as_ref().map(|p| p.display().to_string());
        info
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let end = offset + len.max(1);
        let mut pos = offset;
        let mut inherits = false;
        while pos < end {
            let Some(i) = self.extent_index(pos) else {
                break;
            };
            let e = &mut self.extents[i];
            let within = pos - e.start;
            match &mut e.kind {
                Kind::Zero => pos = e.start + e.len,
                Kind::Flat { .. } => return Ok(true),
                Kind::Sparse(s) => {
                    match s.gte(within)? {
                        0 => inherits = true,
                        1 => {}
                        _ => return Ok(true),
                    }
                    pos += s.grain - within % s.grain;
                }
            }
        }
        match self.parent.as_mut() {
            Some(p) if inherits => p.is_allocated(offset, len),
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_and_flat_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        for dynamic in [true, false] {
            let p = dir.path().join(format!("d{dynamic}.vmdk"));
            let size = 1_474_560;
            let mut d = VmdkDisk::create(&p, size, dynamic).unwrap();
            d.write_at(65_000, &[0x5A; 2000]).unwrap();
            d.write_at(size - 512, &[0x11; 512]).unwrap();
            d.sync().unwrap();
            drop(d);
            let mut d = VmdkDisk::open(&p, false).unwrap();
            assert_eq!(d.size(), size);
            assert_eq!(
                d.info().variant,
                if dynamic {
                    "monolithicSparse"
                } else {
                    "monolithicFlat"
                }
            );
            let mut b = vec![0u8; 2100];
            d.read_at(64_950, &mut b).unwrap();
            assert!(b[..50].iter().all(|&x| x == 0));
            assert!(b[50..2050].iter().all(|&x| x == 0x5A));
            assert!(b[2050..].iter().all(|&x| x == 0));
            let mut b = [0u8; 512];
            d.read_at(size - 512, &mut b).unwrap();
            assert!(b.iter().all(|&x| x == 0x11));
            if dynamic {
                assert!(!d.is_allocated(262_144, 65_536).unwrap());
                assert!(d.is_allocated(65_536, 1).unwrap());
            }
        }
    }

    #[test]
    fn descriptor_detection() {
        let d = descriptor("monolithicFlat", 100, "FLAT", "x-flat.vmdk", Some(0));
        assert!(looks_like_descriptor(d.as_bytes()));
        assert!(!looks_like_descriptor(&[0xEB, 0x3C, 0x90, 0, 0]));
        let parsed = parse_descriptor(&d).unwrap();
        assert_eq!(parsed.extents[0].1, 100);
        assert_eq!(parsed.extents[0].3.as_deref(), Some("x-flat.vmdk"));
    }
}
