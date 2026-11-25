//! Apple disk images (UDIF .dmg, .sparseimage, .sparsebundle), built here from the
//! documented layouts (the Keramics format notes) around an HFS disk with an Apple
//! partition map, the way hdiutil lays them out.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sectorzero_core::{
    ContainerKind, CreateOptions, FsKind, NewImage, OpenOptions, PartitionScheme, Volume,
    checksum_virtual_disk, probe,
};

const S: usize = 512;

fn data() -> Vec<u8> {
    (0..200_000u32).map(|i| (i * 13 % 251) as u8).collect()
}

/// An 8 MiB HFS disk with an Apple partition map holding DATA.BIN.
fn disk(dir: &Path) -> PathBuf {
    let img = dir.join("src.img");
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 8 << 20,
        },
        &CreateOptions {
            label: Some("Apple Test".into()),
            fs: FsKind::Hfs,
            scheme: PartitionScheme::Apm,
            ..Default::default()
        },
    )
    .unwrap();
    let host = dir.join("DATA.BIN");
    fs::write(&host, data()).unwrap();
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

fn sum(p: &Path) -> String {
    checksum_virtual_disk(p, None).unwrap().sha256
}

fn data_file(img: &Path) -> Vec<u8> {
    let v = Volume::open(img, &OpenOptions::default()).unwrap();
    let mut out = vec![];
    v.copy_out("/DATA.BIN", &mut out).unwrap();
    out
}

