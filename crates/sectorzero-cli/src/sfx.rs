//! Self-extracting images: a copy of this `s0` executable with a disk image appended
//! (WinImage's "Create self-extracting file"). When `s0` starts it checks its own tail for a
//! payload and, if there is one, runs as the extractor instead of the normal command line.
//!
//! Layout: `[executable][manifest JSON][image bytes][footer]`, footer = 32 bytes:
//! magic (8), manifest length (u64 LE), image length (u64 LE), reserved (8, zero).

use std::fs::File;
use std::io::{self, BufRead, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sectorzero_core::{Image, OpenOptions, Volume};
use sectorzero_i18n::t;
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"S0SFX\x01\r\n";
const FOOTER: u64 = 32;

#[derive(Serialize, Deserialize)]
struct Manifest {
    /// File name of the packaged image (e.g. `boot.ima`).
    name: String,
    #[serde(default)]
    comment: String,
    /// Version of `s0` that built it.
    #[serde(default)]
    version: String,
}

pub struct Payload {
    exe: PathBuf,
    manifest: Manifest,
    image_offset: u64,
    image_len: u64,
}

/// `(manifest length, image length)` when `f` ends with an SFX footer.
fn footer(f: &mut File) -> io::Result<Option<(u64, u64)>> {
    let len = f.metadata()?.len();
    if len < FOOTER {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(len - FOOTER))?;
    let mut b = [0u8; FOOTER as usize];
    f.read_exact(&mut b)?;
    if &b[..8] != MAGIC {
        return Ok(None);
    }
    let m = u64::from_le_bytes(b[8..16].try_into().unwrap());
    let i = u64::from_le_bytes(b[16..24].try_into().unwrap());
    if m.checked_add(i)
        .and_then(|x| x.checked_add(FOOTER))
        .is_none_or(|t| t > len)
    {
        return Ok(None);
    }
    Ok(Some((m, i)))
}

/// The payload of the running executable, if it is a self-extracting image.
pub fn detect() -> Option<Payload> {
    let exe = std::env::current_exe().ok()?;
    let mut f = File::open(&exe).ok()?;
    let (m_len, i_len) = footer(&mut f).ok()??;
    let len = f.metadata().ok()?.len();
    let m_start = len - FOOTER - i_len - m_len;
    f.seek(SeekFrom::Start(m_start)).ok()?;
    let mut m = vec![0u8; m_len as usize];
    f.read_exact(&mut m).ok()?;
    let manifest: Manifest = serde_json::from_slice(&m).ok()?;
    Some(Payload {
        exe,
        manifest,
        image_offset: m_start + m_len,
        image_len: i_len,
    })
}

/// Bytes of `f` that are the executable itself (without any payload already attached).
fn stub_len(f: &mut File) -> io::Result<u64> {
    let len = f.metadata()?.len();
    Ok(match footer(f)? {
        Some((m, i)) => len - FOOTER - m - i,
        None => len,
    })
}

/// Build `out` from this executable and `image`. Returns the size of `out`.
pub fn create(image: &Path, out: &Path, comment: Option<String>, force: bool) -> Result<u64> {
    Image::open(image, &OpenOptions::default())
        .with_context(|| t!("cannot open {}", image.display()))?;
    if !force && out.exists() {
        bail!(t!("{} exists (use --force to replace it)", out.display()));
    }
    let exe = std::env::current_exe().context(t!("cannot find this executable"))?;
    if let (Ok(a), Ok(b)) = (exe.canonicalize(), out.canonicalize())
        && a == b
    {
        bail!(t!("the output must not be the running executable"));
    }
    let manifest = serde_json::to_vec(&Manifest {
        name: image
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "disk.img".into()),
        comment: comment.unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION").into(),
    })?;
    let mut src = File::open(&exe)?;
    let stub = stub_len(&mut src)?;
    src.seek(SeekFrom::Start(0))?;
    let mut dst = File::create(out).with_context(|| t!("cannot write {}", out.display()))?;
    io::copy(&mut (&mut src).take(stub), &mut dst)?;
    dst.write_all(&manifest)?;
    let image_len = io::copy(&mut File::open(image)?, &mut dst)?;
    let mut foot = [0u8; FOOTER as usize];
    foot[..8].copy_from_slice(MAGIC);
    foot[8..16].copy_from_slice(&(manifest.len() as u64).to_le_bytes());
    foot[16..24].copy_from_slice(&image_len.to_le_bytes());
    dst.write_all(&foot)?;
    dst.flush()?;
    let size = dst.metadata()?.len();
    drop(dst);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(size)
}

