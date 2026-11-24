//! `s0`: the SectorZero command line.

mod sfx;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use sectorzero_core::{
    BootPlatform, BootSectorEdit, Checksums, ContainerKind, CreateOptions, DriveWriteOptions,
    Entry, EntryEdit, FLOPPY_FORMATS, FloppyFormat, FsKind, Geometry, Image, IsoBuildOptions,
    Layout, ListingFormat, NewImage, OpenOptions, PartitionScheme, Stats, Volume, VolumeInfo,
    build_iso, checksum_file, convert_with_progress, create_differencing, drive_info, list_drives,
    probe, render_listing, truncate_image, write_image_to_drive,
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
    /// Image file: .ima .img .imz .vhd .vhdx .vmdk .vdi .qcow2 .iso .cue .nrg …, or a block device
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
enum FsArg {
    Fat,
    Ext2,
    Ext3,
    Ext4,
    Exfat,
}

#[derive(Clone, Copy, ValueEnum)]
enum Container {
    Raw,
    Imz,
    Vhd,
    Vhdx,
    Vmdk,
    Vdi,
    Qcow2,
    Nhd,
    Hdi,
    Fdi,
}

impl From<Container> for ContainerKind {
    fn from(c: Container) -> Self {
        match c {
            Container::Raw => ContainerKind::Raw,
            Container::Imz => ContainerKind::Imz,
            Container::Vhd => ContainerKind::Vhd,
            Container::Vhdx => ContainerKind::Vhdx,
            Container::Vmdk => ContainerKind::Vmdk,
            Container::Vdi => ContainerKind::Vdi,
            Container::Qcow2 => ContainerKind::Qcow2,
            Container::Nhd => ContainerKind::Nhd,
            Container::Hdi => ContainerKind::Hdi,
            Container::Fdi => ContainerKind::Fdi,
        }
    }
}

#[derive(clap::Args)]
struct FormatSpec {
    /// Floppy format id (see `s0 formats`), e.g. 1440, 720, dmf2048.
    #[arg(
        short,
        long,
        conflicts_with_all = ["size", "like"],
        required_unless_present_any = ["size", "like", "sectors"]
    )]
    format: Option<String>,
    /// Size of a hard disk / volume image instead of a floppy, e.g. 32M, 512M, 2G.
    #[arg(short, long)]
    size: Option<String>,
    /// Copy the geometry of another image, or of a 512-byte boot sector file (.bin).
    #[arg(long)]
    like: Option<PathBuf>,
    #[command(flatten)]
    geo: GeometryArgs,
}

/// Any of these turns the format into a custom one (WinImage's custom format).
#[derive(clap::Args)]
#[command(next_help_heading = "Custom geometry")]
struct GeometryArgs {
    /// Total 512-byte sectors.
    #[arg(long)]
    sectors: Option<u32>,
    #[arg(long)]
    sectors_per_track: Option<u16>,
    #[arg(long)]
    heads: Option<u16>,
    /// Cluster size, e.g. 512, 2K, 32K.
    #[arg(long)]
    cluster: Option<String>,
    /// Root folder entries (FAT12/16).
    #[arg(long)]
    root_entries: Option<u16>,
    /// Number of FAT copies (1 or 2).
    #[arg(long)]
    fats: Option<u8>,
    /// Media descriptor byte, e.g. 0xF0 (floppy) or 0xF8 (hard disk).
    #[arg(long, value_parser = parse_byte)]
    media: Option<u8>,
    /// FAT type: 12, 16 or 32.
    #[arg(long)]
    fat: Option<u8>,
    #[arg(long)]
    hidden_sectors: Option<u32>,
    /// BIOS drive number, e.g. 0x00 (floppy) or 0x80 (hard disk).
    #[arg(long, value_parser = parse_byte)]
    drive: Option<u8>,
}

impl GeometryArgs {
    fn any(&self) -> bool {
        self.sectors.is_some()
            || self.sectors_per_track.is_some()
            || self.heads.is_some()
            || self.cluster.is_some()
            || self.root_entries.is_some()
            || self.fats.is_some()
            || self.media.is_some()
            || self.fat.is_some()
            || self.hidden_sectors.is_some()
            || self.drive.is_some()
    }
}

/// The boot sector of an image, or the contents of a 512-byte `.bin` file.
fn read_boot_sector(p: &Path) -> Result<Vec<u8>> {
    let len = std::fs::metadata(p)
        .with_context(|| format!("cannot read {}", p.display()))?
        .len();
    if len == 512 {
        return Ok(std::fs::read(p)?);
    }
    Ok(Image::open(p, &OpenOptions::default())
        .with_context(|| format!("cannot open {}", p.display()))?
        .boot_sector()?)
}

