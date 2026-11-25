//! VirtualBox Virtual Disk Image (VDI): fixed, dynamic ("normal") and differencing (snapshots),
//! from the published description of the VDI 1.1 header.
//!
//! Layout: a 64-byte text pre-header, the signature 0xBEDA107F at 0x40, the header, a block
//! map of little-endian u32 entries (the block's index in the data area, or "free" / "zero"),
//! then the data blocks, each preceded by `block_extra` bytes of service data.
//!
//! Differencing and undo VDIs name their parent only by UUID (the parent's creation UUID is
//! the child's linkage UUID); the path lives in VirtualBox's media registry. The parent is
//! looked for among the .vdi files next to the child and in the folder above (VirtualBox
//! keeps snapshots in the VM's `Snapshots` folder), and through the disks a `.vbox` file in
//! those folders lists. Free blocks of a child come from the parent; writing one copies the
//! parent's block in first. The first write to any VDI gives it a new modification UUID, as
//! VirtualBox does.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::{ContainerInfo, ContainerKind, Disk, open_parent, read_up_to};
use crate::error::{Error, Result};
use crate::util::*;

const SIGNATURE: u32 = 0xBEDA_107F;
const VERSION_1_1: u32 = 0x0001_0001;
const HEADER_SIZE: u32 = 0x190;
const BLOCK_FREE: u32 = 0xFFFF_FFFF;
const BLOCK_ZERO: u32 = 0xFFFF_FFFE;
const TYPE_NORMAL: u32 = 1;
const TYPE_FIXED: u32 = 2;
const TYPE_UNDO: u32 = 3;
const TYPE_DIFF: u32 = 4;
const DEFAULT_BLOCK: u32 = 1024 * 1024;
const PRE_HEADER: &[u8] = b"<<< Oracle VM VirtualBox Disk Image >>>\n";
/// Where new images put the block map, and how the data area is aligned.
const MAP_OFFSET: u64 = 0x200;
const DATA_ALIGN: u64 = 1024 * 1024;

// Header field offsets (absolute, version 1.1).
const H_TYPE: usize = 0x4C;
const H_OFFSET_MAP: usize = 0x154;
const H_OFFSET_DATA: usize = 0x158;
const H_LEGACY_SECTOR: usize = 0x168;
const H_DISK_SIZE: usize = 0x170;
const H_BLOCK_SIZE: usize = 0x178;
const H_BLOCK_EXTRA: usize = 0x17C;
const H_BLOCKS: usize = 0x180;
const H_ALLOCATED: usize = 0x184;
const H_UUID_CREATE: usize = 0x188;
const H_UUID_MODIFY: usize = 0x198;
const H_UUID_LINKAGE: usize = 0x1A8;
const H_LCHS_SECTOR: usize = 0x1D4;

pub(crate) fn looks_like_vdi(head: &[u8]) -> bool {
    head.len() >= 0x48 && le32(head, 0x40) == SIGNATURE
}

pub(crate) struct VdiDisk {
    file: File,
    size: u64,
    block_size: u64,
    block_extra: u64,
    map_offset: u64,
    data_offset: u64,
    map: Vec<u32>,
    allocated: u32,
    fixed: bool,
    writable: bool,
    /// Set once this session has given the image a new modification UUID.
    modified: bool,
    /// Differencing VDIs: the parent that supplies the free blocks.
    parent: Option<Box<dyn Disk>>,
    parent_path: Option<PathBuf>,
}

/// The creation UUID of the VDI at `path`, if it is one.
fn uuid_create(path: &Path) -> Option<[u8; 16]> {
    let mut f = File::open(path).ok()?;
    let mut h = [0u8; 0x1D8];
    (read_up_to(&mut f, 0, &mut h).ok()? == h.len() && looks_like_vdi(&h))
        .then(|| h[H_UUID_CREATE..H_UUID_CREATE + 16].try_into().unwrap())
}

/// Disk paths listed in `.vbox` files in `dir` for the media UUID `uuid`.
fn vbox_locations(dir: &Path, uuid: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("vbox"))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        for tag in text.split("<HardDisk ").skip(1) {
            let attr = |name: &str| -> Option<String> {
                let at = tag.find(&format!("{name}=\""))? + name.len() + 2;
                let end = tag[at..].find('"')?;
                Some(tag[at..at + end].to_string())
            };
            if attr("uuid").is_some_and(|u| u.trim_matches(['{', '}']).eq_ignore_ascii_case(uuid))
                && let Some(loc) = attr("location")
            {
                out.push(dir.join(loc.replace("&amp;", "&")));
            }
        }
    }
    out
}

