//! PC-98 emulator images: a header in front of plain sectors.
//!
//! - NHD (T98-Next hard disk): 512-byte header starting `T98HDDIMAGE.R0`, then header size,
//!   cylinders (u32), heads (u16), sectors per track (u16) and sector size (u16) at 0x110.
//! - HDI (Anex86 hard disk) and FDI (Anex86 floppy): eight little-endian u32s — reserved,
//!   drive type, header size, data size, sector size, sectors, heads, cylinders — in a header
//!   that is usually 4096 bytes long.
//!
//! HDI/FDI have no signature, so they are only recognised when the file has that extension
//! and every header field is consistent (see `anex_header`).

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use super::{ContainerInfo, ContainerKind, Disk, read_up_to};
use crate::error::{Error, Result};
use crate::util::*;

const NHD_MAGIC: &[u8] = b"T98HDDIMAGE.R0\0";
const NHD_HEADER: u64 = 512;
const ANEX_HEADER: u64 = 4096;

pub(crate) fn looks_like_nhd(head: &[u8]) -> bool {
    head.starts_with(NHD_MAGIC)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub cylinders: u32,
    pub heads: u32,
    pub sectors: u32,
    pub sector_size: u32,
}

impl Geometry {
    fn bytes(&self) -> u64 {
        u64::from(self.cylinders)
            * u64::from(self.heads)
            * u64::from(self.sectors)
            * u64::from(self.sector_size)
    }
}

fn plausible_sector_size(s: u32) -> bool {
    matches!(s, 128 | 256 | 512 | 1024 | 2048)
}

/// Header offset and geometry of an Anex86 HDI/FDI header, or `None` unless every field is
/// sane: reserved word zero, header size a multiple of 256 inside the file, a usual sector
/// size, a sane geometry, and a data size equal to cylinders × heads × sectors × sector size.
pub(crate) fn anex_header(head: &[u8], file_len: u64) -> Option<(u64, Geometry)> {
    if head.len() < 32 || le32(head, 0) != 0 {
        return None;
    }
    let header = u64::from(le32(head, 8));
    let data = u64::from(le32(head, 12));
    let g = Geometry {
        sector_size: le32(head, 16),
        sectors: le32(head, 20),
        heads: le32(head, 24),
        cylinders: le32(head, 28),
    };
    let ok = (256..=1 << 20).contains(&header)
        && header.is_multiple_of(256)
        && file_len >= header
        && data > 0
        && plausible_sector_size(g.sector_size)
        && (1..=255).contains(&g.sectors)
        && (1..=255).contains(&g.heads)
        && (1..=65535).contains(&g.cylinders)
        && g.bytes() == data;
    ok.then_some((header, g))
}

fn nhd_header(head: &[u8]) -> Result<(u64, Geometry)> {
    if head.len() < 0x11E || !looks_like_nhd(head) {
        return Err(Error::Invalid("NHD header not found".into()));
    }
    let header = u64::from(le32(head, 0x110));
    let g = Geometry {
        cylinders: le32(head, 0x114),
        heads: u32::from(le16(head, 0x118)),
        sectors: u32::from(le16(head, 0x11A)),
        sector_size: u32::from(le16(head, 0x11C)),
    };
    if header < 0x120 || !plausible_sector_size(g.sector_size) || g.bytes() == 0 {
        return Err(Error::Invalid("NHD header is inconsistent".into()));
    }
    Ok((header, g))
}

