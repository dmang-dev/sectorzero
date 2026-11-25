//! Clonezilla / Rescuezilla image folders and partclone images, built here from the
//! documented layouts (partclone's IMAGE_FORMATS.md, Clonezilla's file names).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sectorzero_core::{
    ContainerKind, CreateOptions, NewImage, OpenOptions, PartitionScheme, Volume,
    checksum_virtual_disk, probe,
};

const BS: usize = 4096;

fn disk(dir: &Path, scheme: PartitionScheme) -> PathBuf {
    let img = dir.join("src.img");
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 24 << 20,
        },
        &CreateOptions {
            label: Some("CLONE".into()),
            scheme,
            ..Default::default()
        },
    )
    .unwrap();
    let host = dir.join("DATA.BIN");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    fs::write(&host, &data).unwrap();
    let rw = OpenOptions {
        writable: true,
        ..Default::default()
    };
    Volume::open(&img, &rw)
        .unwrap()
        .inject(&host, "/", false)
        .unwrap();
    img
}

/// First partition: (start sector, sectors).
fn first_part(d: &[u8]) -> (u64, u64) {
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap()) as u64;
    let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    if d[446 + 4] == 0xEE {
        let e = 1024;
        let (s, l) = (u64_at(e + 32), u64_at(e + 40));
        (s, l - s + 1)
    } else {
        (u32_at(446 + 8), u32_at(446 + 12))
    }
}

fn used_blocks(fs_bytes: &[u8]) -> Vec<bool> {
    fs_bytes
        .chunks(BS)
        .map(|c| c.iter().any(|&b| b != 0))
        .collect()
}

/// Partclone format 0002 with a checksum of `sum` bytes after every `per` blocks.
fn partclone_v2(fs_bytes: &[u8], sum: u16, per: u32) -> Vec<u8> {
    let used = used_blocks(fs_bytes);
    let mut h = vec![0u8; 110];
    h[..15].copy_from_slice(b"partclone-image");
    h[16..21].copy_from_slice(b"0.3.2");
    h[30..34].copy_from_slice(b"0002");
    h[34..36].copy_from_slice(&0xC0DEu16.to_le_bytes());
    h[36..40].copy_from_slice(b"FAT1");
    h[52..60].copy_from_slice(&(fs_bytes.len() as u64).to_le_bytes());
    h[60..68].copy_from_slice(&(used.len() as u64).to_le_bytes());
    let n = used.iter().filter(|&&u| u).count() as u64;
    h[68..76].copy_from_slice(&n.to_le_bytes());
    h[76..84].copy_from_slice(&n.to_le_bytes());
    h[84..88].copy_from_slice(&(BS as u32).to_le_bytes());
    h[88..92].copy_from_slice(&70u32.to_le_bytes());
    h[92..94].copy_from_slice(&2u16.to_le_bytes());
    h[94..96].copy_from_slice(&64u16.to_le_bytes());
    h[96..98].copy_from_slice(&(if sum == 0 { 0u16 } else { 1 }).to_le_bytes());
    h[98..100].copy_from_slice(&sum.to_le_bytes());
    h[100..104].copy_from_slice(&per.to_le_bytes());
    h[104] = 1;
    h[105] = 1;
    let crc = crc32fast::hash(&h[..106]);
    h[106..110].copy_from_slice(&crc.to_le_bytes());
    let mut bitmap = vec![0u8; used.len().div_ceil(8)];
    for (i, &u) in used.iter().enumerate() {
        if u {
            bitmap[i / 8] |= 1 << (i % 8);
        }
    }
    let crc = crc32fast::hash(&bitmap);
    h.extend_from_slice(&bitmap);
    h.extend_from_slice(&crc.to_le_bytes());
    let mut k = 0;
    for (i, c) in fs_bytes.chunks(BS).enumerate() {
        if !used[i] {
            continue;
        }
        h.extend_from_slice(c);
        k += 1;
        if sum > 0 && k % per == 0 {
            h.extend_from_slice(&vec![0xCC; sum as usize]);
        }
    }
    if sum > 0 && k % per != 0 {
        h.extend_from_slice(&vec![0xCC; sum as usize]);
    }
    h
}

