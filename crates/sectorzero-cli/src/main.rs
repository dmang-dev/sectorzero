//! `s0`: the SectorZero command line.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use sectorzero_core::{
    BootPlatform, BootSectorEdit, ContainerKind, CreateOptions, Entry, FLOPPY_FORMATS,
    FloppyFormat, Image, IsoBuildOptions, Layout, NewImage, OpenOptions, PartitionScheme, Stats,
    Volume, VolumeInfo, build_iso, convert, probe,
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
    /// Image file: .ima .img .vfd .imz .wlz .vhd .vhdx .vmdk .iso …
    image: PathBuf,
    /// Partition number (MBR 1-4 primary, 5+ logical; GPT entry). Default: the first FAT partition.
    #[arg(short, long)]
    partition: Option<usize>,
    /// Image inside a WLZ list (number or member name). Default: the first.
    #[arg(long)]
    member: Option<String>,
}

impl Img {
    fn opts(&self, writable: bool) -> OpenOptions {
        OpenOptions {
            writable,
            partition: self.partition,
            member: self.member.clone(),
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Table {
    None,
    Mbr,
    Gpt,
}

#[derive(Clone, Copy, ValueEnum)]
enum Container {
    Raw,
    Imz,
    Vhd,
    Vhdx,
    Vmdk,
}

impl From<Container> for ContainerKind {
    fn from(c: Container) -> Self {
        match c {
            Container::Raw => ContainerKind::Raw,
            Container::Imz => ContainerKind::Imz,
            Container::Vhd => ContainerKind::Vhd,
            Container::Vhdx => ContainerKind::Vhdx,
            Container::Vmdk => ContainerKind::Vmdk,
        }
    }
}

#[derive(clap::Args)]
struct FormatSpec {
    /// Floppy format id (see `s0 formats`), e.g. 1440, 720, dmf2048.
    #[arg(short, long, conflicts_with = "size", required_unless_present = "size")]
    format: Option<String>,
    /// Size of a hard disk / volume image instead of a floppy, e.g. 32M, 512M, 2G.
    #[arg(short, long)]
    size: Option<String>,
}

impl FormatSpec {
    fn spec(&self) -> Result<NewImage> {
        Ok(match (&self.format, &self.size) {
            (Some(f), _) => NewImage::Floppy(
                FloppyFormat::find(f)
                    .with_context(|| format!("unknown format {f:?}; see `s0 formats`"))?,
            ),
            (None, Some(s)) => NewImage::Volume {
                size_bytes: parse_size(s)?,
            },
            (None, None) => unreachable!("clap requires one of --format/--size"),
        })
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// List the built-in floppy formats.
    Formats,
    /// Create a new empty formatted image. The container follows the extension
    /// (.imz .vhd .vhdx .vmdk; raw otherwise) unless --container is given.
    New {
        image: PathBuf,
        #[command(flatten)]
        spec: FormatSpec,
        #[arg(short, long)]
        label: Option<String>,
        /// Partition table for a hard disk image (one FAT partition at 1 MiB).
        #[arg(short, long, value_enum, default_value = "none")]
        table: Table,
        #[arg(short, long, value_enum)]
        container: Option<Container>,
        /// Fixed-size VHD/VHDX/VMDK (default: dynamic, growing as data is written).
        #[arg(long)]
        fixed: bool,
        /// Replace the file if it exists.
        #[arg(long)]
        force: bool,
    },
    /// Show the container, layout and volume details.
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
    /// Show, set or remove the volume label.
    Label {
        #[command(flatten)]
        img: Img,
        /// New label (up to 11 characters). Omit to show the current one.
        label: Option<String>,
        /// Remove the label.
        #[arg(long, conflicts_with = "label")]
        clear: bool,
    },
    /// Show or edit the FAT boot sector (or, with --mbr, the disk's master boot record).
    Boot {
        #[command(flatten)]
        img: Img,
        /// Save the 512-byte sector to a file.
        #[arg(long)]
        export: Option<PathBuf>,
        /// Load boot code from a file, keeping this volume's BPB (or partition table with --mbr).
        #[arg(long)]
        import: Option<PathBuf>,
        /// Work on the master boot record instead of the volume boot sector.
        #[arg(long)]
        mbr: bool,
        #[arg(long)]
        oem: Option<String>,
        /// Volume serial number, e.g. 1A2B-3C4D.
        #[arg(long)]
        serial: Option<String>,
        #[arg(long)]
        heads: Option<u16>,
        #[arg(long)]
        sectors_per_track: Option<u16>,
        #[arg(long)]
        hidden_sectors: Option<u32>,
        /// BIOS drive number, e.g. 0x00 (floppy) or 0x80 (hard disk).
        #[arg(long, value_parser = parse_byte)]
        drive: Option<u8>,
        /// Overwrite the --export file.
        #[arg(long)]
        force: bool,
    },
    /// Defragment a FAT volume (files become contiguous, free space is consolidated).
    Defrag {
        #[command(flatten)]
        img: Img,
        /// Only report fragmentation.
        #[arg(long)]
        analyze: bool,
        /// Also zero the free space (IMZ images compress better).
        #[arg(long)]
        zero_free: bool,
    },
    /// Change the format/size of a floppy or volume image, keeping its files ("Change format").
    Resize {
        #[command(flatten)]
        img: Img,
        #[command(flatten)]
        spec: FormatSpec,
    },
    /// Copy an image into another container (e.g. IMA → VHDX, VMDK → IMZ).
    Convert {
        source: PathBuf,
        dest: PathBuf,
        /// Container of the destination (default: from its extension).
        #[arg(short, long, value_enum)]
        container: Option<Container>,
        /// Fixed-size VHD/VHDX/VMDK (default: dynamic).
        #[arg(long)]
        fixed: bool,
        /// Image inside a WLZ list.
        #[arg(long)]
        member: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Build an ISO 9660 + Joliet image from a folder, optionally bootable (El Torito).
    Iso {
        folder: PathBuf,
        output: PathBuf,
        #[arg(short, long, default_value = "")]
        label: String,
        /// Boot image: a 1.2/1.44/2.88 MB floppy image (emulated) or a no-emulation loader.
        #[arg(short, long)]
        boot: Option<PathBuf>,
        /// The boot image is for UEFI (an EFI system partition image).
        #[arg(long, requires = "boot")]
        efi: bool,
        /// No emulation even for a floppy-sized boot image.
        #[arg(long, requires = "boot")]
        no_emulation: bool,
        /// 512-byte sectors the BIOS loads for a no-emulation image (default 4).
        #[arg(long, requires = "boot")]
        load_sectors: Option<u16>,
        #[arg(long)]
        force: bool,
    },
    /// Save the El Torito boot image of an ISO to a file.
    BootImage {
        #[command(flatten)]
        img: Img,
        /// Boot catalog entry (see `s0 info`).
        #[arg(short = 'n', long, default_value_t = 1)]
        entry: usize,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long)]
        force: bool,
    },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("s0: {e:#}");
        std::process::exit(1);
    }
}

fn open(img: &Img, writable: bool) -> Result<Volume> {
    Volume::open(&img.image, &img.opts(writable))
        .with_context(|| format!("cannot open {}", img.image.display()))
}

fn open_image(img: &Img, writable: bool) -> Result<Image> {
    Image::open(&img.image, &img.opts(writable))
        .with_context(|| format!("cannot open {}", img.image.display()))
}

fn write_new(path: &Path, data: &[u8], force: bool) -> Result<()> {
    if !force && path.exists() {
        bail!("{} exists (use --force to replace it)", path.display());
    }
    std::fs::write(path, data).with_context(|| format!("cannot write {}", path.display()))
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
            spec,
            label,
            table,
            container,
            fixed,
            force,
        } => {
            let opts = CreateOptions {
                label,
                container: container.map(Into::into),
                scheme: match table {
                    Table::None => PartitionScheme::None,
                    Table::Mbr => PartitionScheme::Mbr,
                    Table::Gpt => PartitionScheme::Gpt,
                },
                dynamic: !fixed,
                overwrite: force,
            };
            Volume::create(&image, spec.spec()?, &opts)
                .with_context(|| format!("cannot create {}", image.display()))?;
            let info = Volume::open(&image, &OpenOptions::default())?.info()?;
            if json {
                return print_json(&info);
            }
            println!(
                "created {} ({}, {}, {} bytes free)",
                image.display(),
                info.container.kind.name(),
                info.floppy_format.unwrap_or(&info.fs_type),
                info.free_bytes
            );
        }
        Cmd::Info(img) => {
            let (container, layout) = probe(&img.image)?;
            let vol = open(&img, false);
            if json {
                return match vol {
                    Ok(v) => print_json(&v.info()?),
                    Err(_) => {
                        print_json(&serde_json::json!({ "container": container, "layout": layout }))
                    }
                };
            }
            println!("image        {}", img.image.display());
            println!(
                "container    {} ({}), {} bytes",
                container.kind.name(),
                container.variant,
                container.virtual_size
            );
            if let Some(m) = &container.member {
                println!(
                    "member       {m} ({} image(s) in the archive)",
                    container.members.len()
                );
            }
            if let Some(c) = &container.comment {
                println!("comment      {c}");
            }
            print_layout(&layout);
            match vol {
                Ok(v) => print_volume(&v.info()?),
                Err(e) => println!("volume       not readable: {e:#}"),
            }
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
        Cmd::Label { img, label, clear } => {
            if label.is_some() || clear {
                open_image(&img, true)?.set_label(label.as_deref())?;
            }
            let l = open(&img, false)?.label()?;
            if json {
                return print_json(&serde_json::json!({ "label": l }));
            }
            println!("{}", if l.is_empty() { "(no label)" } else { &l });
        }
        Cmd::Boot {
            img,
            export,
            import,
            mbr,
            oem,
            serial,
            heads,
            sectors_per_track,
            hidden_sectors,
            drive,
            force,
        } => {
            let edit = BootSectorEdit {
                oem_name: oem,
                volume_id: serial,
                sectors_per_track,
                heads,
                hidden_sectors,
                drive_number: drive,
            };
            let editing = edit.oem_name.is_some()
                || edit.volume_id.is_some()
                || edit.sectors_per_track.is_some()
                || edit.heads.is_some()
                || edit.hidden_sectors.is_some()
                || edit.drive_number.is_some();
            if mbr && editing {
                bail!("--oem/--serial/--heads/… edit the volume boot sector, not the MBR");
            }
            let image = open_image(&img, import.is_some() || editing)?;
            if let Some(f) = &import {
                let data =
                    std::fs::read(f).with_context(|| format!("cannot read {}", f.display()))?;
                if mbr {
                    image.import_mbr_code(&data)?
                } else {
                    image.import_boot_sector(&data)?
                }
            }
            if editing {
                image.edit_boot_sector(&edit)?;
            }
            if let Some(f) = &export {
                let sector = if mbr {
                    image.mbr()?
                } else {
                    image.boot_sector()?
                };
                write_new(f, &sector, force)?;
                if !json {
                    println!("saved {}", f.display());
                }
            }
            if mbr {
                let s = image.mbr()?;
                if json {
                    return print_json(&serde_json::json!({
                        "disk_signature": format!("{:08X}", u32::from_le_bytes(s[440..444].try_into().unwrap())),
                        "has_boot_code": s[..440].iter().any(|&b| b != 0),
                        "layout": image.layout(),
                    }));
                }
                println!(
                    "disk signature {:08X}",
                    u32::from_le_bytes(s[440..444].try_into().unwrap())
                );
                println!(
                    "boot code      {}",
                    if s[..440].iter().any(|&b| b != 0) {
                        "present"
                    } else {
                        "none"
                    }
                );
                print_layout(image.layout());
            } else {
                let b = image.boot_sector_info()?;
                if json {
                    return print_json(&b);
                }
                let rows: [(&str, String); 19] = [
                    ("jump", b.jump),
                    ("oem name", format!("{:?}", b.oem_name)),
                    ("bytes/sector", b.bytes_per_sector.to_string()),
                    ("sectors/cluster", b.sectors_per_cluster.to_string()),
                    ("reserved sectors", b.reserved_sectors.to_string()),
                    ("FATs", b.fats.to_string()),
                    ("root entries", b.root_entries.to_string()),
                    ("total sectors", b.total_sectors.to_string()),
                    ("media", b.media),
                    ("sectors/FAT", b.sectors_per_fat.to_string()),
                    ("sectors/track", b.sectors_per_track.to_string()),
                    ("heads", b.heads.to_string()),
                    ("hidden sectors", b.hidden_sectors.to_string()),
                    ("drive number", b.drive_number),
                    ("serial", b.volume_id.unwrap_or_else(|| "-".into())),
                    ("label", b.label.unwrap_or_else(|| "-".into())),
                    ("fs type field", b.fs_type.unwrap_or_else(|| "-".into())),
                    ("file system", b.fat_type),
                    (
                        "boot code",
                        format!(
                            "{}{}",
                            if b.has_boot_code { "present" } else { "none" },
                            if b.signature_55aa {
                                ""
                            } else {
                                ", no 55AA signature"
                            }
                        ),
                    ),
                ];
                for (k, v) in rows {
                    println!("{k:<17} {v}");
                }
            }
        }
        Cmd::Defrag {
            img,
            analyze,
            zero_free,
        } => {
            let image = open_image(&img, !analyze)?;
            let r = if analyze {
                image.analyze_fragmentation()?
            } else {
                image.defragment(zero_free)?
            };
            if json {
                return print_json(&r);
            }
            println!("files/folders {} / {}", r.files, r.dirs);
            println!("fragmented    {} ({} extents)", r.fragmented, r.fragments);
            println!(
                "clusters      {} used, {} free, {} lost ({} bytes each)",
                r.used_clusters, r.free_clusters, r.lost_clusters, r.cluster_size
            );
            if !analyze {
                println!("moved         {} clusters", r.moved_clusters);
            }
        }
        Cmd::Resize { img, spec } => {
            let s = Volume::change_format(&img.image, &img.opts(true), spec.spec()?)
                .with_context(|| format!("cannot change the format of {}", img.image.display()))?;
            let info = open(&img, false)?.info()?;
            if json {
                return print_json(&info);
            }
            println!(
                "rebuilt as {} with {} file(s), {} folder(s); {} bytes free",
                info.floppy_format.unwrap_or(&info.fs_type),
                s.files,
                s.dirs,
                info.free_bytes
            );
        }
        Cmd::Convert {
            source,
            dest,
            container,
            fixed,
            member,
            force,
        } => {
            let r = convert(
                &source,
                member.as_deref(),
                &dest,
                container.map(Into::into),
                !fixed,
                force,
            )
            .with_context(|| {
                format!("cannot convert {} to {}", source.display(), dest.display())
            })?;
            if json {
                return print_json(&r);
            }
            println!(
                "wrote {} ({} {}, {} bytes, {} stored)",
                dest.display(),
                r.container.kind.name(),
                r.container.variant,
                r.bytes,
                r.stored
            );
        }
        Cmd::Iso {
            folder,
            output,
            label,
            boot,
            efi,
            no_emulation,
            load_sectors,
            force,
        } => {
            let opts = IsoBuildOptions {
                volume_id: label,
                boot_image: boot,
                boot_platform: if efi {
                    BootPlatform::Uefi
                } else {
                    BootPlatform::Bios
                },
                no_emulation,
                load_sectors,
                overwrite: force,
            };
            let r = build_iso(&folder, &output, &opts)
                .with_context(|| format!("cannot build {}", output.display()))?;
            if json {
                return print_json(&r);
            }
            println!(
                "built {} ({} bytes): {} file(s), {} folder(s){}",
                output.display(),
                r.image_size,
                r.files,
                r.dirs,
                if r.bootable { ", bootable" } else { "" }
            );
        }
        Cmd::BootImage {
            img,
            entry,
            out,
            force,
        } => {
            let data = open(&img, false)?.boot_image(entry)?;
            write_new(&out, &data, force)?;
            if json {
                return print_json(&serde_json::json!({ "file": out, "bytes": data.len() }));
            }
            println!(
                "saved boot image {entry} to {} ({} bytes)",
                out.display(),
                data.len()
            );
        }
    }
    Ok(())
}

fn print_layout(layout: &Layout) {
    match layout {
        Layout::BareVolume => println!("layout       bare volume (no partition table)"),
        Layout::Iso => println!("layout       ISO 9660 CD/DVD image"),
        Layout::Unknown => println!("layout       unknown"),
        Layout::Mbr(parts) | Layout::Gpt(parts) => {
            println!(
                "layout       {}",
                if matches!(layout, Layout::Gpt(_)) {
                    "GPT"
                } else {
                    "MBR"
                }
            );
            for p in parts {
                println!(
                    "  part {:<3}   {:<6} {:<18} start {:>10}  {:>14} bytes  {}{}{}",
                    p.index,
                    if p.scheme == "mbr" {
                        p.type_id.as_str()
                    } else {
                        ""
                    },
                    p.type_name,
                    p.start_lba,
                    p.size_bytes(),
                    p.fs.unwrap_or("-"),
                    if p.bootable { "  boot" } else { "" },
                    if p.name.is_empty() {
                        String::new()
                    } else {
                        format!("  \"{}\"", p.name)
                    }
                );
            }
        }
    }
}

fn print_volume(i: &VolumeInfo) {
    if let Some(p) = &i.partition {
        println!("opened       partition {}", p.index);
    }
    println!(
        "file system  {}{}",
        i.fs_type,
        if i.fs_writable { "" } else { " (read-only)" }
    );
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
    if !i.volume_id.is_empty() {
        println!("serial       {}", i.volume_id);
    }
    println!(
        "{:<12} {}",
        if i.iso.is_some() { "system" } else { "oem" },
        i.oem_name
    );
    println!("sector size  {}", i.bytes_per_sector);
    println!("cluster size {}", i.cluster_size);
    println!(
        "capacity     {} bytes ({} clusters)",
        i.total_bytes, i.total_clusters
    );
    if i.fs_writable {
        println!(
            "free         {} bytes ({} clusters)",
            i.free_bytes, i.free_clusters
        );
    }
    if let Some(iso) = &i.iso {
        if !iso.application.is_empty() {
            println!("application  {}", iso.application);
        }
        if !iso.created.is_empty() {
            println!("created      {}", iso.created);
        }
        for b in &iso.boot_entries {
            println!(
                "boot {}       {} {}{}, {} bytes at block {}",
                b.index,
                b.platform,
                b.media,
                if b.bootable { "" } else { " (not bootable)" },
                b.image_size,
                b.lba
            );
        }
    }
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

fn parse_byte(s: &str) -> std::result::Result<u8, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u8::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|_| format!("{s:?} is not a byte value like 0x80"))
}

/// `512M`, `2G`, `1440K`, or plain bytes.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim().to_ascii_uppercase();
    let s = s.trim_end_matches('B');
    let (num, mult) = match s.chars().last() {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        Some('T') => (&s[..s.len() - 1], 1 << 40),
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