/// A geometry for a new image of `size` bytes (cylinders × heads × sectors must cover it
/// exactly). Standard PC-98 and PC floppies get their real geometry.
fn geometry_for(kind: ContainerKind, size: u64) -> Result<Geometry> {
    let known = |c, h, s, ss| Geometry {
        cylinders: c,
        heads: h,
        sectors: s,
        sector_size: ss,
    };
    if kind == ContainerKind::Fdi {
        match size {
            1_261_568 => return Ok(known(77, 2, 8, 1024)), // PC-98 2HD
            1_474_560 => return Ok(known(80, 2, 18, 512)),
            1_228_800 => return Ok(known(80, 2, 15, 512)),
            737_280 => return Ok(known(80, 2, 9, 512)),
            655_360 => return Ok(known(80, 2, 8, 512)), // PC-98 2DD
            _ => {}
        }
    }
    let total = size / 512;
    // SASI (8×17), SCSI (8×32), large (16×63) layouts first, then whatever divides.
    let candidates: &[(u32, u32)] = &[
        (8, 17),
        (8, 32),
        (16, 63),
        (16, 32),
        (4, 17),
        (2, 18),
        (2, 15),
        (2, 9),
        (2, 8),
        (1, 8),
        (1, 1),
    ];
    for &(h, s) in candidates {
        let per_cyl = u64::from(h * s);
        if total.is_multiple_of(per_cyl) && total / per_cyl <= 65535 {
            return Ok(known((total / per_cyl) as u32, h, s, 512));
        }
    }
    // Large odd sizes: NHD's cylinder field is 32-bit, Anex86's data size is too.
    if kind == ContainerKind::Nhd && total <= u64::from(u32::MAX) {
        return Ok(known(total as u32, 1, 1, 512));
    }
    Err(Error::Invalid(format!(
        "no {} geometry fits {size} bytes (use a multiple of 8 × 17 × 512 bytes)",
        kind.name()
    )))
}

pub(crate) struct Pc98Disk {
    kind: ContainerKind,
    file: File,
    data_offset: u64,
    size: u64,
    geometry: Geometry,
}

impl Pc98Disk {
    /// Open an NHD (by signature) or an HDI/FDI (`kind`, already validated by the caller
    /// with `anex_header`).
    pub(crate) fn open(path: &Path, kind: ContainerKind, writable: bool) -> Result<Self> {
        let mut file = File::options().read(true).write(writable).open(path)?;
        let len = file.metadata()?.len();
        let mut head = [0u8; 512];
        let got = read_up_to(&mut file, 0, &mut head)?;
        let head = &head[..got];
        let (data_offset, geometry) = if kind == ContainerKind::Nhd {
            nhd_header(head)?
        } else {
            anex_header(head, len)
                .ok_or_else(|| Error::Invalid(format!("{} header is invalid", kind.name())))?
        };
        Ok(Pc98Disk {
            kind,
            file,
            data_offset,
            size: geometry.bytes(),
            geometry,
        })
    }

    pub(crate) fn create(path: &Path, kind: ContainerKind, size: u64) -> Result<Self> {
        let g = geometry_for(kind, size)?;
        let (header, data_offset) = if kind == ContainerKind::Nhd {
            let mut h = vec![0u8; NHD_HEADER as usize];
            h[..NHD_MAGIC.len()].copy_from_slice(NHD_MAGIC);
            put_le32(&mut h, 0x110, NHD_HEADER as u32);
            put_le32(&mut h, 0x114, g.cylinders);
            put_le16(&mut h, 0x118, g.heads as u16);
            put_le16(&mut h, 0x11A, g.sectors as u16);
            put_le16(&mut h, 0x11C, g.sector_size as u16);
            (h, NHD_HEADER)
        } else {
            if size > u64::from(u32::MAX) {
                return Err(Error::Invalid(format!(
                    "{} images are limited to 4 GiB",
                    kind.name()
                )));
            }
            let mut h = vec![0u8; ANEX_HEADER as usize];
            let drive_type = match (kind, size) {
                (ContainerKind::Hdi, _) => 0,
                (_, 1_474_560) => 0x30,
                (_, s) if s <= 800 * 1024 => 0x10,
                _ => 0x90,
            };
            put_le32(&mut h, 4, drive_type);
            put_le32(&mut h, 8, ANEX_HEADER as u32);
            put_le32(&mut h, 12, size as u32);
            put_le32(&mut h, 16, g.sector_size);
            put_le32(&mut h, 20, g.sectors);
            put_le32(&mut h, 24, g.heads);
            put_le32(&mut h, 28, g.cylinders);
            (h, ANEX_HEADER)
        };
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(&header)?;
        file.set_len(data_offset + size)?;
        Ok(Pc98Disk {
            kind,
            file,
            data_offset,
            size,
            geometry: g,
        })
    }
}

