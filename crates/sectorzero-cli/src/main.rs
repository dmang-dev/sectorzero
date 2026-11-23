//! `s0`: the SectorZero command line.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sectorzero_core::{
    Entry, FLOPPY_FORMATS, FloppyFormat, Layout, NewImage, OpenOptions, Stats, Volume, probe,
};

#[derive(Parser)]
#[command(
    name = "s0",
    version,
    about = "Create, inspect and edit disk images (SectorZero)"
)]
struct Cli {
    /// Print results as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct Img {
    /// Image file (.ima, .img, .vfd, .flp, .dsk).
    image: PathBuf,
    /// MBR partition number (1-4). Default: the first FAT partition.
    #[arg(short, long)]
    partition: Option<usize>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the built-in floppy formats.
    Formats,
    /// Create a new empty formatted image.
    New {
        image: PathBuf,
        /// Floppy format id (see `s0 formats`), e.g. 1440, 720, dmf2048.
        #[arg(short, long, conflicts_with = "size", required_unless_present = "size")]
        format: Option<String>,
        /// Size of a bare FAT volume instead of a floppy, e.g. 32M, 512M, 2G.
        #[arg(short, long)]
        size: Option<String>,
        #[arg(short, long)]
        label: Option<String>,
        /// Replace the file if it exists.
        #[arg(long)]
        force: bool,
    },
    /// Show the image layout and volume details.
    Info(Img),
    /// List files. Use -r for everything below the folder.
    Ls {
        #[command(flatten)]
        img: Img,
        /// Folder inside the image.
        #[arg(default_value = "/")]
        dir: String,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Copy files or folders out of the image (everything if no paths are given).
    Extract {
        #[command(flatten)]
        img: Img,
        /// Paths inside the image.
        paths: Vec<String>,
        /// Destination folder on this computer.
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
    },
    /// Copy files or folders from this computer into the image.
    Inject {
        #[command(flatten)]
        img: Img,
        /// Files or folders on this computer.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Destination folder inside the image (created if missing).
        #[arg(short, long, default_value = "/")]
        to: String,
        /// Overwrite existing files in the image.
        #[arg(long)]
        force: bool,
    },
    /// Create a folder (and its parents) inside the image.
    Mkdir {
        #[command(flatten)]
        img: Img,
        path: String,
    },
    /// Delete files or folders inside the image.
    Rm {
        #[command(flatten)]
        img: Img,
        #[arg(required = true)]
        paths: Vec<String>,
        /// Delete folders with everything in them.
        #[arg(short, long)]
        recursive: bool,
    },
    /// Rename or move a file or folder inside the image.
    Mv {
        #[command(flatten)]
        img: Img,
        from: String,
        to: String,
    },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("s0: {e:#}");
        std::process::exit(1);
    }
}

fn open(img: &Img, writable: bool) -> Result<Volume> {
    Volume::open(
        &img.image,
        OpenOptions {
            writable,
            partition: img.partition,
        },
    )
    .with_context(|| format!("cannot open {}", img.image.display()))
}

fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    match cli.cmd {
        Cmd::Formats => {
            if json {
                return print_json(&FLOPPY_FORMATS);
            }
            println!(
                "{:<8} {:>10}  {:>7}  {:>4}  description",
                "id", "bytes", "cluster", "root"
            );
            for f in FLOPPY_FORMATS {
                println!(
                    "{:<8} {:>10}  {:>7}  {:>4}  {}",
                    f.id,
                    f.size_bytes(),
                    f.bytes_per_cluster,
                    f.root_entries,
                    f.name
                );
            }
        }
        Cmd::New {
            image,
            format,
            size,
            label,
            force,
        } => {
            let spec = match (format, size) {
                (Some(f), _) => NewImage::Floppy(
                    FloppyFormat::find(&f)
                        .with_context(|| format!("unknown format {f:?}; see `s0 formats`"))?,
                ),
                (None, Some(s)) => NewImage::Volume {
                    size_bytes: parse_size(&s)?,
                },
                (None, None) => unreachable!("clap requires one of --format/--size"),
            };
            Volume::create(&image, spec, label.as_deref(), force)
                .with_context(|| format!("cannot create {}", image.display()))?;
            let vol = Volume::open(&image, OpenOptions::default())?;
            let info = vol.info()?;
            if json {
                return print_json(&info);
            }
            println!(
                "created {} ({}, {} bytes free)",
                image.display(),
                info.floppy_format.unwrap_or(info.fat_type),
                info.free_bytes
            );
        }
        Cmd::Info(img) => {
            let layout = probe(&img.image)?;
            let vol = open(&img, false)?;
            let i = vol.info()?;
            if json {
                return print_json(&i);
            }
            println!("image        {}", img.image.display());
            match &layout {
                Layout::BareVolume => println!("layout       bare volume (no partition table)"),
                Layout::Mbr(parts) => {
                    println!("layout       MBR");
                    for p in parts {
                        println!(
                            "  part {}     type 0x{:02X} {:<14} start {:>10}  {:>12} bytes{}",
                            p.index,
                            p.type_id,
                            p.type_name,
                            p.start_lba,
                            p.size_bytes(),
                            if p.bootable { "  boot" } else { "" }
                        );
                    }
                }
                other => println!("layout       {other:?}"),
            }
            if let Some(p) = &i.partition {
                println!("opened       partition {}", p.index);
            }
            println!("file system  {}", i.fat_type);
            if let Some(f) = i.floppy_format {
                println!("format       {f}");
            }
            println!(
                "label        {}",
                if i.label.is_empty() {
                    "(none)"
                } else {
                    &i.label
                }
            );
            println!("serial       {}", i.volume_id);
            println!("oem          {}", i.oem_name);
            println!("sector size  {}", i.bytes_per_sector);
            println!("cluster size {}", i.cluster_size);
            println!(
                "capacity     {} bytes ({} clusters)",
                i.total_bytes, i.total_clusters
            );
            println!(
                "free         {} bytes ({} clusters)",
                i.free_bytes, i.free_clusters
            );
        }
        Cmd::Ls {
            img,
            dir,
            recursive,
        } => {
            let vol = open(&img, false)?;
            let entries = if recursive {
                vol.walk(&dir)?
            } else {
                vol.list(&dir)?
            };
            if json {
                return print_json(&entries);
            }
            print_listing(&entries, recursive);
        }
        Cmd::Extract {
            img,
            paths,
            out,
            force,
        } => {
            let vol = open(&img, false)?;
            let mut total = Stats::default();
            let paths = if paths.is_empty() {
                vec![String::new()]
            } else {
                paths
            };
            for p in &paths {
                add(
                    &mut total,
                    vol.extract(p, &out, force)
                        .with_context(|| format!("extracting {p:?}"))?,
                );
            }
            report(json, "extracted", &total, Some(&out))?;
        }
        Cmd::Inject {
            img,
            files,
            to,
            force,
        } => {
            let vol = open(&img, true)?;
            let mut total = Stats::default();
            for f in &files {
                add(
                    &mut total,
                    vol.inject(f, &to, force)
                        .with_context(|| format!("injecting {}", f.display()))?,
                );
            }
            let free = vol.info()?.free_bytes;
            vol.close()?;
            report(json, "injected", &total, None)?;
            if !json {
                println!("{free} bytes free");
            }
        }
        Cmd::Mkdir { img, path } => {
            let vol = open(&img, true)?;
            vol.mkdir(&path)?;
            vol.close()?;
        }
        Cmd::Rm {
            img,
            paths,
            recursive,
        } => {
            let vol = open(&img, true)?;
            let mut total = Stats::default();
            for p in &paths {
                add(
                    &mut total,
                    vol.remove(p, recursive)
                        .with_context(|| format!("removing {p:?}"))?,
                );
            }
            vol.close()?;
            report(json, "removed", &total, None)?;
        }
        Cmd::Mv { img, from, to } => {
            let vol = open(&img, true)?;
            vol.rename(&from, &to)?;
            vol.close()?;
        }
    }
    Ok(())
}

fn print_listing(entries: &[Entry], recursive: bool) {
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    for e in entries {
        let size = if e.is_dir {
            "<DIR>".to_string()
        } else {
            e.size.to_string()
        };
        let name = if recursive {
            format!("/{}", e.path)
        } else {
            e.name.clone()
        };
        let short = if e.short_name != e.name && !e.short_name.is_empty() {
            format!("  [{}]", e.short_name)
        } else {
            String::new()
        };
        println!(
            "{}  {}  {:>10}  {}{}",
            e.modified,
            e.attributes.flags(),
            size,
            name,
            short
        );
        if e.is_dir {
            dirs += 1;
        } else {
            files += 1;
            bytes += e.size;
        }
    }
    println!("{files} file(s), {bytes} bytes; {dirs} folder(s)");
}

fn add(total: &mut Stats, s: Stats) {
    total.files += s.files;
    total.dirs += s.dirs;
    total.bytes += s.bytes;
}

fn report(json: bool, verb: &str, s: &Stats, dest: Option<&Path>) -> Result<()> {
    if json {
        return print_json(s);
    }
    let to = dest
        .map(|d| format!(" to {}", d.display()))
        .unwrap_or_default();
    println!(
        "{verb} {} file(s), {} folder(s), {} bytes{to}",
        s.files, s.dirs, s.bytes
    );
    Ok(())
}

fn print_json<T: serde::Serialize + ?Sized>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// `512M`, `2G`, `1440K`, or plain bytes.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim().to_ascii_uppercase();
    let s = s.trim_end_matches('B');
    let (num, mult) = match s.chars().last() {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("bad size {s:?}"))?;
    if n <= 0.0 {
        bail!("size must be positive");
    }
    Ok((n * mult as f64) as u64)
}
