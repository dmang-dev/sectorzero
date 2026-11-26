//! APFS volumes made by macOS: the samples from the macOS cross-check in CI
//! (.github/oracle/apfs.sh), kept in tests/data/apfs with a manifest of what macOS showed when
//! the image was mounted (sizes, SHA-256, link targets, flags, Finder info, resource forks).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use sectorzero_core::{
    ContainerKind, CreateOptions, Error, ForkMode, FsKind, NewImage, OpenOptions, PartitionScheme,
    Volume, convert,
};

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/apfs")
        .join(name)
}

fn open(path: &Path, volume: Option<&str>) -> sectorzero_core::Result<Volume> {
    Volume::open(
        path,
        &OpenOptions {
            volume: volume.map(Into::into),
            ..Default::default()
        },
    )
}

fn bytes(v: &Volume, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    v.copy_out(path, &mut out).unwrap();
    out
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

#[test]
fn sample_volume_matches_macos() {
    let v = open(&data("sample.dmg"), None).unwrap();
    let info = v.info().unwrap();
    assert_eq!(info.fs_type, "APFS");
    assert_eq!(info.label, "SZ Sample");
    assert!(!info.fs_writable);
    assert_eq!(info.cluster_size, 4096);
    let apfs = info.apfs.as_ref().unwrap();
    assert_eq!(apfs.volumes.len(), 1);
    assert!(apfs.volumes[0].open && !apfs.volumes[0].encrypted);
    assert!(apfs.free_blocks.is_some_and(|f| f > 0 && f < apfs.blocks));

    let manifest: serde_json::Map<String, Value> =
        serde_json::from_str(&fs::read_to_string(data("sample.json")).unwrap()).unwrap();
    let entries: std::collections::HashMap<String, _> = v
        .walk("")
        .unwrap()
        .into_iter()
        .map(|e| (e.path.nfc().collect::<String>(), e))
        .collect();
    let mut files = 0;
    for (path, want) in &manifest {
        let e = entries
            .get(path)
            .unwrap_or_else(|| panic!("{path} missing"));
        match want["type"].as_str().unwrap() {
            "dir" => assert!(e.is_dir, "{path}"),
            "link" => {
                assert!(!e.is_dir, "{path}");
                let target = want["target"].as_str().unwrap();
                assert_eq!(bytes(&v, &e.path), target.as_bytes(), "{path}");
                assert_eq!(e.size, target.len() as u64, "{path}");
            }
            _ => {
                files += 1;
                assert_eq!(e.size, want["size"].as_u64().unwrap(), "{path} size");
                let sum = v.checksum(&e.path).unwrap();
                assert_eq!(sum.sha256, want["sha256"].as_str().unwrap(), "{path}");
                assert_eq!(
                    e.attributes.hidden,
                    want["hidden"].as_bool().unwrap(),
                    "{path} hidden"
                );
                if let Some(f) = want.get("finder") {
                    let mac = e.mac.as_ref().unwrap();
                    assert_eq!(f.as_str().unwrap()[..16], *"5445585474747874", "{path}");
                    assert_eq!(
                        (mac.file_type.as_str(), mac.creator.as_str()),
                        ("TEXT", "ttxt")
                    );
                }
                if want.get("rsrc_sha256").is_some() {
                    assert!(e.mac.as_ref().unwrap().resource_size > 0, "{path}");
                }
            }
        }
    }
    // Everything s0 lists is in the manifest, apart from what macOS keeps for itself.
    for path in entries.keys() {
        assert!(
            manifest.contains_key(path) || path.starts_with(".fseventsd"),
            "{path} is not in the manifest"
        );
    }
    assert!(files > 300, "{files} files");
    // The compressed files covered every method the sample has.
    for dir in ["comp-zlib", "comp-lzvn", "comp-lzfse", "comp-ditto"] {
        assert!(manifest.contains_key(&format!("{dir}/text.txt")), "{dir}");
    }
    let old = &entries["old.txt"];
    assert!(old.modified.starts_with("1999-12-31") || old.modified.starts_with("2000-01-01"));
}

#[test]
fn resource_forks_and_finder_info_extract_as_appledouble() {
    let v = open(&data("sample.dmg"), None).unwrap();
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data("sample.json")).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for name in ["forked.txt", "big-fork.txt", "typed.txt", "readme.txt"] {
        v.extract_with(name, dir.path(), false, ForkMode::AppleDouble)
            .unwrap();
    }
    let double = |name: &str| fs::read(dir.path().join(format!("._{name}"))).ok();
    assert!(
        double("readme.txt").is_none(),
        "no forks, no AppleDouble file"
    );
    for name in ["forked.txt", "big-fork.txt"] {
        let d = double(name).unwrap();
        assert_eq!(&d[..8], &[0, 5, 0x16, 7, 0, 2, 0, 0]);
        // Entry 2 (the resource fork) is the second one.
        let off = u32::from_be_bytes(d[42..46].try_into().unwrap()) as usize;
        let len = u32::from_be_bytes(d[46..50].try_into().unwrap()) as usize;
        assert_eq!(
            sha(&d[off..off + len]),
            manifest[name]["rsrc_sha256"].as_str().unwrap(),
            "{name}"
        );
    }
    let d = double("typed.txt").unwrap();
    assert_eq!(&d[50..58], b"TEXTttxt");
    assert_eq!(
        fs::read(dir.path().join("typed.txt")).unwrap(),
        b"TEXT ttxt\n"
    );
}