/// Find the VDI whose creation UUID is `linkage`: next to `child`, in the folder above, or
/// where a `.vbox` file there says it is.
fn find_parent(child: &Path, linkage: &[u8; 16]) -> Result<PathBuf> {
    let dir = child
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let mut dirs = vec![dir.clone()];
    if let Some(up) = std::fs::canonicalize(&dir)
        .ok()
        .and_then(|d| d.parent().map(Path::to_path_buf))
    {
        dirs.push(up);
    }
    let uuid = guid_string(linkage);
    let me = std::fs::canonicalize(child).ok();
    for d in &dirs {
        let mut candidates = vbox_locations(d, &uuid);
        if let Ok(entries) = std::fs::read_dir(d) {
            candidates.extend(
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vdi"))),
            );
        }
        for c in candidates {
            if std::fs::canonicalize(&c).ok() != me && uuid_create(&c).as_ref() == Some(linkage) {
                return Ok(c);
            }
        }
    }
    Err(Error::Invalid(format!(
        "parent disk of the differencing VDI not found (looked for UUID {uuid} next to it and \
         in the folder above)"
    )))
}

impl VdiDisk {
    pub(crate) fn open(path: &Path, writable: bool) -> Result<Self> {
        let mut file = File::options().read(true).write(writable).open(path)?;
        let mut h = [0u8; 512];
        let got = read_up_to(&mut file, 0, &mut h)?;
        if got < 0x1D8 || !looks_like_vdi(&h) {
            return Err(Error::Invalid("VDI header not found".into()));
        }
        let version = le32(&h, 0x44);
        if version >> 16 != 1 {
            return Err(Error::Unsupported(format!(
                "VDI version {}.{} (only 1.x)",
                version >> 16,
                version & 0xFFFF
            )));
        }
        let image_type = le32(&h, H_TYPE);
        let fixed = match image_type {
            TYPE_NORMAL | TYPE_UNDO | TYPE_DIFF => false,
            TYPE_FIXED => true,
            t => return Err(Error::Invalid(format!("unknown VDI image type {t}"))),
        };
        let size = le64(&h, H_DISK_SIZE);
        let block_size = u64::from(le32(&h, H_BLOCK_SIZE));
        let block_extra = u64::from(le32(&h, H_BLOCK_EXTRA));
        let blocks = le32(&h, H_BLOCKS);
        if block_size == 0 || !block_size.is_multiple_of(512) {
            return Err(Error::Invalid("VDI block size is invalid".into()));
        }
        if u64::from(blocks) * block_size < size || blocks > 64 * 1024 * 1024 {
            return Err(Error::Invalid(
                "VDI block count does not cover the disk".into(),
            ));
        }
        let map_offset = u64::from(le32(&h, H_OFFSET_MAP));
        let data_offset = u64::from(le32(&h, H_OFFSET_DATA));
        let mut raw = vec![0u8; blocks as usize * 4];
        read_up_to(&mut file, map_offset, &mut raw)?;
        let map = raw.chunks(4).map(|c| le32(c, 0)).collect();
        let (parent, parent_path) = if matches!(image_type, TYPE_UNDO | TYPE_DIFF) {
            let linkage: [u8; 16] = h[H_UUID_LINKAGE..H_UUID_LINKAGE + 16].try_into().unwrap();
            let found = find_parent(path, &linkage)?;
            (Some(open_parent(&found)?), Some(found))
        } else {
            (None, None)
        };
        Ok(VdiDisk {
            file,
            size,
            block_size,
            block_extra,
            map_offset,
            data_offset,
            map,
            allocated: le32(&h, H_ALLOCATED),
            fixed,
            writable,
            modified: false,
            parent,
            parent_path,
        })
    }