impl Payload {
    fn write_image(&self, dest: &Path, force: bool) -> Result<()> {
        if !force && dest.exists() {
            bail!(t!("{} exists (use --force to replace it)", dest.display()));
        }
        let mut f = File::open(&self.exe)?;
        f.seek(SeekFrom::Start(self.image_offset))?;
        let mut out = File::create(dest).with_context(|| t!("cannot write {}", dest.display()))?;
        let n = io::copy(&mut f.take(self.image_len), &mut out)?;
        if n != self.image_len {
            bail!(t!("the packaged image is cut short"));
        }
        Ok(())
    }

    /// The image as a temporary file (kept until the returned guard drops).
    fn temp_image(&self) -> Result<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::Builder::new().prefix("s0-sfx-").tempdir()?;
        let path = dir.path().join(&self.manifest.name);
        self.write_image(&path, true)?;
        Ok((dir, path))
    }

    fn extract(&self, dest: &Path, force: bool) -> Result<()> {
        let (_guard, path) = self.temp_image()?;
        let vol = Volume::open(&path, &OpenOptions::default())?;
        let s = vol.extract("", dest, force)?;
        println!(
            "{}",
            t!(
                "extracted {} file(s), {} folder(s), {} bytes to {}",
                s.files,
                s.dirs,
                s.bytes,
                dest.display()
            )
        );
        Ok(())
    }

    fn list(&self) -> Result<()> {
        let (_guard, path) = self.temp_image()?;
        let vol = Volume::open(&path, &OpenOptions::default())?;
        for e in vol.walk("")? {
            if e.is_dir {
                println!("{:>12}  {}/", "<DIR>", e.path);
            } else {
                println!("{:>12}  {}", e.size, e.path);
            }
        }
        Ok(())
    }

    fn usage(&self) {
        let me = self
            .exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "this program".into());
        println!(
            "{}",
            t!(
                "Self-extracting disk image: {} ({} bytes)",
                self.manifest.name,
                self.image_len
            )
        );
        if !self.manifest.comment.is_empty() {
            println!("{}", self.manifest.comment);
        }
        println!(
            "{}",
            t!(
                "\nUsage:\n  {} --list                 list the files in the image\n  \
             {} --extract <folder>     copy every file out (also /X <folder>)\n  \
             {} --write-image <file>   save the disk image itself (also /W:<file>)\n  \
             add --force to overwrite existing files\n\nMade with SectorZero {}.",
                me,
                me,
                me,
                self.manifest.version
            )
        );
    }

    fn interactive(&self) -> Result<i32> {
        self.usage();
        let stem = Path::new(&self.manifest.name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "disk".into());
        let base = self.exe.parent().map(Path::to_path_buf).unwrap_or_default();
        println!(
            "{}",
            t!(
                "\n  1) extract the files to {}\n  2) save the image as {}\n  q) quit",
                base.join(&stem).display(),
                base.join(&self.manifest.name).display()
            )
        );
        print!("> ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        match line.trim() {
            "1" => self.extract(&base.join(&stem), false)?,
            "2" => {
                let dest = base.join(&self.manifest.name);
                self.write_image(&dest, false)?;
                println!("{}", t!("saved {}", dest.display()));
            }
            _ => return Ok(0),
        }
        Ok(0)
    }
}

/// Run as the extractor. Returns the process exit code.
pub fn run(p: Payload) -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let force = args
        .iter()
        .any(|a| a == "--force" || a.eq_ignore_ascii_case("/Y"));
    let value_after = |i: usize| args.get(i + 1).map(PathBuf::from);
    let mut action: Option<Result<()>> = None;
    for (i, a) in args.iter().enumerate() {
        let lower = a.to_ascii_lowercase();
        action = match lower.as_str() {
            "--help" | "-h" | "/?" => {
                p.usage();
                return 0;
            }
            "--list" | "-l" | "/l" => Some(p.list()),
            "--extract" | "/x" => Some(match value_after(i) {
                Some(d) => p.extract(&d, force),
                None => Err(anyhow::anyhow!("--extract needs a folder")),
            }),
            "--write-image" => Some(match value_after(i) {
                Some(f) => p
                    .write_image(&f, force)
                    .map(|_| println!("{}", t!("saved {}", f.display()))),
                None => Err(anyhow::anyhow!("--write-image needs a file name")),
            }),
            _ if lower.starts_with("/w:") => {
                let f = PathBuf::from(&a[3..]);
                Some(
                    p.write_image(&f, force)
                        .map(|_| println!("{}", t!("saved {}", f.display()))),
                )
            }
            _ => continue,
        };
        break;
    }
    let result = match action {
        Some(r) => r.map(|_| 0),
        None if io::stdin().is_terminal() => p.interactive(),
        None => {
            p.usage();
            Ok(2)
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{}", t!("error: {}", crate::error_text(&e)));
            1
        }
    }
}