fn base64(d: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for (i, c) in d.chunks(3).enumerate() {
        if i > 0 && i % 13 == 0 {
            s.push_str("\n\t\t\t\t");
        }
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            s.push(if k <= c.len() {
                A[(n >> (18 - 6 * k) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    s
}

/// A small ADC encoder: literal runs, and runs of one repeated byte as overlapping copies.
fn adc(d: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    let mut lit: Vec<u8> = vec![];
    let flush = |lit: &mut Vec<u8>, out: &mut Vec<u8>| {
        for c in lit.chunks(128) {
            out.push(0x80 | (c.len() - 1) as u8);
            out.extend_from_slice(c);
        }
        lit.clear();
    };
    let mut i = 0;
    while i < d.len() {
        let rep = d[i..]
            .iter()
            .take(67)
            .take_while(|&&b| i > 0 && b == d[i - 1])
            .count();
        if rep >= 4 {
            flush(&mut lit, &mut out);
            out.extend_from_slice(&[0x40 | (rep - 4) as u8, 0, 0]);
            i += rep;
        } else if rep == 3 {
            flush(&mut lit, &mut out);
            out.extend_from_slice(&[0x00, 0x00]);
            i += 3;
        } else {
            lit.push(d[i]);
            i += 1;
        }
    }
    flush(&mut lit, &mut out);
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Codec {
    Raw,
    Adc,
    Zlib,
    Bzip2,
    Lzfse,
    Xz,
}

fn compress(codec: Codec, d: &[u8]) -> (u32, Vec<u8>) {
    match codec {
        Codec::Raw => (1, d.to_vec()),
        Codec::Adc => (0x8000_0004, adc(d)),
        Codec::Zlib => {
            let mut e = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::fast());
            e.write_all(d).unwrap();
            (0x8000_0005, e.finish().unwrap())
        }
        Codec::Bzip2 => {
            let mut e = bzip2::write::BzEncoder::new(vec![], bzip2::Compression::fast());
            e.write_all(d).unwrap();
            (0x8000_0006, e.finish().unwrap())
        }
        Codec::Lzfse => {
            let mut out = vec![];
            lzfse_rust::encode_bytes(d, &mut out).unwrap();
            (0x8000_0007, out)
        }
        Codec::Xz => {
            let mut e =
                lzma_rust2::XzWriter::new(vec![], lzma_rust2::XzOptions::with_preset(1)).unwrap();
            e.write_all(d).unwrap();
            (0x8000_0008, e.finish().unwrap())
        }
    }
}

/// `mish` block tables for `disk` (one per half, like a partition map and a volume), and
/// the data fork they point into.
fn block_tables(disk: &[u8], codec: Codec, run: usize) -> (Vec<Vec<u8>>, Vec<u8>) {
    let sectors = disk.len() / S;
    let half = sectors / 2;
    let mut fork = vec![];
    let mut tables = vec![];
    for (start, end) in [(0, half), (half, sectors)] {
        let base = fork.len() as u64;
        let mut entries: Vec<[u64; 5]> = vec![]; // kind, start, count, offset, size
        // A comment entry first: readers skip it.
        entries.push([0x7fff_fffe, 0, 0, 0, 0]);
        let mut s = start;
        while s < end {
            let n = run.min(end - s);
            let chunk = &disk[s * S..(s + n) * S];
            if chunk.iter().all(|&b| b == 0) {
                // Free space (type 2) in the first table, zero fill (type 0) in the second.
                let kind = if start == 0 { 2 } else { 0 };
                entries.push([
                    kind,
                    (s - start) as u64,
                    n as u64,
                    fork.len() as u64 - base,
                    0,
                ]);
            } else {
                let (kind, z) = compress(codec, chunk);
                entries.push([
                    kind as u64,
                    (s - start) as u64,
                    n as u64,
                    fork.len() as u64 - base,
                    z.len() as u64,
                ]);
                fork.extend_from_slice(&z);
            }
            s += n;
        }
        entries.push([
            0xffff_ffff,
            (end - start) as u64,
            0,
            fork.len() as u64 - base,
            0,
        ]);
        let mut t = vec![0u8; 204];
        t[0..4].copy_from_slice(b"mish");
        t[4..8].copy_from_slice(&1u32.to_be_bytes());
        t[8..16].copy_from_slice(&(start as u64).to_be_bytes());
        t[16..24].copy_from_slice(&((end - start) as u64).to_be_bytes());
        t[24..32].copy_from_slice(&base.to_be_bytes());
        t[64..68].copy_from_slice(&2u32.to_be_bytes());
        t[68..72].copy_from_slice(&32u32.to_be_bytes());
        t[200..204].copy_from_slice(&(entries.len() as u32).to_be_bytes());
        for [kind, s, n, off, size] in entries {
            t.extend_from_slice(&(kind as u32).to_be_bytes());
            t.extend_from_slice(&0u32.to_be_bytes());
            for v in [s, n, off, size] {
                t.extend_from_slice(&v.to_be_bytes());
            }
        }
        tables.push(t);
    }
    (tables, fork)
}

fn plist_xml(tables: &[Vec<u8>]) -> Vec<u8> {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>resource-fork</key>\n\t<dict>\n\t\t<key>blkx</key>\n\t\t<array>\n",
    );
    let names = [
        "Apple Partition Map (Apple_partition_map : 1)",
        "disk image (Apple_HFS : 2)",
    ];
    for (i, t) in tables.iter().enumerate() {
        s += &format!(
            "\t\t\t<dict>\n\t\t\t\t<key>Attributes</key>\n\t\t\t\t<string>0x0050</string>\n\t\t\t\t<key>CFName</key>\n\t\t\t\t<string>{0}</string>\n\t\t\t\t<key>Data</key>\n\t\t\t\t<data>\n\t\t\t\t{1}\n\t\t\t\t</data>\n\t\t\t\t<key>ID</key>\n\t\t\t\t<string>{2}</string>\n\t\t\t\t<key>Name</key>\n\t\t\t\t<string>{0}</string>\n\t\t\t</dict>\n",
            names[i % 2],
            base64(t),
            i as i32 - 1
        );
    }
    s += "\t\t</array>\n\t\t<key>plst</key>\n\t\t<array>\n\t\t\t<dict>\n\t\t\t\t<key>Attributes</key>\n\t\t\t\t<string>0x0050</string>\n\t\t\t\t<key>Data</key>\n\t\t\t\t<data>\n\t\t\t\tAAAAAA==\n\t\t\t\t</data>\n\t\t\t\t<key>ID</key>\n\t\t\t\t<string>0</string>\n\t\t\t\t<key>Name</key>\n\t\t\t\t<string></string>\n\t\t\t</dict>\n\t\t</array>\n\t</dict>\n</dict>\n</plist>\n";
    s.into_bytes()
}

/// A classic resource fork holding the tables as `blkx` resources.
fn resource_fork(tables: &[Vec<u8>]) -> Vec<u8> {
    let mut data = vec![];
    let mut offs = vec![];
    for t in tables {
        offs.push(data.len() as u32);
        data.extend_from_slice(&(t.len() as u32).to_be_bytes());
        data.extend_from_slice(t);
    }
    let mut map = vec![0u8; 28];
    map[24..26].copy_from_slice(&28u16.to_be_bytes());
    let refs_at = 2 + 8;
    let names_at = 28 + refs_at + 12 * tables.len();
    map[26..28].copy_from_slice(&(names_at as u16).to_be_bytes());
    map.extend_from_slice(&0u16.to_be_bytes()); // one type
    map.extend_from_slice(b"blkx");
    map.extend_from_slice(&(tables.len() as u16 - 1).to_be_bytes());
    map.extend_from_slice(&(refs_at as u16).to_be_bytes());
    for (i, off) in offs.iter().enumerate() {
        map.extend_from_slice(&(i as u16).to_be_bytes());
        map.extend_from_slice(&0xffffu16.to_be_bytes());
        map.extend_from_slice(&(0x5000_0000 | off).to_be_bytes()); // attributes + offset
        map.extend_from_slice(&0u32.to_be_bytes());
    }
    let mut r = vec![0u8; 256];
    r[0..4].copy_from_slice(&256u32.to_be_bytes());
    r[4..8].copy_from_slice(&((256 + data.len()) as u32).to_be_bytes());
    r[8..12].copy_from_slice(&(data.len() as u32).to_be_bytes());
    r[12..16].copy_from_slice(&(map.len() as u32).to_be_bytes());
    r.extend_from_slice(&data);
    r.extend_from_slice(&map);
    r
}

fn koly(
    data: (u64, u64),
    rsrc: (u64, u64),
    plist: (u64, u64),
    segment: (u32, u32),
    sectors: u64,
) -> Vec<u8> {
    let mut k = vec![0u8; 512];
    k[0..4].copy_from_slice(b"koly");
    k[4..8].copy_from_slice(&4u32.to_be_bytes());
    k[8..12].copy_from_slice(&512u32.to_be_bytes());
    k[12..16].copy_from_slice(&1u32.to_be_bytes());
    k[24..32].copy_from_slice(&data.0.to_be_bytes());
    k[32..40].copy_from_slice(&data.1.to_be_bytes());
    k[40..48].copy_from_slice(&rsrc.0.to_be_bytes());
    k[48..56].copy_from_slice(&rsrc.1.to_be_bytes());
    k[56..60].copy_from_slice(&segment.0.to_be_bytes());
    k[60..64].copy_from_slice(&segment.1.to_be_bytes());
    k[64..80].copy_from_slice(b"segment-set-id-1");
    k[80..84].copy_from_slice(&2u32.to_be_bytes());
    k[84..88].copy_from_slice(&32u32.to_be_bytes());
    k[216..224].copy_from_slice(&plist.0.to_be_bytes());
    k[224..232].copy_from_slice(&plist.1.to_be_bytes());
    k[488..492].copy_from_slice(&1u32.to_be_bytes());
    k[492..500].copy_from_slice(&sectors.to_be_bytes());
    k
}

/// Write `disk` as a UDIF image at `path`; with `segment`, the data fork is cut into pieces
/// of that many bytes in `.dmgpart` files (cutting through compressed blocks).
fn write_udif(path: &Path, disk: &[u8], codec: Codec, rsrc: bool, segment: Option<usize>) {
    let (tables, fork) = block_tables(disk, codec, 128);
    let sectors = (disk.len() / S) as u64;
    let pieces: Vec<&[u8]> = match segment {
        Some(n) => fork.chunks(n).collect(),
        None => vec![&fork],
    };
    let count = pieces.len() as u32;
    let mut first = pieces[0].to_vec();
    let meta = if rsrc {
        resource_fork(&tables)
    } else {
        plist_xml(&tables)
    };
    let at = first.len() as u64;
    first.extend_from_slice(&meta);
    let m = (at, meta.len() as u64);
    let none = (0, 0);
    let (r, p) = if rsrc { (m, none) } else { (none, m) };
    first.extend(koly((0, pieces[0].len() as u64), r, p, (1, count), sectors));
    fs::write(path, first).unwrap();
    let stem = path.file_stem().unwrap().to_string_lossy();
    for (i, piece) in pieces.iter().enumerate().skip(1) {
        let mut f = piece.to_vec();
        f.extend(koly(
            (0, piece.len() as u64),
            none,
            none,
            (i as u32 + 1, count),
            sectors,
        ));
        fs::write(
            path.with_file_name(format!("{stem}.{:03}.dmgpart", i + 1)),
            f,
        )
        .unwrap();
    }
}

#[test]
fn udif_images_with_every_compression() {
    let tmp = tempfile::tempdir().unwrap();
    let img = disk(tmp.path());
    let raw = fs::read(&img).unwrap();
    let want = sum(&img);
    for (codec, variant) in [
        (Codec::Raw, "UDRO"),
        (Codec::Adc, "UDCO"),
        (Codec::Zlib, "UDZO"),
        (Codec::Bzip2, "UDBZ"),
        (Codec::Lzfse, "ULFO"),
        (Codec::Xz, "ULMO"),
    ] {
        let dmg = tmp.path().join(format!("{variant}.dmg"));
        write_udif(&dmg, &raw, codec, false, None);
        let (info, _) = probe(&dmg).unwrap();
        assert_eq!(info.kind, ContainerKind::Dmg);
        assert_eq!(info.variant, variant);
        assert_eq!(info.virtual_size, raw.len() as u64);
        assert_eq!(sum(&dmg), want, "{variant}");
        assert_eq!(data_file(&dmg), data(), "{variant}");
        let v = Volume::open(&dmg, &OpenOptions::default()).unwrap();
        assert_eq!(v.info().unwrap().label, "Apple Test");
    }
}

#[test]
fn udif_resource_fork_segments_and_damage() {
    let tmp = tempfile::tempdir().unwrap();
    let img = disk(tmp.path());
    let raw = fs::read(&img).unwrap();
    let want = sum(&img);

    // Older images keep the block tables in a resource fork.
    let old = tmp.path().join("old.dmg");
    write_udif(&old, &raw, Codec::Zlib, true, None);
    assert_eq!(sum(&old), want);

    // A segmented image, cut through compressed blocks.
    let seg = tmp.path().join("seg.dmg");
    write_udif(&seg, &raw, Codec::Zlib, false, Some(2500));
    assert!(tmp.path().join("seg.003.dmgpart").exists());
    assert_eq!(sum(&seg), want);
    assert_eq!(data_file(&seg), data());
    // A later segment is not an image of its own, and a missing one is reported.
    let err = probe(&tmp.path().join("seg.002.dmgpart")).unwrap_err();
    assert!(err.to_string().contains(".dmg"), "{err}");
    fs::remove_file(tmp.path().join("seg.003.dmgpart")).unwrap();
    let err = probe(&seg).unwrap_err();
    assert!(err.to_string().contains("seg.003.dmgpart"), "{err}");

    // Read-only.
    let dmg = tmp.path().join("ro.dmg");
    write_udif(&dmg, &raw, Codec::Zlib, false, None);
    let host = tmp.path().join("X.TXT");
    fs::write(&host, b"x").unwrap();
    let rw = OpenOptions {
        writable: true,
        ..Default::default()
    };
    let err = Volume::open(&dmg, &rw)
        .and_then(|v| v.inject(&host, "/", false))
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");

    // A damaged zlib block fails its check: an error, not wrong data.
    let mut d = fs::read(&dmg).unwrap();
    for b in &mut d[600..700] {
        *b ^= 0x5a;
    }
    fs::write(&dmg, d).unwrap();
    assert!(checksum_virtual_disk(&dmg, None).is_err());

    // Encrypted images are refused with a clear message.
    let enc = tmp.path().join("enc.dmg");
    let mut e = b"encrcdsa\0\0\0\x02".to_vec();
    e.resize(8192, 0);
    fs::write(&enc, e).unwrap();
    let err = probe(&enc).unwrap_err();
    assert!(
        err.to_string().contains("encrypted Apple disk images"),
        "{err}"
    );
}

#[test]
fn sparse_image_and_sparse_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let img = disk(tmp.path());
    let raw = fs::read(&img).unwrap();
    let want = sum(&img);
    let band = 1 << 20;
    let used: Vec<usize> = (0..raw.len() / band)
        .filter(|&b| raw[b * band..(b + 1) * band].iter().any(|&x| x != 0))
        .collect();
    assert!(used.len() >= 2 && used.len() < raw.len() / band);

    // .sparseimage: bands stored in the order they were first written (here, reversed).
    let mut sp = vec![0u8; 4096];
    sp[0..4].copy_from_slice(b"sprs");
    sp[4..8].copy_from_slice(&3u32.to_be_bytes());
    sp[8..12].copy_from_slice(&((band / S) as u32).to_be_bytes());
    sp[12..16].copy_from_slice(&1u32.to_be_bytes());
    sp[16..20].copy_from_slice(&((raw.len() / S) as u32).to_be_bytes());
    for (slot, &b) in used.iter().rev().enumerate() {
        sp[64 + slot * 4..68 + slot * 4].copy_from_slice(&(b as u32 + 1).to_be_bytes());
    }
    for &b in used.iter().rev() {
        sp.extend_from_slice(&raw[b * band..(b + 1) * band]);
    }
    let spi = tmp.path().join("disk.sparseimage");
    fs::write(&spi, &sp).unwrap();
    let (info, _) = probe(&spi).unwrap();
    assert_eq!(info.kind, ContainerKind::SparseImage);
    assert_eq!(info.virtual_size, raw.len() as u64);
    assert_eq!(sum(&spi), want);
    assert_eq!(data_file(&spi), data());

    // More than 1008 bands in use: each further header follows the last band of the one
    // before and is linked from offset 20, as hdiutil writes them (seen on macOS 26).
    let small = 4096;
    let order: Vec<usize> = (0..raw.len() / small).rev().collect();
    let mut sp = Vec::new();
    for (k, group) in order.chunks(1008).enumerate() {
        let start = sp.len();
        sp.resize(start + 4096, 0);
        let h = &mut sp[start..];
        h[0..4].copy_from_slice(b"sprs");
        h[4..8].copy_from_slice(&3u32.to_be_bytes());
        h[8..12].copy_from_slice(&((small / S) as u32).to_be_bytes());
        h[12..16].copy_from_slice(&1u32.to_be_bytes());
        h[16..20].copy_from_slice(&((raw.len() / S) as u32).to_be_bytes());
        for (slot, &b) in group.iter().enumerate() {
            h[64 + slot * 4..68 + slot * 4].copy_from_slice(&(b as u32 + 1).to_be_bytes());
        }
        for &b in group {
            sp.extend_from_slice(&raw[b * small..(b + 1) * small]);
        }
        if k + 1 < order.len().div_ceil(1008) {
            let next = sp.len() as u64;
            sp[start + 20..start + 28].copy_from_slice(&next.to_be_bytes());
        }
    }
    fs::write(&spi, &sp).unwrap();
    assert_eq!(sum(&spi), want);
    assert_eq!(data_file(&spi), data());

    // .sparsebundle: Info.plist, an empty token, and band files named in hex; the last
    // band file is shorter than the band size.
    let sb = tmp.path().join("disk.sparsebundle");
    fs::create_dir_all(sb.join("bands")).unwrap();
    fs::write(
        sb.join("Info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>CFBundleInfoDictionaryVersion</key>\n\t<string>6.0</string>\n\t<key>band-size</key>\n\t<integer>{band}</integer>\n\t<key>bundle-backingstore-version</key>\n\t<integer>1</integer>\n\t<key>diskimage-bundle-type</key>\n\t<string>com.apple.diskimage.sparsebundle</string>\n\t<key>size</key>\n\t<integer>{}</integer>\n</dict>\n</plist>\n",
            raw.len()
        ),
    )
    .unwrap();
    fs::write(sb.join("token"), b"").unwrap();
    for &b in &used {
        let mut c = &raw[b * band..(b + 1) * band];
        while c.last() == Some(&0) {
            c = &c[..c.len() - 1];
        }
        fs::write(sb.join("bands").join(format!("{b:x}")), c).unwrap();
    }
    let (info, _) = probe(&sb).unwrap();
    assert_eq!(info.kind, ContainerKind::SparseBundle);
    assert_eq!(info.virtual_size, raw.len() as u64);
    assert_eq!(sum(&sb), want);
    assert_eq!(data_file(&sb), data());

    // An encrypted bundle (a token with key data) is refused.
    fs::write(sb.join("token"), b"cdsaencr").unwrap();
    let err = probe(&sb).unwrap_err();
    assert!(
        err.to_string().contains("encrypted Apple disk images"),
        "{err}"
    );
}