    pub(crate) fn create(path: &Path, size: u64, dynamic: bool) -> Result<Self> {
        let block_size = u64::from(DEFAULT_BLOCK);
        let blocks = size.div_ceil(block_size);
        if blocks > u64::from(u32::MAX / 2) {
            return Err(Error::Invalid("disk too large for VDI".into()));
        }
        let blocks = blocks as u32;
        let data_offset = round_up(MAP_OFFSET + u64::from(blocks) * 4, DATA_ALIGN);
        if data_offset > u64::from(u32::MAX) {
            return Err(Error::Invalid("disk too large for VDI".into()));
        }
        let mut h = vec![0u8; MAP_OFFSET as usize];
        h[..PRE_HEADER.len()].copy_from_slice(PRE_HEADER);
        put_le32(&mut h, 0x40, SIGNATURE);
        put_le32(&mut h, 0x44, VERSION_1_1);
        put_le32(&mut h, 0x48, HEADER_SIZE);
        put_le32(
            &mut h,
            H_TYPE,
            if dynamic { TYPE_NORMAL } else { TYPE_FIXED },
        );
        put_le32(&mut h, H_OFFSET_MAP, MAP_OFFSET as u32);
        put_le32(&mut h, H_OFFSET_DATA, data_offset as u32);
        put_le32(&mut h, H_LEGACY_SECTOR, 512);
        put_le64(&mut h, H_DISK_SIZE, size);
        put_le32(&mut h, H_BLOCK_SIZE, DEFAULT_BLOCK);
        put_le32(&mut h, H_BLOCKS, blocks);
        let allocated = if dynamic { 0 } else { blocks };
        put_le32(&mut h, H_ALLOCATED, allocated);
        h[H_UUID_CREATE..H_UUID_CREATE + 16].copy_from_slice(&new_guid());
        h[H_UUID_MODIFY..H_UUID_MODIFY + 16].copy_from_slice(&new_guid());
        put_le32(&mut h, H_LCHS_SECTOR, 512);
        let map: Vec<u32> = if dynamic {
            vec![BLOCK_FREE; blocks as usize]
        } else {
            (0..blocks).collect()
        };
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(&h)?;
        let raw: Vec<u8> = map.iter().flat_map(|e| e.to_le_bytes()).collect();
        file.write_all(&raw)?;
        let end = if dynamic {
            data_offset
        } else {
            data_offset + u64::from(blocks) * block_size
        };
        file.set_len(end)?;
        Ok(VdiDisk {
            file,
            size,
            block_size,
            block_extra: 0,
            map_offset: MAP_OFFSET,
            data_offset,
            map,
            allocated,
            fixed: !dynamic,
            writable: true,
            modified: true,
            parent: None,
            parent_path: None,
        })
    }

    fn block_start(&self, index: u32) -> u64 {
        self.data_offset
            + u64::from(index) * (self.block_size + self.block_extra)
            + self.block_extra
    }

    /// Before the first change: a new modification UUID, so snapshots made from this image
    /// can tell it changed.
    fn mark_modified(&mut self) -> io::Result<()> {
        if !self.modified && self.writable {
            self.file.seek(SeekFrom::Start(H_UUID_MODIFY as u64))?;
            self.file.write_all(&new_guid())?;
            self.modified = true;
        }
        Ok(())
    }

    /// Give `block` the next slot of the data area (zero-filled) and record it.
    fn allocate(&mut self, block: usize) -> io::Result<u32> {
        let index = self.allocated;
        let end = self.block_start(index) + self.block_size;
        if self.file.metadata()?.len() < end {
            self.file.set_len(end)?;
        }
        self.map[block] = index;
        self.file
            .seek(SeekFrom::Start(self.map_offset + block as u64 * 4))?;
        self.file.write_all(&index.to_le_bytes())?;
        self.allocated += 1;
        self.file.seek(SeekFrom::Start(H_ALLOCATED as u64))?;
        self.file.write_all(&self.allocated.to_le_bytes())?;
        Ok(index)
    }
}

