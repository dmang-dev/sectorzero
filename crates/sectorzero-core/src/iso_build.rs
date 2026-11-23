//! Build an ISO 9660 image from a host folder: ISO 9660 level 1 (8.3) names plus a Joliet
//! tree with long names, both path tables, and an optional El Torito boot entry
//! (floppy emulation or no emulation, BIOS or UEFI).

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::util::*;

const SECTOR: u64 = 2048;
const MAX_FILE: u64 = 0xFFFF_F800;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BootPlatform {
    #[default]
    Bios,
    Uefi,
}

#[derive(Debug, Clone, Default)]
pub struct IsoBuildOptions {
    /// Volume label (up to 32 characters; ISO 9660 keeps A–Z, 0–9 and `_`).
    pub volume_id: String,
    /// Boot image for El Torito: a 1.2/1.44/2.88 MB floppy image (emulated) or a
    /// no-emulation loader / EFI system partition image.
    pub boot_image: Option<PathBuf>,
    pub boot_platform: BootPlatform,
    /// Force no emulation even for a floppy-sized boot image.
    pub no_emulation: bool,
    /// 512-byte sectors the BIOS loads (no emulation). Default 4, as mkisofs does.
    pub load_sectors: Option<u16>,
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct IsoBuildReport {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub image_size: u64,
    pub bootable: bool,
}

struct Node {
    host: PathBuf,
    name: String,
    is_dir: bool,
    size: u64,
    mtime: SystemTime,
    children: Vec<usize>,
    parent: usize,
    iso_name: String,
    joliet_name: Vec<u16>,
    /// LBA of the file data, or of the directory in the ISO / Joliet tree.
    lba: u32,
    iso_lba: u32,
    iso_size: u32,
    joliet_lba: u32,
    joliet_size: u32,
}

fn scan(root: &Path) -> Result<Vec<Node>> {
    let meta = fs::metadata(root)?;
    if !meta.is_dir() {
        return Err(Error::Invalid(format!(
            "{} is not a folder",
            root.display()
        )));
    }
    let mut nodes = vec![node(root.to_path_buf(), String::new(), &meta, 0)];
    let mut i = 0;
    while i < nodes.len() {
        if nodes[i].is_dir {
            let mut kids: Vec<_> = fs::read_dir(&nodes[i].host)?.collect::<io::Result<_>>()?;
            kids.sort_by_key(|k| k.file_name());
            for k in kids {
                let m = fs::metadata(k.path())?;
                if !m.is_dir() && m.len() > MAX_FILE {
                    return Err(Error::Unsupported(format!(
                        "files of 4 GiB or more in ISO images ({})",
                        k.path().display()
                    )));
                }
                let n = node(
                    k.path(),
                    k.file_name().to_string_lossy().into_owned(),
                    &m,
                    i,
                );
                nodes.push(n);
                let id = nodes.len() - 1;
                nodes[i].children.push(id);
            }
        }
        i += 1;
    }
    Ok(nodes)
}

fn node(host: PathBuf, name: String, m: &fs::Metadata, parent: usize) -> Node {
    Node {
        host,
        name,
        is_dir: m.is_dir(),
        size: if m.is_dir() { 0 } else { m.len() },
        mtime: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        children: vec![],
        parent,
        iso_name: String::new(),
        joliet_name: vec![],
        lba: 0,
        iso_lba: 0,
        iso_size: 0,
        joliet_lba: 0,
        joliet_size: 0,
    }
}

fn d_chars(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| c.to_ascii_uppercase())
        .map(|c| {
            if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(max)
        .collect()
}

/// Unique ISO 9660 level-1 names (`NAME.EXT;1`, folders `NAME`) within each folder.
fn assign_iso_names(nodes: &mut [Node]) {
    for d in 0..nodes.len() {
        let kids = nodes[d].children.clone();
        let mut used = HashSet::new();
        for k in kids {
            let n = &nodes[k];
            let (base, ext) = match (n.is_dir, n.name.rfind('.')) {
                (false, Some(i)) if i > 0 => (&n.name[..i], &n.name[i + 1..]),
                _ => (n.name.as_str(), ""),
            };
            let base = {
                let b = d_chars(base, 8);
                if b.is_empty() { "_".to_string() } else { b }
            };
            let ext = d_chars(ext, 3);
            let make = |b: &str| {
                if nodes[k].is_dir {
                    b.to_string()
                } else {
                    format!("{b}.{ext}")
                }
            };
            let mut name = make(&base);
            let mut i = 1;
            while used.contains(&name) {
                let suffix = format!("~{i}");
                let keep = 8usize.saturating_sub(suffix.len()).min(base.len());
                name = make(&format!("{}{suffix}", &base[..keep]));
                i += 1;
            }
            used.insert(name.clone());
            nodes[k].iso_name = if nodes[k].is_dir {
                name
            } else {
                format!("{name};1")
            };
        }
    }
}

fn assign_joliet_names(nodes: &mut [Node]) {
    for d in 0..nodes.len() {
        let kids = nodes[d].children.clone();
        let mut used = HashSet::new();
        for k in kids {
            let clean: String = nodes[k]
                .name
                .chars()
                .map(|c| {
                    if "*/:;?\\".contains(c) || c < ' ' {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let mut units: Vec<u16> = clean.encode_utf16().take(64).collect();
            let mut i = 1;
            while used.contains(&units) {
                let suffix: Vec<u16> = format!("~{i}").encode_utf16().collect();
                units = clean
                    .encode_utf16()
                    .take(64 - suffix.len())
                    .chain(suffix)
                    .collect();
                i += 1;
            }
            used.insert(units.clone());
            if !nodes[k].is_dir {
                units.extend(";1".encode_utf16());
            }
            nodes[k].joliet_name = units;
        }
    }
}

fn name_bytes(n: &Node, joliet: bool) -> Vec<u8> {
    if joliet {
        n.joliet_name.iter().flat_map(|u| u.to_be_bytes()).collect()
    } else {
        n.iso_name.as_bytes().to_vec()
    }
}

fn rec_len(name_len: usize) -> usize {
    33 + name_len + (1 - name_len % 2)
}

/// Children in on-disc order (sorted by identifier, as ISO 9660 requires).
fn sorted_children(nodes: &[Node], d: usize, joliet: bool) -> Vec<usize> {
    let mut kids = nodes[d].children.clone();
    kids.sort_by(|&a, &b| name_bytes(&nodes[a], joliet).cmp(&name_bytes(&nodes[b], joliet)));
    kids
}

/// Directory size in bytes: records may not cross sector boundaries.
fn dir_size(nodes: &[Node], d: usize, joliet: bool) -> u64 {
    let mut lens = vec![34usize, 34];
    lens.extend(
        sorted_children(nodes, d, joliet)
            .iter()
            .map(|&k| rec_len(name_bytes(&nodes[k], joliet).len())),
    );
    let mut pos = 0u64;
    for l in lens {
        let l = l as u64;
        if pos % SECTOR + l > SECTOR {
            pos = round_up(pos, SECTOR);
        }
        pos += l;
    }
    round_up(pos, SECTOR)
}

/// Directories in path-table order (breadth first, children sorted).
fn dir_order(nodes: &[Node], joliet: bool) -> Vec<usize> {
    let mut order = vec![0usize];
    let mut i = 0;
    while i < order.len() {
        for k in sorted_children(nodes, order[i], joliet) {
            if nodes[k].is_dir {
                order.push(k);
            }
        }
        i += 1;
    }
    order
}

fn both16(b: &mut [u8], at: usize, v: u16) {
    put_le16(b, at, v);
    put_be16(b, at + 2, v);
}

fn both32(b: &mut [u8], at: usize, v: u32) {
    put_le32(b, at, v);
    put_be32(b, at + 4, v);
}

fn rec_date(t: SystemTime) -> [u8; 7] {
    let d: DateTime<Utc> = t.into();
    let y = (d.year() - 1900).clamp(0, 255) as u8;
    [
        y,
        d.month() as u8,
        d.day() as u8,
        d.hour() as u8,
        d.minute() as u8,
        d.second() as u8,
        0,
    ]
}

fn dir_record(lba: u32, size: u32, date: [u8; 7], is_dir: bool, name: &[u8]) -> Vec<u8> {
    let len = rec_len(name.len());
    let mut r = vec![0u8; len];
    r[0] = len as u8;
    both32(&mut r, 2, lba);
    both32(&mut r, 10, size);
    r[18..25].copy_from_slice(&date);
    r[25] = if is_dir { 2 } else { 0 };
    both16(&mut r, 28, 1);
    r[32] = name.len() as u8;
    r[33..33 + name.len()].copy_from_slice(name);
    r
}

fn pad_text(b: &mut [u8], s: &str, joliet: bool) {
    if joliet {
        let mut units: Vec<u16> = s.encode_utf16().collect();
        units.resize(b.len() / 2, 0x20);
        for (i, u) in units.iter().take(b.len() / 2).enumerate() {
            put_be16(b, i * 2, *u);
        }
    } else {
        b.fill(b' ');
        let n = s.len().min(b.len());
        b[..n].copy_from_slice(&s.as_bytes()[..n]);
    }
}

fn vd_date(t: SystemTime) -> [u8; 17] {
    let d: DateTime<Utc> = t.into();
    let s = format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}00",
        d.year(),
        d.month(),
        d.day(),
        d.hour(),
        d.minute(),
        d.second()
    );
    let mut out = [0u8; 17];
    out[..16].copy_from_slice(s.as_bytes());
    out
}

fn path_table(nodes: &[Node], order: &[usize], joliet: bool, big_endian: bool) -> Vec<u8> {
    let mut t = Vec::new();
    for &d in order {
        let name = if d == 0 {
            vec![0u8]
        } else {
            name_bytes(&nodes[d], joliet)
        };
        let lba = if joliet {
            nodes[d].joliet_lba
        } else {
            nodes[d].iso_lba
        };
        let parent_num = order
            .iter()
            .position(|&x| x == nodes[d].parent)
            .unwrap_or(0) as u16
            + 1;
        let mut e = vec![0u8; 8 + name.len() + name.len() % 2];
        e[0] = name.len() as u8;
        if big_endian {
            put_be32(&mut e, 2, lba);
            put_be16(&mut e, 6, parent_num);
        } else {
            put_le32(&mut e, 2, lba);
            put_le16(&mut e, 6, parent_num);
        }
        e[8..8 + name.len()].copy_from_slice(&name);
        t.extend(e);
    }
    t
}

struct Boot {
    data: Vec<u8>,
    media: u8,
    sectors: u16,
    platform: u8,
    lba: u32,
}

/// Build `out` from the contents of `source` (the folder itself is the root).
pub fn build_iso(source: &Path, out: &Path, opts: &IsoBuildOptions) -> Result<IsoBuildReport> {
    if !opts.overwrite && out.exists() {
        return Err(Error::Exists(out.display().to_string()));
    }
    let mut nodes = scan(source)?;
    assign_iso_names(&mut nodes);
    assign_joliet_names(&mut nodes);

    let mut boot = match &opts.boot_image {
        None => None,
        Some(p) => {
            let data = fs::read(p)?;
            if data.is_empty() {
                return Err(Error::Invalid("the boot image is empty".into()));
            }
            let media = match (opts.no_emulation, data.len()) {
                (false, 1_228_800) => 1,
                (false, 1_474_560) => 2,
                (false, 2_949_120) => 3,
                _ => 0,
            };
            let sectors = if media != 0 {
                1
            } else if opts.boot_platform == BootPlatform::Uefi {
                (data.len().div_ceil(512)).min(0xFFFF) as u16
            } else {
                opts.load_sectors
                    .unwrap_or_else(|| (data.len().div_ceil(512)).min(4) as u16)
            };
            let platform = if opts.boot_platform == BootPlatform::Uefi {
                0xEF
            } else {
                0
            };
            Some(Boot {
                data,
                media,
                sectors,
                platform,
                lba: 0,
            })
        }
    };

    // Allocation: 16 system sectors, descriptors, catalog, path tables, directories, data.
    let descriptors = 2 + u64::from(boot.is_some()) + 1; // PVD, SVD, [boot record], terminator
    let mut lba = 16 + descriptors;
    let catalog_lba = boot.as_ref().map(|_| {
        lba += 1;
        (lba - 1) as u32
    });
    let iso_order = dir_order(&nodes, false);
    let jol_order = dir_order(&nodes, true);
    // Path table sizes depend only on names, so compute them with placeholder LBAs.
    let iso_pt_len = path_table(&nodes, &iso_order, false, false).len() as u64;
    let jol_pt_len = path_table(&nodes, &jol_order, true, false).len() as u64;
    let pt = |len: u64, lba: &mut u64| {
        let at = *lba;
        *lba += len.div_ceil(SECTOR).max(1);
        at as u32
    };
    let (iso_l, iso_m) = (pt(iso_pt_len, &mut lba), pt(iso_pt_len, &mut lba));
    let (jol_l, jol_m) = (pt(jol_pt_len, &mut lba), pt(jol_pt_len, &mut lba));
    for &d in &iso_order {
        let size = dir_size(&nodes, d, false);
        nodes[d].iso_lba = lba as u32;
        nodes[d].iso_size = size as u32;
        lba += size / SECTOR;
    }
    for &d in &jol_order {
        let size = dir_size(&nodes, d, true);
        nodes[d].joliet_lba = lba as u32;
        nodes[d].joliet_size = size as u32;
        lba += size / SECTOR;
    }
    if let Some(b) = boot.as_mut() {
        b.lba = lba as u32;
        lba += (b.data.len() as u64).div_ceil(SECTOR);
    }
    for n in nodes.iter_mut().filter(|n| !n.is_dir) {
        n.lba = if n.size == 0 { 0 } else { lba as u32 };
        lba += n.size.div_ceil(SECTOR);
    }
    let total = lba;
    if total > u64::from(u32::MAX) {
        return Err(Error::Unsupported("ISO images larger than 8 TiB".into()));
    }

    let result = write_image(
        out,
        &nodes,
        &boot,
        opts,
        total,
        catalog_lba,
        [iso_l, iso_m, jol_l, jol_m],
        (&iso_order, &jol_order),
    );
    if let Err(e) = result {
        let _ = fs::remove_file(out);
        return Err(e);
    }
    let files = nodes.iter().filter(|n| !n.is_dir).count() as u64;
    Ok(IsoBuildReport {
        files,
        dirs: nodes.len() as u64 - files - 1,
        bytes: nodes.iter().map(|n| n.size).sum(),
        image_size: total * SECTOR,
        bootable: boot.is_some(),
    })
}

#[allow(clippy::too_many_arguments)]
fn write_image(
    out: &Path,
    nodes: &[Node],
    boot: &Option<Boot>,
    opts: &IsoBuildOptions,
    total: u64,
    catalog_lba: Option<u32>,
    tables: [u32; 4],
    orders: (&[usize], &[usize]),
) -> Result<()> {
    let mut f = BufWriter::new(File::create(out)?);
    let now = SystemTime::now();
    let sector = |f: &mut BufWriter<File>, lba: u64, data: &[u8]| -> io::Result<()> {
        f.seek(SeekFrom::Start(lba * SECTOR))?;
        f.write_all(data)
    };
    f.get_ref().set_len(total * SECTOR)?;

    // Volume descriptors.
    let volume_id = if opts.volume_id.trim().is_empty() {
        "CDROM".to_string()
    } else {
        opts.volume_id.trim().to_string()
    };
    let mut vd_lba = 16;
    for joliet in [false, true] {
        let mut d = vec![0u8; SECTOR as usize];
        d[0] = if joliet { 2 } else { 1 };
        d[1..6].copy_from_slice(b"CD001");
        d[6] = 1;
        pad_text(&mut d[8..40], "", joliet);
        if joliet {
            pad_text(
                &mut d[40..72],
                &volume_id.chars().take(16).collect::<String>(),
                true,
            );
            d[88..91].copy_from_slice(b"%/E");
        } else {
            pad_text(&mut d[40..72], &d_chars(&volume_id, 32), false);
        }
        both32(&mut d, 80, total as u32);
        both16(&mut d, 120, 1);
        both16(&mut d, 124, 1);
        both16(&mut d, 128, SECTOR as u16);
        let order = if joliet { orders.1 } else { orders.0 };
        let pt_len = path_table(nodes, order, joliet, false).len() as u32;
        both32(&mut d, 132, pt_len);
        let (l, m) = if joliet {
            (tables[2], tables[3])
        } else {
            (tables[0], tables[1])
        };
        put_le32(&mut d, 140, l);
        put_be32(&mut d, 148, m);
        let (root_lba, root_size) = if joliet {
            (nodes[0].joliet_lba, nodes[0].joliet_size)
        } else {
            (nodes[0].iso_lba, nodes[0].iso_size)
        };
        d[156..190].copy_from_slice(&dir_record(root_lba, root_size, rec_date(now), true, &[0]));
        for (a, b) in [
            (190, 318),
            (318, 446),
            (446, 574),
            (702, 739),
            (739, 776),
            (776, 813),
        ] {
            pad_text(&mut d[a..b], "", joliet);
        }
        pad_text(&mut d[574..702], "SECTORZERO", joliet);
        let date = vd_date(now);
        d[813..830].copy_from_slice(&date);
        d[830..847].copy_from_slice(&date);
        d[847..863].copy_from_slice(b"0000000000000000");
        d[864..880].copy_from_slice(b"0000000000000000");
        d[881] = 1;
        sector(&mut f, vd_lba, &d)?;
        vd_lba += 1;
        if !joliet && let Some(cat) = catalog_lba {
            let mut b = vec![0u8; SECTOR as usize];
            b[1..6].copy_from_slice(b"CD001");
            b[6] = 1;
            b[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
            put_le32(&mut b, 71, cat);
            sector(&mut f, vd_lba, &b)?;
            vd_lba += 1;
        }
    }
    let mut term = vec![0u8; SECTOR as usize];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    sector(&mut f, vd_lba, &term)?;

    // Boot catalog and image.
    if let (Some(cat), Some(b)) = (catalog_lba, boot) {
        let mut c = vec![0u8; SECTOR as usize];
        c[0] = 1;
        c[1] = b.platform;
        c[4..14].copy_from_slice(b"SECTORZERO");
        c[30] = 0x55;
        c[31] = 0xAA;
        let sum = (0..16).fold(0u16, |s, i| s.wrapping_add(le16(&c, i * 2)));
        put_le16(&mut c, 28, 0u16.wrapping_sub(sum));
        c[32] = 0x88;
        c[33] = b.media;
        put_le16(&mut c, 38, b.sectors);
        put_le32(&mut c, 40, b.lba);
        sector(&mut f, u64::from(cat), &c)?;
        sector(&mut f, u64::from(b.lba), &b.data)?;
    }

    // Path tables.
    for (joliet, l, m) in [(false, tables[0], tables[1]), (true, tables[2], tables[3])] {
        let order = if joliet { orders.1 } else { orders.0 };
        sector(
            &mut f,
            u64::from(l),
            &path_table(nodes, order, joliet, false),
        )?;
        sector(
            &mut f,
            u64::from(m),
            &path_table(nodes, order, joliet, true),
        )?;
    }

    // Directories.
    for joliet in [false, true] {
        let order = if joliet { orders.1 } else { orders.0 };
        for &d in order {
            let (lba, size) = if joliet {
                (nodes[d].joliet_lba, nodes[d].joliet_size)
            } else {
                (nodes[d].iso_lba, nodes[d].iso_size)
            };
            let p = nodes[d].parent;
            let (plba, psize) = if joliet {
                (nodes[p].joliet_lba, nodes[p].joliet_size)
            } else {
                (nodes[p].iso_lba, nodes[p].iso_size)
            };
            let mut recs = vec![
                dir_record(lba, size, rec_date(nodes[d].mtime), true, &[0]),
                dir_record(plba, psize, rec_date(nodes[p].mtime), true, &[1]),
            ];
            for k in sorted_children(nodes, d, joliet) {
                let n = &nodes[k];
                let (l, s) = if n.is_dir {
                    if joliet {
                        (n.joliet_lba, n.joliet_size)
                    } else {
                        (n.iso_lba, n.iso_size)
                    }
                } else {
                    (n.lba, n.size as u32)
                };
                recs.push(dir_record(
                    l,
                    s,
                    rec_date(n.mtime),
                    n.is_dir,
                    &name_bytes(n, joliet),
                ));
            }
            let mut buf = vec![0u8; size as usize];
            let mut pos = 0usize;
            for r in recs {
                if pos % SECTOR as usize + r.len() > SECTOR as usize {
                    pos = round_up(pos as u64, SECTOR) as usize;
                }
                buf[pos..pos + r.len()].copy_from_slice(&r);
                pos += r.len();
            }
            sector(&mut f, u64::from(lba), &buf)?;
        }
    }

    // File data.
    let mut buf = vec![0u8; 1024 * 1024];
    for n in nodes.iter().filter(|n| !n.is_dir && n.size > 0) {
        f.seek(SeekFrom::Start(u64::from(n.lba) * SECTOR))?;
        let mut src = File::open(&n.host)?;
        let mut left = n.size;
        while left > 0 {
            let want = (buf.len() as u64).min(left) as usize;
            let got = src.read(&mut buf[..want])?;
            if got == 0 {
                return Err(Error::Invalid(format!(
                    "{} changed size while building the ISO",
                    n.host.display()
                )));
            }
            f.write_all(&buf[..got])?;
            left -= got as u64;
        }
    }
    f.flush()?;
    f.get_ref().sync_all()?;
    Ok(())
}