impl FormatSpec {
    fn spec(&self) -> Result<NewImage> {
        if self.like.is_some() || self.geo.any() {
            return self.custom();
        }
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

    fn custom(&self) -> Result<NewImage> {
        let mut g = if let Some(p) = &self.like {
            Geometry::from_boot_sector(&read_boot_sector(p)?)
                .with_context(|| format!("{} has no usable FAT boot sector", p.display()))?
        } else if let Some(f) = &self.format {
            Geometry::from_floppy(
                FloppyFormat::find(f)
                    .with_context(|| format!("unknown format {f:?}; see `s0 formats`"))?,
            )
        } else {
            Geometry {
                total_sectors: 0,
                sectors_per_track: 32,
                heads: 64,
                bytes_per_cluster: 0,
                root_entries: 0,
                fats: 2,
                media: 0xF8,
                fat_bits: 0,
                hidden_sectors: 0,
                drive_number: 0x80,
            }
        };
        if let Some(s) = &self.size {
            g.total_sectors =
                u32::try_from(parse_size(s)? / 512).context("the size is too large for FAT")?;
        }
        let a = &self.geo;
        if let Some(v) = a.sectors {
            g.total_sectors = v;
        }
        if let Some(v) = a.sectors_per_track {
            g.sectors_per_track = v;
        }
        if let Some(v) = a.heads {
            g.heads = v;
        }
        if let Some(v) = &a.cluster {
            g.bytes_per_cluster = u32::try_from(parse_size(v)?).context("cluster size")?;
        }
        if let Some(v) = a.root_entries {
            g.root_entries = v;
        }
        if let Some(v) = a.fats {
            g.fats = v;
        }
        if let Some(v) = a.media {
            g.media = v;
        }
        if let Some(v) = a.fat {
            g.fat_bits = v;
        }
        if let Some(v) = a.hidden_sectors {
            g.hidden_sectors = v;
        }
        if let Some(v) = a.drive {
            g.drive_number = v;
        }
        if g.total_sectors == 0 {
            bail!("a custom format needs --sectors or --size");
        }
        Ok(NewImage::Custom(g))
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ListFmt {
    Txt,
    Csv,
    Html,
}

impl From<ListFmt> for ListingFormat {
    fn from(f: ListFmt) -> Self {
        match f {
            ListFmt::Txt => ListingFormat::Text,
            ListFmt::Csv => ListingFormat::Csv,
            ListFmt::Html => ListingFormat::Html,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// List the built-in floppy formats.
    Formats,
    /// Create a new empty formatted image. The container follows the extension
    /// (.imz .vhd .vhdx .vmdk .vdi .qcow2 .nhd .hdi .fdi; raw otherwise) unless --container
    /// is given.
    New {
        image: PathBuf,
        #[command(flatten)]
        spec: FormatSpec,
        #[arg(short, long)]
        label: Option<String>,
        /// File system: FAT (12/16/32 from the size), exFAT (1 MiB and up), ext2, ext3 (ext2
        /// with a journal) or ext4 (extents and metadata checksums; journaled from 2 MiB up).
        #[arg(long = "fs", value_enum, default_value = "fat")]
        fs: FsArg,
        /// Partition table for a hard disk image (one partition at 1 MiB).
        #[arg(short, long, value_enum, default_value = "none")]
        table: Table,
        #[arg(short, long, value_enum)]
        container: Option<Container>,
        /// Fixed-size VHD/VHDX/VMDK/VDI (default: dynamic, growing as data is written).
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
        /// Write a listing (always recursive) as text, CSV or HTML instead.
        #[arg(long, value_enum)]
        format: Option<ListFmt>,
        /// File for --format (default: print it).
        #[arg(short, long, requires = "format")]
        out: Option<PathBuf>,
        /// Overwrite the --out file.
        #[arg(long)]
        force: bool,
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
    /// Copy an image into another container (e.g. IMA → VHDX, VMDK → IMZ). With a drive as
    /// SOURCE (see `s0 drives`), reads the drive into an image.
    Convert {
        source: PathBuf,
        dest: PathBuf,
        /// Container of the destination (default: from its extension).
        #[arg(short, long, value_enum)]
        container: Option<Container>,
        /// Fixed-size VHD/VHDX/VMDK/VDI (default: dynamic).
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
    /// Show or change attributes and dates of files or folders inside the image (FAT and
    /// ext; on ext, read-only is the absence of write permission and H/S/A do not exist).
    Attrib {
        #[command(flatten)]
        img: Img,
        #[arg(required = true)]
        paths: Vec<String>,
        /// Attributes to set: any of R (read-only), H (hidden), S (system), A (archive).
        /// ext has only R.
        #[arg(long, value_name = "RHSA")]
        set: Option<String>,
        /// Attributes to clear.
        #[arg(long, value_name = "RHSA")]
        clear: Option<String>,
        /// Last-modified time, local: YYYY-MM-DD[ HH:MM[:SS]].
        #[arg(long)]
        modified: Option<String>,
        /// Creation time, local: YYYY-MM-DD[ HH:MM[:SS]].
        #[arg(long)]
        created: Option<String>,
        /// Last-access time, local: YYYY-MM-DD[ HH:MM[:SS]] (FAT keeps only the date).
        #[arg(long)]
        accessed: Option<String>,
        /// Apply to everything inside the given folders too.
        #[arg(short, long)]
        recursive: bool,
    },
    /// CRC32, Adler-32, MD5, SHA-1 and SHA-256 of the virtual disk, the image file, or a file
    /// inside the image.
    Checksum {
        #[command(flatten)]
        img: Img,
        /// A file inside the image. Without it, the whole virtual disk is hashed (the same
        /// for every container holding the same disk).
        path: Option<String>,
        /// Hash the image file byte for byte as stored instead (e.g. the compressed .imz).
        #[arg(long, conflicts_with = "path")]
        file: bool,
    },
    /// Cut unused space off the end of a raw floppy or volume image.
    Truncate { image: PathBuf },
    /// Create a differencing VHD (or Virtual PC undo disk, .vud) over PARENT: reads of
    /// unchanged sectors come from the parent, writes go only to the new file.
    Diff {
        parent: PathBuf,
        child: PathBuf,
        /// Replace CHILD if it exists.
        #[arg(long)]
        force: bool,
    },
    /// Replace a file inside an ISO image in place. The new file must fit in the sectors
    /// the old one occupies.
    IsoReplace {
        #[command(flatten)]
        img: Img,
        /// File inside the ISO.
        path: String,
        /// New contents (a file on this computer).
        file: PathBuf,
    },
    /// Package an image into a self-extracting executable (for the platform this s0 runs on).
    Sfx {
        image: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        /// Text shown when the executable runs.
        #[arg(long)]
        comment: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// List the physical drives (USB sticks, memory cards, floppy drives, disks). Their
    /// paths work wherever an image is expected, and as `s0 convert` sources.
    Drives,
    /// Write an image onto a physical drive sector for sector, replacing everything on it,
    /// then read it back to verify. Needs administrator rights (root on Linux).
    WriteDrive {
        /// Image file (any container s0 reads).
        image: PathBuf,
        /// Drive path from `s0 drives`, e.g. \\.\PhysicalDrive2, \\.\A: or /dev/sdb.
        drive: String,
        /// Image inside a WLZ list.
        #[arg(long)]
        member: Option<String>,
        /// Skip reading the drive back.
        #[arg(long)]
        no_verify: bool,
        /// Allow a fixed (non-removable) disk. The system disk is always refused.
        #[arg(long)]
        allow_fixed: bool,
        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
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
    if let Some(payload) = sfx::detect() {
        std::process::exit(sfx::run(payload));
    }
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

/// A one-line progress display on stderr (only when stderr is a terminal).
struct Progress {
    on: bool,
    last: u64,
    shown: bool,
}

impl Progress {
    fn new(json: bool) -> Progress {
        use std::io::IsTerminal;
        Progress {
            on: !json && std::io::stderr().is_terminal(),
            last: u64::MAX,
            shown: false,
        }
    }

    fn show(&mut self, phase: &str, done: u64, total: u64) {
        if !self.on || total == 0 {
            return;
        }
        let pct = done * 100 / total;
        if pct == self.last {
            return;
        }
        self.last = pct;
        self.shown = true;
        eprint!(
            "\r{phase} {pct:>3}%  {} / {} MiB   ",
            done >> 20,
            total.div_ceil(1 << 20)
        );
    }

    fn done(&mut self) {
        if self.shown {
            eprintln!();
            self.shown = false;
        }
        self.last = u64::MAX;
    }
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        bail!("{question} Pass --yes to confirm without a prompt.");
    }
    eprint!("{question} Type yes to continue: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("yes"))
}

fn human_size(n: u64) -> String {
    match n {
        0 => "-".into(),
        n if n >= 1 << 30 => format!("{:.1} GiB", n as f64 / f64::from(1u32 << 30)),
        n if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / f64::from(1u32 << 20)),
        n => format!("{} KiB", n >> 10),
    }
}

fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    match cli.cmd {
        Cmd::Drives => {
            let drives = list_drives().context("cannot list drives")?;
            if json {
                return print_json(&drives);
            }
            if drives.is_empty() {
                println!("no drives found");
            }
            println!(
                "{:<22} {:>10}  {:<17} {:<10} model",
                "path", "size", "bus", "volumes"
            );
            for d in &drives {
                let mut bus = d.bus.clone();
                if d.removable && !matches!(d.bus.as_str(), "USB" | "SD" | "MMC" | "Floppy") {
                    bus.push_str(" removable");
                }
                let mut model = d.model.clone();
                if d.system {
                    model.push_str("  [system disk]");
                }
                println!(
                    "{:<22} {:>10}  {:<17} {:<10} {}",
                    d.path,
                    if d.media {
                        human_size(d.size)
                    } else {
                        "no medium".into()
                    },
                    bus,
                    d.volumes.join(" "),
                    model
                );
            }
        }
        Cmd::WriteDrive {
            image,
            drive,
            member,
            no_verify,
            allow_fixed,
            yes,
        } => {
            let info = drive_info(&drive)
                .context("cannot list drives")?
                .with_context(|| format!("{drive} is not a drive (see s0 drives)"))?;
            if !yes
                && !info.system
                && !confirm(&format!(
                    "This replaces everything on {} with {}.",
                    info.describe(),
                    image.display()
                ))?
            {
                bail!("cancelled");
            }
            let mut bar = Progress::new(json);
            let mut phase = String::new();
            let r = write_image_to_drive(
                &image,
                &drive,
                &DriveWriteOptions {
                    verify: !no_verify,
                    allow_fixed,
                    member,
                },
                &mut |p, done, total| {
                    if p != phase {
                        bar.done();
                        phase = p.to_string();
                    }
                    bar.show(p, done, total)
                },
            )
            .with_context(|| format!("cannot write {} to {drive}", image.display()));
            bar.done();
            let r = r?;
            if json {
                return print_json(&r);
            }
            println!(
                "wrote {} bytes to {}{}",
                r.bytes,
                r.drive.describe(),
                if r.verified { ", verified" } else { "" }
            );
        }
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
            fs,
            table,
            container,
            fixed,
            force,
        } => {
            let opts = CreateOptions {
                label,
                fs: match fs {
                    FsArg::Fat => FsKind::Fat,
                    FsArg::Ext2 => FsKind::Ext2,
                    FsArg::Ext3 => FsKind::Ext3,
                    FsArg::Ext4 => FsKind::Ext4,
                    FsArg::Exfat => FsKind::Exfat,
                },
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
            format,
            out,
            force,
        } => {
            let vol = open(&img, false)?;
            if let Some(f) = format {
                let title = img
                    .image
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let text = render_listing(&title, &vol.walk(&dir)?, f.into());
                return match out {
                    Some(o) => {
                        write_new(&o, text.as_bytes(), force)?;
                        if !json {
                            println!("wrote {}", o.display());
                        }
                        Ok(())
                    }
                    None => {
                        print!("{text}");
                        Ok(())
                    }
                };
            }
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
            let mut bar = Progress::new(json);
            let r = convert_with_progress(
                &source,
                member.as_deref(),
                &dest,
                container.map(Into::into),
                !fixed,
                force,
                &mut |done, total| bar.show("copying", done, total),
            )
            .with_context(|| format!("cannot convert {} to {}", source.display(), dest.display()));
            bar.done();
            let r = r?;
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
        Cmd::Attrib {
            img,
            paths,
            set,
            clear,
            modified,
            created,
            accessed,
            recursive,
        } => {
            let mut edit = EntryEdit {
                modified,
                created,
                accessed,
                ..Default::default()
            };
            for (letters, on) in [(&set, true), (&clear, false)] {
                for c in letters.as_deref().unwrap_or("").chars() {
                    let slot = match c.to_ascii_uppercase() {
                        'R' => &mut edit.read_only,
                        'H' => &mut edit.hidden,
                        'S' => &mut edit.system,
                        'A' => &mut edit.archive,
                        other => bail!("unknown attribute {other:?} (use R, H, S or A)"),
                    };
                    if slot.is_some_and(|v| v != on) {
                        bail!("attribute {c} is both set and cleared");
                    }
                    *slot = Some(on);
                }
            }
            if edit.is_empty() {
                let vol = open(&img, false)?;
                let mut shown = Vec::new();
                for p in &paths {
                    let e = vol
                        .stat(p)?
                        .with_context(|| format!("{p}: not found (or the root folder)"))?;
                    shown.push(e);
                }
                if json {
                    return print_json(&shown);
                }
                for e in shown {
                    println!(
                        "{}  modified {}  created {}  accessed {}  /{}",
                        e.attributes.flags(),
                        e.modified,
                        if e.created.is_empty() {
                            "-"
                        } else {
                            &e.created
                        },
                        if e.accessed.is_empty() {
                            "-"
                        } else {
                            &e.accessed
                        },
                        e.path
                    );
                }
                return Ok(());
            }
            let image = open_image(&img, true)?;
            let mut changed = 0;
            for p in &paths {
                changed += image
                    .edit_entry(p, &edit, recursive)
                    .with_context(|| format!("cannot change {p}"))?;
            }
            if json {
                return print_json(&serde_json::json!({ "changed": changed }));
            }
            println!(
                "changed {changed} entr{}",
                if changed == 1 { "y" } else { "ies" }
            );
        }
        Cmd::Checksum { img, path, file } => {
            let sums = if file {
                checksum_file(&img.image)?
            } else if let Some(p) = &path {
                open(&img, false)?.checksum(p)?
            } else {
                open_image(&img, false)?.checksum_disk()?
            };
            if json {
                return print_json(&sums);
            }
            let what = match (&path, file) {
                (Some(p), _) => format!("file /{}", p.trim_start_matches(['/', '\\'])),
                (None, true) => "image file as stored".into(),
                (None, false) => "virtual disk".into(),
            };
            print_checksums(&what, &sums);
        }
        Cmd::Diff {
            parent,
            child,
            force,
        } => {
            let info = create_differencing(&child, &parent, force)
                .with_context(|| format!("cannot create {}", child.display()))?;
            if json {
                return print_json(&info);
            }
            println!(
                "created {} ({} bytes) over {}",
                child.display(),
                info.virtual_size,
                parent.display()
            );
        }
        Cmd::Truncate { image } => {
            let r = truncate_image(&image)
                .with_context(|| format!("cannot truncate {}", image.display()))?;
            if json {
                return print_json(&r);
            }
            if r.new_size < r.old_size {
                println!(
                    "truncated {} from {} to {} bytes",
                    image.display(),
                    r.old_size,
                    r.new_size
                );
            } else {
                println!("{} has no unused space at the end", image.display());
            }
        }
        Cmd::IsoReplace { img, path, file } => {
            let data =
                std::fs::read(&file).with_context(|| format!("cannot read {}", file.display()))?;
            open_image(&img, true)?
                .iso_replace_file(&path, &data)
                .with_context(|| format!("cannot replace {path}"))?;
            if json {
                return print_json(&serde_json::json!({ "path": path, "bytes": data.len() }));
            }
            println!("replaced {path} ({} bytes)", data.len());
        }
        Cmd::Sfx {
            image,
            out,
            comment,
            force,
        } => {
            let size = sfx::create(&image, &out, comment, force)?;
            if json {
                return print_json(&serde_json::json!({ "file": out, "bytes": size }));
            }
            println!("wrote {} ({size} bytes)", out.display());
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
    if let Some(why) = &i.read_only_reason {
        println!("read-only    {why}");
    }
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
        let what = if i.fs_type.starts_with("ext") {
            "uuid"
        } else {
            "serial"
        };
        println!("{what:<12} {}", i.volume_id);
    }
    if !i.oem_name.is_empty() {
        println!(
            "{:<12} {}",
            if i.iso.is_some() { "system" } else { "oem" },
            i.oem_name
        );
    }
    println!("sector size  {}", i.bytes_per_sector);
    println!("cluster size {}", i.cluster_size);
    println!(
        "capacity     {} bytes ({} clusters)",
        i.total_bytes, i.total_clusters
    );
    if i.iso.is_none() {
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

fn print_checksums(what: &str, c: &Checksums) {
    println!("{what}, {} bytes", c.bytes);
    println!("  CRC32    {}", c.crc32);
    println!("  Adler-32 {}", c.adler32);
    println!("  MD5      {}", c.md5);
    println!("  SHA-1    {}", c.sha1);
    println!("  SHA-256  {}", c.sha256);
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