impl Disk for VdiDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            if offset >= self.size {
                buf.fill(0);
                return Ok(());
            }
            let block = (offset / self.block_size) as usize;
            let within = offset % self.block_size;
            let n = (buf.len() as u64)
                .min(self.block_size - within)
                .min(self.size - offset) as usize;
            let (chunk, rest) = buf.split_at_mut(n);
            match self.map.get(block).copied().unwrap_or(BLOCK_FREE) {
                BLOCK_FREE if self.parent.is_some() => {
                    self.parent.as_mut().unwrap().read_at(offset, chunk)?;
                }
                e if e >= BLOCK_ZERO => chunk.fill(0),
                e => {
                    let at = self.block_start(e) + within;
                    let got = read_up_to(&mut self.file, at, chunk)?;
                    chunk[got..].fill(0);
                }
            }
            offset += n as u64;
            buf = rest;
        }
        Ok(())
    }

    fn write_at(&mut self, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the VDI",
            ));
        }
        while !buf.is_empty() {
            let block = (offset / self.block_size) as usize;
            let within = offset % self.block_size;
            let n = (buf.len() as u64).min(self.block_size - within) as usize;
            let mut entry = self.map[block];
            if entry == BLOCK_FREE && self.parent.is_some() {
                // Copy the parent's block in before part of it is overwritten.
                self.mark_modified()?;
                let mut data = vec![0u8; self.block_size as usize];
                let start = block as u64 * self.block_size;
                self.parent.as_mut().unwrap().read_at(start, &mut data)?;
                entry = self.allocate(block)?;
                self.file.seek(SeekFrom::Start(self.block_start(entry)))?;
                self.file.write_all(&data)?;
            } else if entry >= BLOCK_ZERO {
                if is_zero(&buf[..n]) {
                    offset += n as u64;
                    buf = &buf[n..];
                    continue;
                }
                entry = self.allocate(block)?;
            }
            self.mark_modified()?;
            self.file
                .seek(SeekFrom::Start(self.block_start(entry) + within))?;
            self.file.write_all(&buf[..n])?;
            offset += n as u64;
            buf = &buf[n..];
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn info(&self) -> ContainerInfo {
        let mut info = ContainerInfo::simple(
            ContainerKind::Vdi,
            if self.parent.is_some() {
                "differencing"
            } else if self.fixed {
                "fixed"
            } else {
                "dynamic"
            },
            self.size,
        );
        info.parent = self.parent_path.as_ref().map(|p| p.display().to_string());
        info
    }

    fn is_allocated(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let first = offset / self.block_size;
        let last = (offset + len.max(1) - 1) / self.block_size;
        let mut inherits = false;
        for b in first..=last {
            match self.map.get(b as usize).copied() {
                Some(e) if e < BLOCK_ZERO => return Ok(true),
                Some(BLOCK_FREE) => inherits = true,
                _ => {}
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
    fn fixed_and_dynamic_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let size = 5 * 1024 * 1024 + 512;
        for dynamic in [false, true] {
            let path = tmp.path().join(format!("d{dynamic}.vdi"));
            let data: Vec<u8> = (0..300_000u32).map(|i| (i % 249) as u8 + 1).collect();
            {
                let mut d = VdiDisk::create(&path, size, dynamic).unwrap();
                d.write_at(1024 * 1024 - 1000, &data).unwrap();
                d.write_at(size - 512, &[9u8; 512]).unwrap();
                d.write_at(3 * 1024 * 1024, &[0u8; 4096]).unwrap();
                d.sync().unwrap();
            }
            let head = std::fs::read(&path).unwrap();
            assert!(looks_like_vdi(&head));
            assert!(head.starts_with(PRE_HEADER));
            let mut d = VdiDisk::open(&path, false).unwrap();
            assert_eq!(d.size(), size);
            assert_eq!(d.info().variant, if dynamic { "dynamic" } else { "fixed" });
            let mut got = vec![0u8; data.len()];
            d.read_at(1024 * 1024 - 1000, &mut got).unwrap();
            assert_eq!(got, data);
            let mut s = [0u8; 512];
            d.read_at(size - 512, &mut s).unwrap();
            assert_eq!(s, [9u8; 512]);
            if dynamic {
                // Blocks 0, 1 and 5 hold data; the zero write left block 3 unallocated.
                assert_eq!(d.allocated, 3);
                assert!(!d.is_allocated(3 * 1024 * 1024, 4096).unwrap());
                assert!(d.is_allocated(0, 1024 * 1024).unwrap());
                assert_eq!(
                    std::fs::metadata(&path).unwrap().len(),
                    d.data_offset + 3 * 1024 * 1024
                );
            } else {
                assert!(d.is_allocated(3 * 1024 * 1024, 4096).unwrap());
            }
        }
    }

    #[test]
    fn differencing_reads_the_parent_and_copies_up() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base.vdi");
        {
            let mut d = VdiDisk::create(&base, 4 << 20, true).unwrap();
            d.write_at(0, &[1u8; 3 << 20]).unwrap();
        }
        // A child as VirtualBox makes them: type 4, linkage = the parent's creation UUID.
        let snaps = tmp.path().join("Snapshots");
        std::fs::create_dir_all(&snaps).unwrap();
        let child = snaps.join("{child}.vdi");
        drop(VdiDisk::create(&child, 4 << 20, true).unwrap());
        let linkage = uuid_create(&base).unwrap();
        let mut f = File::options().write(true).open(&child).unwrap();
        f.seek(SeekFrom::Start(H_TYPE as u64)).unwrap();
        f.write_all(&TYPE_DIFF.to_le_bytes()).unwrap();
        f.seek(SeekFrom::Start(H_UUID_LINKAGE as u64)).unwrap();
        f.write_all(&linkage).unwrap();
        drop(f);

        let mut d = VdiDisk::open(&child, true).unwrap();
        assert_eq!(d.info().variant, "differencing");
        d.write_at((1 << 20) + 100, &[7u8; 10]).unwrap();
        let mut got = vec![0u8; 4 << 20];
        d.read_at(0, &mut got).unwrap();
        assert!(got[..(1 << 20) + 100].iter().all(|&b| b == 1));
        assert_eq!(&got[(1 << 20) + 100..(1 << 20) + 110], &[7u8; 10]);
        assert!(got[(1 << 20) + 110..3 << 20].iter().all(|&b| b == 1));
        assert!(got[3 << 20..].iter().all(|&b| b == 0));
        assert_eq!(d.allocated, 1, "only the written block is copied up");
        let mut b = VdiDisk::open(&base, false).unwrap();
        let mut one = [0u8; 10];
        b.read_at((1 << 20) + 100, &mut one).unwrap();
        assert_eq!(one, [1u8; 10], "parent untouched");
    }
}
