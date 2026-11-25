//! Windows Backup system images (WindowsImageBackup folders).

use std::fs;
use std::path::Path;

use sectorzero_core::{
    ContainerKind, CreateOptions, NewImage, OpenOptions, PartitionScheme, Volume, probe,
};

fn disk(path: &Path, size: u64, label: &str) {
    Volume::create(
        path,
        NewImage::Volume { size_bytes: size },
        &CreateOptions {
            label: Some(label.into()),
            dynamic: true,
            scheme: PartitionScheme::Mbr,
            ..Default::default()
        },
    )
    .unwrap();
}

fn label(path: &Path, member: Option<&str>) -> String {
    Volume::open(
        path,
        &OpenOptions {
            member: member.map(str::to_string),
            ..Default::default()
        },
    )
    .unwrap()
    .info()
    .unwrap()
    .label
}

#[test]
fn a_windows_backup_folder_opens_as_its_disks() {
    let tmp = tempfile::tempdir().unwrap();
    let pc = tmp
        .path()
        .join("E")
        .join("WindowsImageBackup")
        .join("DESKTOP-1");
    let old = pc.join("Backup 2026-01-02 030405");
    let new = pc.join("Backup 2026-11-24 101500");
    for b in [&old, &new] {
        fs::create_dir_all(b).unwrap();
        fs::write(b.join("BackupSpecs.xml"), "<BackupSpecs/>").unwrap();
    }
    fs::create_dir_all(pc.join("Catalog")).unwrap();
    fs::write(pc.join("MediaId"), [0u8; 16]).unwrap();
    disk(
        &old.join("{11111111-0000-0000-0000-000000000000}.vhdx"),
        48 << 20,
        "OLDSYS",
    );
    disk(
        &new.join("{22222222-0000-0000-0000-000000000000}.vhdx"),
        48 << 20,
        "NEWSYS",
    );
    disk(
        &new.join("{33333333-0000-0000-0000-000000000000}.vhdx"),
        24 << 20,
        "DATA",
    );

    // The drive, the WindowsImageBackup folder, the computer's folder and a backup folder
    // all open; by default the largest disk of the newest backup.
    for dir in [
        tmp.path().join("E"),
        tmp.path().join("E").join("WindowsImageBackup"),
        pc.clone(),
        new.clone(),
    ] {
        let (info, _) = probe(&dir).unwrap();
        assert_eq!(info.kind, ContainerKind::Vhdx, "{}", dir.display());
        assert!(
            info.member.as_deref().unwrap().contains("{22222222"),
            "{info:?}"
        );
        assert_eq!(label(&dir, None), "NEWSYS");
    }
    let (info, _) = probe(&pc).unwrap();
    assert_eq!(info.members.len(), 3);
    assert_eq!(
        info.members[0],
        "Backup 2026-01-02 030405/{11111111-0000-0000-0000-000000000000}.vhdx"
    );
    assert_eq!(label(&pc, Some("1")), "OLDSYS");
    assert_eq!(
        label(&pc, Some("{33333333-0000-0000-0000-000000000000}.vhdx")),
        "DATA"
    );

    // Read-only.
    let host = tmp.path().join("X.TXT");
    fs::write(&host, b"x").unwrap();
    let rw = OpenOptions {
        writable: true,
        ..Default::default()
    };
    let err = Volume::open(&pc, &rw)
        .and_then(|v| v.inject(&host, "/", false))
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");

    // Any other folder is refused.
    assert!(
        probe(
            &tmp.path()
                .join("E")
                .join("WindowsImageBackup")
                .join("DESKTOP-1")
                .join("Catalog")
        )
        .is_err()
    );
}
