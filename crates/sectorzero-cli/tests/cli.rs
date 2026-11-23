use std::fs;
use std::process::Command;

fn s0(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_s0"))
        .args(args)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn new_inject_ls_extract() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("disk.ima");
    let img = img.to_str().unwrap();
    let file = tmp.path().join("readme.txt");
    fs::write(&file, b"SectorZero").unwrap();

    let (ok, out) = s0(&["new", img, "-f", "1.44", "-l", "SMOKE"]);
    assert!(ok, "{out}");
    assert!(out.contains("1.44 MB"), "{out}");

    let (ok, out) = s0(&["inject", img, file.to_str().unwrap(), "--to", "/docs"]);
    assert!(ok, "{out}");

    let (ok, out) = s0(&["ls", img, "-r"]);
    assert!(ok && out.contains("/docs/readme.txt"), "{out}");

    let (ok, out) = s0(&["info", img]);
    assert!(
        ok && out.contains("SMOKE") && out.contains("FAT12"),
        "{out}"
    );

    let dest = tmp.path().join("out");
    let (ok, out) = s0(&["extract", img, "/docs", "-o", dest.to_str().unwrap()]);
    assert!(ok, "{out}");
    assert_eq!(
        fs::read(dest.join("docs/readme.txt")).unwrap(),
        b"SectorZero"
    );

    // Errors go to stderr with a non-zero exit.
    let (ok, out) = s0(&["ls", img, "/nope"]);
    assert!(!ok && out.contains("not found"), "{out}");
}

#[test]
fn self_extracting_image_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("boot.ima");
    let img = img.to_str().unwrap();
    let file = tmp.path().join("readme.txt");
    fs::write(&file, b"packed").unwrap();
    assert!(s0(&["new", img, "-f", "720"]).0);
    assert!(s0(&["inject", img, file.to_str().unwrap()]).0);

    let exe = tmp.path().join(if cfg!(windows) {
        "boot.exe"
    } else {
        "boot.run"
    });
    let (ok, out) = s0(&["sfx", img, "-o", exe.to_str().unwrap(), "--comment", "Hi"]);
    assert!(ok, "{out}");

    let run = |args: &[&str]| {
        let o = Command::new(&exe).args(args).output().unwrap();
        (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        )
    };
    let (ok, out) = run(&["--list"]);
    assert!(ok && out.contains("readme.txt"), "{out}");
    let dest = tmp.path().join("x");
    let (ok, out) = run(&["--extract", dest.to_str().unwrap()]);
    assert!(ok, "{out}");
    assert_eq!(fs::read(dest.join("readme.txt")).unwrap(), b"packed");
    let back = tmp.path().join("back.ima");
    let (ok, out) = run(&["--write-image", back.to_str().unwrap()]);
    assert!(ok, "{out}");
    assert_eq!(fs::read(&back).unwrap(), fs::read(img).unwrap());
    // Without arguments and without a terminal it explains itself and exits with 2.
    let o = Command::new(&exe)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stdout).contains("Hi"));
    // An SFX built from an SFX does not stack payloads.
    let exe2 = tmp.path().join(if cfg!(windows) {
        "again.exe"
    } else {
        "again.run"
    });
    let (ok, _) = s0(&["sfx", back.to_str().unwrap(), "-o", exe2.to_str().unwrap()]);
    assert!(ok);
    assert_eq!(
        fs::metadata(&exe2).unwrap().len(),
        fs::metadata(&exe).unwrap().len() - 2, /* "Hi" */
    );
}

#[test]
fn custom_geometry_and_attrib_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("c.ima");
    let img = img.to_str().unwrap();
    let (ok, out) = s0(&[
        "new",
        img,
        "--sectors",
        "2000",
        "--sectors-per-track",
        "10",
        "--heads",
        "2",
        "--cluster",
        "1K",
        "--fat",
        "12",
        "--media",
        "0xF9",
        "--drive",
        "0x00",
    ]);
    assert!(ok, "{out}");
    let (ok, out) = s0(&["boot", img]);
    assert!(ok && out.contains("2000") && out.contains("0xF9"), "{out}");
    let file = tmp.path().join("f.txt");
    fs::write(&file, b"x").unwrap();
    assert!(s0(&["inject", img, file.to_str().unwrap()]).0);
    let (ok, out) = s0(&[
        "attrib",
        img,
        "f.txt",
        "--set",
        "RH",
        "--modified",
        "2001-09-09 01:46:40",
    ]);
    assert!(ok, "{out}");
    let (ok, out) = s0(&["attrib", img, "f.txt"]);
    assert!(
        ok && out.contains("RH--") && out.contains("2001-09-09 01:46:40"),
        "{out}"
    );
    let (ok, out) = s0(&["attrib", img, "f.txt", "--set", "R", "--clear", "R"]);
    assert!(!ok && out.contains("both"), "{out}");
    let (ok, out) = s0(&["checksum", img, "f.txt"]);
    assert!(
        ok && out.contains("2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"),
        "{out}"
    );
}