#[test]
fn case_sensitive_volume_keeps_names_apart() {
    let v = open(&data("case.dmg"), None).unwrap();
    assert_eq!(v.info().unwrap().fs_type, "APFS (case-sensitive)");
    assert_eq!(bytes(&v, "Readme.txt"), b"upper\n");
    assert_eq!(bytes(&v, "readme.txt"), b"SectorZero APFS oracle\n");
    // Names match by Unicode normalization too: café.txt is stored decomposed.
    assert_eq!(bytes(&v, "caf\u{e9}.txt"), "cafe\u{301}.txt\n".as_bytes());
    assert_eq!(bytes(&v, "Folder/Sub/deep.txt"), b"deep\n");
}

/// A copy of the sample as a raw disk, for damaging.
fn raw_sample(dir: &Path) -> PathBuf {
    let raw = dir.join("sample.img");
    convert(
        &data("sample.dmg"),
        None,
        &raw,
        Some(ContainerKind::Raw),
        false,
        false,
    )
    .unwrap();
    raw
}

#[test]
fn damaged_metadata_is_an_error_not_wrong_data() {
    let dir = tempfile::tempdir().unwrap();
    let raw = raw_sample(dir.path());
    let mut img = fs::read(&raw).unwrap();
    // Flip one byte in every leaf node of the file-system tree (B-tree nodes, subtype 0x0E).
    let part = 40 * 512;
    let mut hit = 0;
    for b in (part..img.len()).step_by(4096) {
        let blk = &img[b..b + 4096];
        let t = u32::from_le_bytes(blk[24..28].try_into().unwrap()) & 0xFFFF;
        let sub = u32::from_le_bytes(blk[28..32].try_into().unwrap());
        let flags = u16::from_le_bytes(blk[32..34].try_into().unwrap());
        if t == 3 && sub == 0x0E && flags & 2 != 0 {
            img[b + 2000] ^= 0x40;
            hit += 1;
        }
    }
    assert!(hit > 1, "{hit} leaf nodes");
    fs::write(&raw, &img).unwrap();
    let err = match open(&raw, None) {
        Err(e) => e,
        Ok(v) => match v.walk("") {
            Err(e) => e,
            Ok(entries) => entries
                .iter()
                .filter(|e| !e.is_dir)
                .find_map(|e| v.checksum(&e.path).err())
                .expect("a damaged node must give an error"),
        },
    };
    assert!(err.to_string().contains("damaged APFS volume"), "{err}");
}

#[test]
fn volume_option_needs_an_apfs_container() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("fat.img");
    Volume::create(
        &img,
        NewImage::Volume {
            size_bytes: 2 << 20,
        },
        &CreateOptions {
            fs: FsKind::Fat,
            scheme: PartitionScheme::Mbr,
            ..Default::default()
        },
    )
    .unwrap();
    let err = open(&img, Some("1")).err().unwrap();
    assert!(err.to_string().contains("--volume only applies"), "{err}");
    let err = open(&data("sample.dmg"), Some("Nowhere")).err().unwrap();
    assert!(matches!(err, Error::NotFound(_)), "{err}");
    assert!(err.to_string().contains("1 SZ Sample"), "{err}");
    // Writing is refused before anything changes.
    let v = open(&data("sample.dmg"), Some("1")).unwrap();
    assert!(v.mkdir("New").is_err());
}