/// Partclone format 0001: a byte per block, `BiTmAgIc`, a CRC32 after every block.
fn partclone_v1(fs_bytes: &[u8]) -> Vec<u8> {
    let used = used_blocks(fs_bytes);
    let mut h = vec![0u8; 4160];
    h[..15].copy_from_slice(b"partclone-image");
    h[15..19].copy_from_slice(b"FAT1");
    h[30..34].copy_from_slice(b"0001");
    h[36..40].copy_from_slice(&(BS as u32).to_le_bytes());
    h[40..48].copy_from_slice(&(fs_bytes.len() as u64).to_le_bytes());
    h[48..56].copy_from_slice(&(used.len() as u64).to_le_bytes());
    let n = used.iter().filter(|&&u| u).count() as u64;
    h[56..64].copy_from_slice(&n.to_le_bytes());
    h.extend(used.iter().map(|&u| u as u8));
    h.extend_from_slice(b"BiTmAgIc");
    for (i, c) in fs_bytes.chunks(BS).enumerate() {
        if used[i] {
            h.extend_from_slice(c);
            h.extend_from_slice(&crc32fast::hash(c).to_le_bytes());
        }
    }
    h
}

fn gzip(d: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
    e.write_all(d).unwrap();
    e.finish().unwrap()
}

/// Write `data` as `name.aa`, `name.ab`, … pieces of `piece` bytes.
fn split(dir: &Path, name: &str, data: &[u8], piece: usize) {
    for (i, c) in data.chunks(piece).enumerate() {
        let suffix = format!(
            "{}{}",
            (b'a' + (i / 26) as u8) as char,
            (b'a' + (i % 26) as u8) as char
        );
        fs::write(dir.join(format!("{name}.{suffix}")), c).unwrap();
    }
}

fn sfdisk(label: &str, start: u64, size: u64, last_lba: Option<u64>) -> String {
    let mut s = format!("label: {label}\nlabel-id: 0x1234abcd\ndevice: /dev/sda\nunit: sectors\n");
    if let Some(l) = last_lba {
        s += &format!("first-lba: 34\nlast-lba: {l}\n");
    }
    s += &format!("sector-size: 512\n\n/dev/sda1 : start= {start:>11}, size= {size:>11}, type=c\n");
    s
}

fn sum(p: &Path, member: Option<&str>) -> String {
    checksum_virtual_disk(p, member).unwrap().sha256
}

fn data_file(img: &Path) -> Vec<u8> {
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let mut out = vec![];
    v.copy_out("/DATA.BIN", &mut out).unwrap();
    out
}

#[test]
fn mbr_disk_with_split_gzip_partclone() {
    let tmp = tempfile::tempdir().unwrap();
    let img = disk(tmp.path(), PartitionScheme::Mbr);
    let d = fs::read(&img).unwrap();
    let (start, count) = first_part(&d);
    let (s, e) = ((start * 512) as usize, ((start + count) * 512) as usize);
    let cz = tmp.path().join("2026-11-24-10-img_sda");
    fs::create_dir(&cz).unwrap();
    fs::write(cz.join("disk"), "sda\n").unwrap();
    fs::write(cz.join("parts"), "sda1\n").unwrap();
    fs::write(cz.join("clonezilla-img"), "This is a Clonezilla image.\n").unwrap();
    fs::write(cz.join("sda-pt.sf"), sfdisk("dos", start, count, None)).unwrap();
    fs::write(
        cz.join("sda-pt.parted"),
        format!(
            "Model: test\nDisk /dev/sda: {}s\nSector size (logical/physical): 512B/512B\n",
            d.len() / 512
        ),
    )
    .unwrap();
    fs::write(cz.join("sda-mbr"), &d[..512]).unwrap();
    fs::write(cz.join("sda-hidden-data-after-mbr"), &d[512..s]).unwrap();
    let pc = gzip(&partclone_v2(&d[s..e], 4, 3));
    split(&cz, "sda1.vfat-ptcl-img.gz", &pc, 3000);
    assert!(cz.join("sda1.vfat-ptcl-img.gz.ab").exists());

    let (info, _) = probe(&cz).unwrap();
    assert_eq!(info.kind, ContainerKind::Clonezilla);
    assert_eq!(info.variant, "disk");
    assert_eq!(info.virtual_size, d.len() as u64);
    assert_eq!(info.compression.as_deref(), Some("gzip"));
    assert_eq!(sum(&cz, None), sum(&img, None));
    assert_eq!(data_file(&cz), data_file(&img));

    // Read-only.
    let host = tmp.path().join("X.TXT");
    fs::write(&host, b"x").unwrap();
    let rw = OpenOptions {
        writable: true,
        ..Default::default()
    };
    let err = Volume::open(&cz, &rw)
        .and_then(|v| v.inject(&host, "/", false))
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");

    // The partclone piece on its own opens too (gzip around a partclone image).
    let one = tmp.path().join("sda1.vfat-ptcl-img.gz");
    fs::write(&one, &pc).unwrap();
    let (info, _) = probe(&one).unwrap();
    assert_eq!(info.kind, ContainerKind::Partclone);
    assert_eq!(info.compression.as_deref(), Some("gzip"));
    assert_eq!(info.virtual_size, (e - s) as u64);
}

