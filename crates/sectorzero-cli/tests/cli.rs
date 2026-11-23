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
