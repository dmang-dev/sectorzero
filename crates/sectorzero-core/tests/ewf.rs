//! EnCase / Expert Witness evidence files (.E01, SMART .s01), built here from the documented
//! layout (libyal's EWF format document) around a FAT disk with an MBR. The ewfacquire
//! cross-check in CI (.github/oracle/ewfacquire.sh) covers files made by a real tool.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sectorzero_core::{
    ContainerKind, CreateOptions, FsKind, NewImage, OpenOptions, PartitionScheme, Volume,
    checksum_virtual_disk, probe,
};

const S: usize = 512;

fn data() -> Vec<u8> {
    (0..300_000u32).map(|i| (i * 7 % 253) as u8).collect()
}

/// A 4 MiB FAT disk with an MBR holding DATA.BIN (and its zero-filled free space, so
/// chunks both compress and do not).
fn disk(dir: &Path) -> PathBuf {
    let img = dir.join("src.img");
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 4 << 20,
        },
        &CreateOptions {
            label: Some("EWF TEST".into()),
            fs: FsKind::Fat,
            scheme: PartitionScheme::Mbr,
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

fn adler(d: &[u8]) -> u32 {
    let mut a = adler2::Adler32::new();
    a.write_slice(d);
    a.checksum()
}

fn zlib(d: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(d).unwrap();
    e.finish().unwrap()
}

/// Append a section (76-byte descriptor, then the payload) and return its offset. The next
/// offset points just past it; `next` and `done` point at themselves.
fn section(f: &mut Vec<u8>, kind: &str, payload: &[u8]) -> usize {
    let at = f.len();
    let mut d = [0u8; 76];
    d[..kind.len()].copy_from_slice(kind.as_bytes());
    let end = if payload.is_empty() && (kind == "next" || kind == "done") {
        at
    } else {
        at + 76 + payload.len()
    };
    d[16..24].copy_from_slice(&(end as u64).to_le_bytes());
    let size = if end == at { 0 } else { 76 + payload.len() };
    d[24..32].copy_from_slice(&(size as u64).to_le_bytes());
    let c = adler(&d[..72]);
    d[72..76].copy_from_slice(&c.to_le_bytes());
    f.extend_from_slice(&d);
    f.extend_from_slice(payload);
    at
}

#[derive(Clone, Copy, Default)]
struct Opts {
    smart: bool,
    /// Sectors per chunk.
    spc: usize,
    compress: bool,
    /// Chunks per segment file (0: all in one).
    per_segment: usize,
    /// EnCase 6 style: table offsets relative to a base offset.
    base: bool,
    /// Break the first table's checksum (the reader must use table2).
    bad_table: bool,
}

fn volume(o: &Opts, raw: &[u8]) -> Vec<u8> {
    let chunk = o.spc * S;
    let chunks = raw.len().div_ceil(chunk) as u32;
    let mut v = vec![0u8; if o.smart { 94 } else { 1052 }];
    if !o.smart {
        v[0] = 0x01; // fixed disk
    } else {
        v[0..4].copy_from_slice(&1u32.to_le_bytes());
    }
    v[4..8].copy_from_slice(&chunks.to_le_bytes());
    v[8..12].copy_from_slice(&(o.spc as u32).to_le_bytes());
    v[12..16].copy_from_slice(&(S as u32).to_le_bytes());
    if o.smart {
        v[16..20].copy_from_slice(&((raw.len() / S) as u32).to_le_bytes());
        v[85..90].copy_from_slice(b"SMART");
        let c = adler(&v[..90]);
        v[90..94].copy_from_slice(&c.to_le_bytes());
    } else {
        v[16..24].copy_from_slice(&((raw.len() / S) as u64).to_le_bytes());
        v[36] = 0x02; // physical
        v[52] = if o.compress { 2 } else { 0 };
        v[64..80].copy_from_slice(&[0x5a; 16]);
        let c = adler(&v[..1048]);
        v[1048..1052].copy_from_slice(&c.to_le_bytes());
    }
    v
}

fn table(entries: &[u32], base: u64) -> Vec<u8> {
    let mut t = vec![0u8; 24];
    t[0..4].copy_from_slice(&(entries.len() as u32).to_le_bytes());
    t[8..16].copy_from_slice(&base.to_le_bytes());
    let c = adler(&t[..20]);
    t[20..24].copy_from_slice(&c.to_le_bytes());
    let e: Vec<u8> = entries.iter().flat_map(|e| e.to_le_bytes()).collect();
    t.extend_from_slice(&e);
    t.extend_from_slice(&adler(&e).to_le_bytes());
    t
}

/// Write `raw` as an evidence file set named `name` (with its extension, `.E01` or `.s01`)
/// and return the first file.
fn write_ewf(dir: &Path, name: &str, raw: &[u8], o: Opts) -> PathBuf {
    let chunk = o.spc * S;
    let chunks: Vec<&[u8]> = raw.chunks(chunk).collect();
    let per = if o.per_segment == 0 {
        chunks.len()
    } else {
        o.per_segment
    };
    let groups: Vec<&[&[u8]]> = chunks.chunks(per).collect();
    let first = dir.join(name);
    for (k, group) in groups.iter().enumerate() {
        let n = k + 1;
        let mut f = Vec::new();
        f.extend_from_slice(b"EVF\x09\x0d\x0a\xff\x00\x01");
        f.extend_from_slice(&(n as u16).to_le_bytes());
        f.extend_from_slice(&[0, 0]);
        if n == 1 {
            let text = "1\r\nmain\r\nc\tn\ta\te\tt\tm\tu\tp\tr\r\n42\t1\tTest image\tExaminer\tNotes\t2026 11 25 8 0 0\t2026 11 25 8 0 0\t0\tb\r\n\r\n";
            section(&mut f, "header", &zlib(text.as_bytes()));
            section(&mut f, "volume", &volume(&o, raw));
        } else if !o.smart {
            section(&mut f, "data", &volume(&o, raw));
        }
        if o.smart {
            // SMART: the chunks follow the table entries inside the table section, and the
            // table has no footer (as ewfacquire writes it).
            let start = f.len() + 76 + 24 + group.len() * 4;
            let mut payload = Vec::new();
            let mut entries = Vec::new();
            for c in group.iter() {
                entries.push((start + payload.len()) as u32 | 1 << 31);
                payload.extend_from_slice(&zlib(c));
            }
            let mut t = table(&entries, 0);
            t.truncate(t.len() - 4);
            t.extend_from_slice(&payload);
            section(&mut f, "table", &t);
            section(&mut f, if n == groups.len() { "done" } else { "next" }, &[]);
            let ext = name.rsplit('.').next().unwrap();
            fs::write(first.with_extension(format!("{}{n:02}", &ext[..1])), &f).unwrap();
            continue;
        }
        // The sectors section: chunk data.
        let mut payload = Vec::new();
        let mut offsets = Vec::new();
        for c in group.iter() {
            let z = zlib(c);
            if o.compress && z.len() < c.len() {
                offsets.push((payload.len(), true));
                payload.extend_from_slice(&z);
            } else {
                offsets.push((payload.len(), false));
                payload.extend_from_slice(c);
                payload.extend_from_slice(&adler(c).to_le_bytes());
            }
        }
        let sectors = section(&mut f, "sectors", &payload) + 76;
        let base = if o.base { sectors as u64 } else { 0 };
        let entries: Vec<u32> = offsets
            .iter()
            .map(|&(off, z)| ((sectors + off) as u64 - base) as u32 | if z { 1 << 31 } else { 0 })
            .collect();
        let t = table(&entries, base);
        let at = section(&mut f, "table", &t);
        if o.bad_table {
            f[at + 76 + 20] ^= 0xff;
        }
        section(&mut f, "table2", &t);
        section(&mut f, if n == groups.len() { "done" } else { "next" }, &[]);
        let ext = name.rsplit('.').next().unwrap();
        let seg_ext = format!("{}{n:02}", &ext[..1]);
        fs::write(first.with_extension(seg_ext), &f).unwrap();
    }
    first
}

#[test]
fn e01_compressed_stored_and_segmented() {
    let tmp = tempfile::tempdir().unwrap();
    let img = disk(tmp.path());
    let raw = fs::read(&img).unwrap();
    let want = sum(&img);

    // One segment, zlib chunks of 64 sectors, EnCase 6 base offsets.
    let e = write_ewf(
        tmp.path(),
        "one.E01",
        &raw,
        Opts {
            spc: 64,
            compress: true,
            base: true,
            ..Default::default()
        },
    );
    let (info, _) = probe(&e).unwrap();
    assert_eq!(info.kind, ContainerKind::Ewf);
    assert_eq!(info.variant, "EnCase");
    assert_eq!(info.virtual_size, raw.len() as u64);
    assert_eq!(sum(&e), want);
    assert_eq!(data_file(&e), data());

    // Stored chunks of 8 sectors, 50 per segment file (21 files), file-relative offsets.
    let e = write_ewf(
        tmp.path(),
        "many.E01",
        &raw,
        Opts {
            spc: 8,
            per_segment: 50,
            ..Default::default()
        },
    );
    assert!(tmp.path().join("many.E21").exists());
    assert_eq!(sum(&e), want);
    assert_eq!(data_file(&e), data());

    // Opening a later segment, or a set with one missing, is refused.
    let err = probe(&tmp.path().join("many.E02")).unwrap_err().to_string();
    assert!(err.contains("later segment"), "{err}");
    fs::rename(tmp.path().join("many.E07"), tmp.path().join("away")).unwrap();
    let err = probe(&e).unwrap_err().to_string();
    assert!(err.contains("many.E07"), "{err}");
    fs::rename(tmp.path().join("away"), tmp.path().join("many.E07")).unwrap();

    // A damaged table is read from its table2 copy.
    let e = write_ewf(
        tmp.path(),
        "copy.E01",
        &raw,
        Opts {
            spc: 64,
            compress: true,
            bad_table: true,
            ..Default::default()
        },
    );
    assert_eq!(sum(&e), want);

    // A size that is not a whole number of chunks.
    let mut odd = raw.clone();
    odd.extend((0..7 * S).map(|i| (i % 251) as u8));
    let odd_img = tmp.path().join("odd.img");
    fs::write(&odd_img, &odd).unwrap();
    let e = write_ewf(
        tmp.path(),
        "odd.E01",
        &odd,
        Opts {
            spc: 64,
            compress: true,
            ..Default::default()
        },
    );
    assert_eq!(sum(&e), sum(&odd_img));
}

#[test]
fn smart_s01() {
    let tmp = tempfile::tempdir().unwrap();
    let img = disk(tmp.path());
    let raw = fs::read(&img).unwrap();
    let e = write_ewf(
        tmp.path(),
        "disk.s01",
        &raw,
        Opts {
            smart: true,
            spc: 64,
            compress: true,
            per_segment: 30,
            ..Default::default()
        },
    );
    assert!(tmp.path().join("disk.s03").exists());
    let (info, _) = probe(&e).unwrap();
    assert_eq!(info.kind, ContainerKind::Ewf);
    assert_eq!(info.variant, "SMART");
    assert_eq!(sum(&e), sum(&img));
    assert_eq!(data_file(&e), data());
}

#[test]
fn damage_and_unsupported_kinds() {
    let tmp = tempfile::tempdir().unwrap();
    let img = disk(tmp.path());
    let raw = fs::read(&img).unwrap();
    let opts = Opts {
        spc: 64,
        ..Default::default()
    };
    let e = write_ewf(tmp.path(), "bad.E01", &raw, opts);
    let mut f = fs::read(&e).unwrap();
    // Somewhere in the stored data of DATA.BIN (past the MBR gap and the FAT).
    let at = f.len() / 3;
    f[at] ^= 0x01;
    fs::write(&e, &f).unwrap();
    let err = checksum_virtual_disk(&e, None).unwrap_err().to_string();
    assert!(err.contains("damaged chunk"), "{err}");

    // A damaged section descriptor.
    let e = write_ewf(tmp.path(), "desc.E01", &raw, opts);
    let mut f = fs::read(&e).unwrap();
    f[13 + 20] ^= 0x01;
    fs::write(&e, &f).unwrap();
    let err = probe(&e).unwrap_err().to_string();
    assert!(err.contains("damaged EnCase image"), "{err}");

    // Logical evidence files and EWF version 2 are refused by name.
    let l01 = tmp.path().join("files.L01");
    fs::write(&l01, b"LVF\x09\x0d\x0a\xff\x00\x01\x01\x00\x00\x00").unwrap();
    let err = probe(&l01).unwrap_err().to_string();
    assert!(err.contains("logical evidence files"), "{err}");
    let ex01 = tmp.path().join("disk.Ex01");
    let mut h = b"EVF2\x0d\x0a\x81\x00\x02\x01\x01\x00\x01\x00\x00\x00".to_vec();
    h.resize(512, 0);
    fs::write(&ex01, &h).unwrap();
    let err = probe(&ex01).unwrap_err().to_string();
    assert!(err.contains("Ex01"), "{err}");

    // Read-only.
    let e = write_ewf(tmp.path(), "ro.E01", &raw, opts);
    let rw = OpenOptions {
        writable: true,
        ..Default::default()
    };
    let host = tmp.path().join("X.TXT");
    fs::write(&host, b"x").unwrap();
    let err = Volume::open(&e, &rw)
        .and_then(|v| v.inject(&host, "/", false))
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
}

/// Evidence files made by ewfacquire 20140814 (libewf, run as a black-box tool in CI) from a
/// 4 MiB MBR disk that s0 made, holding readme.txt and Folder/numbers.txt (`seq 1 20000`):
/// EnCase 1, EnCase 6, FTK Imager, linen 6 and SMART layouts, best compression.
#[test]
fn ewfacquire_samples() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ewf");
    let numbers: String = (1..=20000)
        .map(|i| {
            format!(
                "{i}
"
            )
        })
        .collect();
    for name in [
        "encase1.E01",
        "encase6.E01",
        "ftk.E01",
        "linen6.E01",
        "smart.s01",
    ] {
        let p = dir.join(name);
        let (info, _) = probe(&p).unwrap();
        assert_eq!(info.kind, ContainerKind::Ewf, "{name}");
        assert_eq!(info.virtual_size, 4 << 20, "{name}");
        let sums = checksum_virtual_disk(&p, None).unwrap();
        assert_eq!(
            sums.sha256, "6f810fa082eb671bf4ec55eee842d906fdfc590cd0d1f451d1cfb63ab0d204ed",
            "{name}"
        );
        // The MD5 ewfacquire stored in the hash section.
        assert_eq!(sums.md5, "61681abcb565904d9cec4860013d4c81", "{name}");
        let v = Volume::open(&p, &OpenOptions::default()).unwrap();
        let mut out = vec![];
        v.copy_out("/readme.txt", &mut out).unwrap();
        assert_eq!(
            out,
            b"SectorZero ewfacquire oracle
",
            "{name}"
        );
        out.clear();
        v.copy_out("/Folder/numbers.txt", &mut out).unwrap();
        assert_eq!(out, numbers.as_bytes(), "{name}");
    }
}