#[test]
fn gpt_disk_with_partclone_v1_uncompressed() {
    let tmp = tempfile::tempdir().unwrap();
    let img = disk(tmp.path(), PartitionScheme::Gpt);
    let d = fs::read(&img).unwrap();
    let (start, count) = first_part(&d);
    let (s, e) = ((start * 512) as usize, ((start + count) * 512) as usize);
    let cz = tmp.path().join("gpt-img");
    fs::create_dir(&cz).unwrap();
    fs::write(cz.join("disk"), "sda").unwrap();
    fs::write(cz.join("parts"), "sda1").unwrap();
    let last = (d.len() / 512 - 34) as u64;
    fs::write(
        cz.join("sda-pt.sf"),
        sfdisk("gpt", start, count, Some(last)),
    )
    .unwrap();
    fs::write(cz.join("sda-mbr"), &d[..512]).unwrap();
    fs::write(cz.join("sda-gpt-1st"), &d[..34 * 512]).unwrap();
    fs::write(cz.join("sda-gpt-2nd"), &d[d.len() - 33 * 512..]).unwrap();
    split(
        &cz,
        "sda1.vfat-ptcl-img.uncomp",
        &partclone_v1(&d[s..e]),
        1 << 20,
    );

    assert_eq!(probe(&cz).unwrap().0.virtual_size, d.len() as u64);
    assert_eq!(probe(&cz).unwrap().0.compression, None);
    assert_eq!(sum(&cz, None), sum(&img, None));
    assert_eq!(data_file(&cz), data_file(&img));
}

#[test]
fn partition_backups_and_other_image_types() {
    let tmp = tempfile::tempdir().unwrap();
    let img = disk(tmp.path(), PartitionScheme::Mbr);
    let d = fs::read(&img).unwrap();
    let (start, count) = first_part(&d);
    let part = &d[(start * 512) as usize..((start + count) * 512) as usize];
    let part_img = tmp.path().join("part.img");
    fs::write(&part_img, part).unwrap();

    // "saveparts": no disk file; each partition is a member. A zstd partclone image
    // without checksums, a gzip dd image, and an ntfsclone image (not supported).
    let cz = tmp.path().join("parts-img");
    fs::create_dir(&cz).unwrap();
    fs::write(cz.join("parts"), "sda1 sdb2 sdc1").unwrap();
    fs::write(cz.join("clonezilla-img"), "").unwrap();
    let z = zstd::bulk::compress(&partclone_v2(part, 0, 256), 3).unwrap();
    split(&cz, "sda1.vfat-ptcl-img.zst", &z, 1 << 30);
    split(&cz, "sdb2.dd-img.gz", &gzip(part), 50_000);
    split(
        &cz,
        "sdc1.ntfs-img.gz",
        &gzip(b"\0ntfsclone-image\x0a\x00"),
        1 << 20,
    );

    let (info, _) = probe(&cz).unwrap();
    assert_eq!(info.variant, "partition");
    assert_eq!(info.members, ["sda1", "sdb2", "sdc1"]);
    assert_eq!(info.member.as_deref(), Some("sda1"));
    assert_eq!(info.compression.as_deref(), Some("zstd"));
    assert_eq!(sum(&cz, None), sum(&part_img, None));
    assert_eq!(sum(&cz, Some("sdb2")), sum(&part_img, None));
    let v = Volume::open(
        &cz,
        &OpenOptions {
            member: Some("2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(v.info().unwrap().label, "CLONE");
    let err = checksum_virtual_disk(&cz, Some("sdc1")).unwrap_err();
    assert!(err.to_string().contains("ntfsclone"), "{err}");

    // A plain uncompressed partclone file.
    let pc = tmp.path().join("p.ptcl");
    fs::write(&pc, partclone_v2(part, 4, 256)).unwrap();
    assert_eq!(sum(&pc, None), sum(&part_img, None));
    assert_eq!(probe(&pc).unwrap().0.variant, "v2 FAT1");
}