impl Disk for Pc98Disk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let n = if offset < self.size {
            (buf.len() as u64).min(self.size - offset) as usize
        } else {
            0
        };
        let got = read_up_to(&mut self.file, self.data_offset + offset, &mut buf[..n])?;
        buf[got..].fill(0);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the disk",
            ));
        }
        self.file.seek(SeekFrom::Start(self.data_offset + offset))?;
        self.file.write_all(buf)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn info(&self) -> ContainerInfo {
        let g = self.geometry;
        ContainerInfo::simple(
            self.kind,
            &format!(
                "{}/{}/{} CHS, {}-byte sectors",
                g.cylinders, g.heads, g.sectors, g.sector_size
            ),
            self.size,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_write_reopen_each_kind() {
        let tmp = tempfile::tempdir().unwrap();
        for (kind, name, size, header) in [
            (ContainerKind::Nhd, "a.nhd", 8 * 17 * 512 * 20, 512),
            (ContainerKind::Hdi, "a.hdi", 8 * 17 * 512 * 20, 4096),
            (ContainerKind::Fdi, "a.fdi", 1_261_568, 4096),
            (ContainerKind::Fdi, "b.fdi", 1_474_560, 4096),
        ] {
            let path = tmp.path().join(name);
            {
                let mut d = Pc98Disk::create(&path, kind, size).unwrap();
                d.write_at(0, &[0x5A; 1000]).unwrap();
                d.write_at(size - 4, &[1, 2, 3, 4]).unwrap();
                assert!(d.write_at(size - 2, &[0; 4]).is_err());
            }
            let raw = std::fs::read(&path).unwrap();
            assert_eq!(raw.len() as u64, header + size, "{name}");
            assert_eq!(raw[header as usize], 0x5A);
            if kind == ContainerKind::Nhd {
                assert!(looks_like_nhd(&raw));
            } else {
                assert_eq!(anex_header(&raw, raw.len() as u64).unwrap().0, header);
            }
            let mut d = Pc98Disk::open(&path, kind, false).unwrap();
            assert_eq!(d.size(), size, "{name}");
            let mut b = [0u8; 8];
            d.read_at(size - 8, &mut b).unwrap();
            assert_eq!(b, [0, 0, 0, 0, 1, 2, 3, 4]);
            assert_eq!(d.info().kind, kind);
        }
        let d = Pc98Disk::open(&tmp.path().join("a.fdi"), ContainerKind::Fdi, false).unwrap();
        assert_eq!(d.info().variant, "77/2/8 CHS, 1024-byte sectors");
    }

    #[test]
    fn anex_detection_does_not_misfire_on_raw_images() {
        // A FAT boot sector, an all-zero image, and a header with a wrong data size.
        let mut boot = vec![0u8; 4096];
        boot[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
        assert!(anex_header(&boot, 1_474_560).is_none());
        assert!(anex_header(&[0u8; 4096], 1_474_560).is_none());
        let mut h = vec![0u8; 4096];
        for (i, v) in [0u32, 0x90, 4096, 1_261_567, 1024, 8, 2, 77]
            .into_iter()
            .enumerate()
        {
            put_le32(&mut h, i * 4, v);
        }
        assert!(anex_header(&h, 4096 + 1_261_568).is_none());
        put_le32(&mut h, 12, 1_261_568);
        assert!(anex_header(&h, 4096 + 1_261_568).is_some());
        assert!(
            anex_header(&h, 100).is_none(),
            "header larger than the file"
        );
    }
}
