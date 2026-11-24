//! `s0`: the SectorZero command line.

mod sfx;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use sectorzero_core::{
    BootPlatform, BootSectorEdit, Checksums, ContainerKind, CreateOptions, DriveWipeOptions,
    DriveWriteOptions, Entry, EntryEdit, FLOPPY_FORMATS, FloppyFormat, ForkMode, FsKind, Geometry,
    Image, IsoBuildOptions, Layout, ListingFormat, MEDIA_FORMATS, MediaFormat, MediaLayout,
    NewImage, OpenOptions, PartitionScheme, Stats, Volume, VolumeInfo, WipePattern, build_iso,
    checksum_file, convert_with_progress, create_differencing, drive_info, list_drives, probe,
    render_listing, truncate_image, wipe_drive, write_image_to_drive,
};
use sectorzero_i18n::{t, tr};

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
    /// Password of encrypted IMZ/WLZ images (WinImage's password protection). "-" reads
    /// it from the first line of standard input.
    #[arg(long, global = true, value_name = "PASSWORD")]
    password: Option<String>,
    /// Language of messages and help: en, cs, de, es, fr, hu, it, ja, ko, nl, pl, pt-BR, ru,
    /// tr, uk, zh-CN or zh-TW. Default: SECTORZERO_LANG, LANG or the system's language.
    #[arg(long, global = true, value_name = "LANG")]
    lang: Option<String>,
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
    /// Apple partition map (HFS only).
    Apm,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Forks {
    Data,
    Appledouble,
    Macbinary,
}

impl Forks {
    fn mode(self) -> ForkMode {
        match self {
            Forks::Data => ForkMode::Data,
            Forks::Appledouble => ForkMode::AppleDouble,
            Forks::Macbinary => ForkMode::MacBinary,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum FsArg {
    Fat,
    Ext2,
    Ext3,
    Ext4,
    Exfat,
    Hfs,
    #[value(hide = true)]
    Hfsplus,
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
    /// Floppy format or removable medium id (see `s0 formats`), e.g. 1440, 720, dmf2048,
    /// zip100, ls120. Media presets are laid out the way their vendor formatted them.
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
        .with_context(|| t!("cannot read {}", p.display()))?
        .len();
    if len == 512 {
        return Ok(std::fs::read(p)?);
    }
    Ok(Image::open(p, &OpenOptions::default())
        .with_context(|| t!("cannot open {}", p.display()))?
        .boot_sector()?)
}

impl FormatSpec {
    fn spec(&self) -> Result<NewImage> {
        if self.like.is_some() || self.geo.any() {
            return self.custom();
        }
        Ok(match (&self.format, &self.size) {
            (Some(f), _) => match (FloppyFormat::find(f), MediaFormat::find(f)) {
                (Some(f), _) => NewImage::Floppy(f),
                (None, Some(m)) => NewImage::Media(m),
                _ => bail!(t!(
                    "unknown format {}; see `s0 formats`",
                    format!("{:?}", f)
                )),
            },
            (None, Some(s)) => NewImage::Volume {
                size_bytes: parse_size(s)?,
            },
            (None, None) => unreachable!("clap requires one of --format/--size"),
        })
    }

    fn custom(&self) -> Result<NewImage> {
        let mut g =
            if let Some(p) = &self.like {
                Geometry::from_boot_sector(&read_boot_sector(p)?)
                    .with_context(|| t!("{} has no usable FAT boot sector", p.display()))?
            } else if let Some(f) = &self.format {
                Geometry::from_floppy(FloppyFormat::find(f).with_context(|| {
                    t!("unknown format {}; see `s0 formats`", format!("{:?}", f))
                })?)
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
                u32::try_from(parse_size(s)? / 512).context(t!("the size is too large for FAT"))?;
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
            g.bytes_per_cluster = u32::try_from(parse_size(v)?).context(t!("cluster size"))?;
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
            bail!(t!("a custom format needs --sectors or --size"));
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
        /// Save the IMZ image encrypted with this password ("-": read it from standard
        /// input).
        #[arg(long, value_name = "PASSWORD")]
        encrypt: Option<String>,
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
        /// Macintosh resource forks and Finder info (HFS / HFS+): data = data fork only,
        /// appledouble = plus a `._name` file per file, macbinary = one MacBinary III
        /// `name.bin` per file.
        #[arg(long, value_enum, default_value_t = Forks::Data)]
        forks: Forks,
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
        /// Macintosh resource forks and Finder info (HFS / HFS+): data = plain files,
        /// appledouble = take them from the `._name` file next to each file, macbinary =
        /// unpack MacBinary files (other files go in as they are).
        #[arg(long, value_enum, default_value_t = Forks::Data)]
        forks: Forks,
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
        /// New label (FAT: up to 11 characters; HFS: 27, HFS+: 255). Omit to show the
        /// current one.
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
    /// On a partitioned (MBR/GPT) disk, `--size` changes the disk size: the last partition
    /// grows or shrinks with it (its FAT volume is rebuilt, keeping its files), or with
    /// --keep-partitions only the disk and partition table change.
    Resize {
        #[command(flatten)]
        img: Img,
        #[command(flatten)]
        spec: FormatSpec,
        /// Partitioned disks: leave every partition's size alone.
        #[arg(long)]
        keep_partitions: bool,
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
        /// Save an IMZ destination encrypted with this password (--password is the
        /// source's).
        #[arg(long, value_name = "PASSWORD")]
        encrypt: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Encrypt an IMZ/WLZ image with a password, change its password (give the current one
    /// with --password), or store it unencrypted again.
    Password {
        #[command(flatten)]
        img: Img,
        /// The new password ("-": read it from standard input).
        #[arg(
            long,
            value_name = "PASSWORD",
            conflicts_with = "remove",
            required_unless_present = "remove"
        )]
        set: Option<String>,
        /// Store the image unencrypted.
        #[arg(long)]
        remove: bool,
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
    /// then read it back to verify. The source can also be another drive (drive-to-drive
    /// copy). Needs administrator rights (root on Linux).
    WriteDrive {
        /// Image file (any container s0 reads), or a drive path to copy from.
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
    /// Overwrite a whole physical drive with zeros (or random data, then zeros), removing
    /// its partitions and file systems. Needs administrator rights (root on Linux).
    WipeDrive {
        /// Drive path from `s0 drives`.
        drive: String,
        /// A pass of pseudo-random data before the zeros.
        #[arg(long)]
        random: bool,
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
    /// Eject the medium of a removable drive (Zip, Jaz, SuperDisk, MO, CD). Needs
    /// administrator rights (root on Linux).
    Eject {
        /// Drive path from `s0 drives`.
        drive: String,
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
    // `run` is one large match; unoptimized builds need more than Windows' 1 MiB main
    // thread stack for it, so the work runs on a thread with room to spare.
    let code = std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(|| {
            sectorzero_i18n::init();
            if let Some(payload) = sfx::detect() {
                return sfx::run(payload);
            }
            if let Some(lang) = lang_arg() {
                sectorzero_i18n::set_language(&lang);
            }
            let matches = localize(Cli::command()).get_matches();
            let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
            match run(cli) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("s0: {}", error_text(&e));
                    1
                }
            }
        })
        .expect("cannot start the main thread")
        .join()
        .unwrap_or(101);
    std::process::exit(code);
}

/// `--lang` from the command line, read before clap so that the help is translated too.
fn lang_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--" {
            break;
        }
        if a == "--lang" {
            return args.next();
        }
        if let Some(v) = a.strip_prefix("--lang=") {
            return Some(v.to_string());
        }
    }
    None
}

/// The command line with its help in the current language: descriptions, headings and
/// clap's own help and version flags. Clap's error messages stay English.
fn localize(mut cmd: clap::Command) -> clap::Command {
    if sectorzero_i18n::language() == "en" {
        return cmd;
    }
    cmd.build();
    localize_built(cmd)
}

fn localize_built(cmd: clap::Command) -> clap::Command {
    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }
    let mut cmd = cmd
        .help_template(format!(
            "{{before-help}}{{about-with-newline}}\n{} {{usage}}\n\n{{all-args}}{{after-help}}",
            t!("Usage:")
        ))
        .subcommand_help_heading(leak(t!("Commands")))
        .mut_args(|a| {
            let help = a.get_help().map(|h| tr(&h.to_string()));
            let long = a.get_long_help().map(|h| tr(&h.to_string()));
            let heading = match a.get_help_heading() {
                Some(h) => tr(h),
                None if a.is_positional() => t!("Arguments"),
                None => t!("Options"),
            };
            let mut a = a.help_heading(leak(heading));
            if let Some(h) = help {
                a = a.help(h);
            }
            if let Some(h) = long {
                a = a.long_help(h);
            }
            a
        })
        .mut_subcommands(localize_built);
    if let Some(about) = cmd.get_about().map(|a| tr(&a.to_string())) {
        cmd = cmd.about(about);
    }
    if let Some(about) = cmd.get_long_about().map(|a| tr(&a.to_string())) {
        cmd = cmd.long_about(about);
    }
    cmd
}

/// Every help text of the command line: the keys its translations need.
#[cfg(test)]
fn help_keys() -> Vec<String> {
    fn walk(cmd: &clap::Command, out: &mut Vec<String>) {
        let mut add = |s: Option<String>| {
            if let Some(s) = s.filter(|s| !s.trim().is_empty())
                && !out.contains(&s)
            {
                out.push(s);
            }
        };
        add(cmd.get_about().map(|a| a.to_string()));
        add(cmd.get_long_about().map(|a| a.to_string()));
        for a in cmd.get_arguments() {
            add(a.get_help().map(|h| h.to_string()));
            add(a.get_long_help().map(|h| h.to_string()));
            add(a.get_help_heading().map(str::to_string));
        }
        for s in cmd.get_subcommands() {
            walk(s, out);
        }
    }
    let mut cmd = Cli::command();
    cmd.build();
    let mut out = Vec::new();
    walk(&cmd, &mut out);
    out
}

fn open(img: &Img, writable: bool) -> Result<Volume> {
    Volume::open(&img.image, &img.opts(writable))
        .with_context(|| t!("cannot open {}", img.image.display()))
}

fn open_image(img: &Img, writable: bool) -> Result<Image> {
    Image::open(&img.image, &img.opts(writable))
        .with_context(|| t!("cannot open {}", img.image.display()))
}

fn write_new(path: &Path, data: &[u8], force: bool) -> Result<()> {
    if !force && path.exists() {
        bail!(t!("{} exists (use --force to replace it)", path.display()));
    }
    std::fs::write(path, data).with_context(|| t!("cannot write {}", path.display()))
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
        let phase = match phase {
            "writing" => t!("writing"),
            "verifying" => t!("verifying"),
            "reading" => t!("reading"),
            "random" => t!("random data"),
            "zeroing" => t!("zeroing"),
            other => other.to_string(),
        };
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
        bail!(t!("{} Pass --yes to confirm without a prompt.", question));
    }
    eprint!("{}", t!("{} Type yes to continue: ", question));
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("yes"))
}

fn human_size(n: u64) -> String {
    match n {
        0 => "-".into(),
        n if n >= 1 << 30 => t!("{} GiB", format!("{:.1}", n as f64 / f64::from(1u32 << 30))),
        n if n >= 1 << 20 => t!("{} MiB", format!("{:.1}", n as f64 / f64::from(1u32 << 20))),
        n => t!("{} KiB", n >> 10),
    }
}

/// A password argument; "-" reads one line from standard input.
fn password_arg(value: Option<String>) -> Result<Option<String>> {
    match value.as_deref() {
        Some("-") => {
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context(t!("cannot read the password from standard input"))?;
            let pw = line.trim_end_matches(['\r', '\n']).to_string();
            if pw.is_empty() {
                bail!(t!("the password on standard input is empty"));
            }
            Ok(Some(pw))
        }
        Some("") => bail!(t!("the password is empty")),
        _ => Ok(value),
    }
}

fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    if let Some(l) = &cli.lang
        && sectorzero_i18n::matching(l).is_none()
    {
        let codes: Vec<&str> = sectorzero_i18n::LANGUAGES.iter().map(|(c, _)| *c).collect();
        bail!(t!(
            "unknown language {}; use one of {}",
            l,
            codes.join(", ")
        ));
    }
    sectorzero_core::set_fallback_password(password_arg(cli.password)?.as_deref());
    match cli.cmd {
        Cmd::Drives => {
            let drives = list_drives().context(t!("cannot list drives"))?;
            if json {
                return print_json(&drives);
            }
            if drives.is_empty() {
                println!("{}", t!("no drives found"));
            }
            println!(
                "{:<22} {:>10}  {:<17} {:<10} model",
                "path", "size", "bus", "volumes"
            );
            for d in &drives {
                let mut bus = d.bus.clone();
                if d.removable && !matches!(d.bus.as_str(), "USB" | "SD" | "MMC" | "Floppy") {
                    bus.push_str(&t!(" removable"));
                }
                let mut model = d.model.clone();
                if d.system {
                    model.push_str(&t!("  [system disk]"));
                }
                println!(
                    "{:<22} {:>10}  {:<17} {:<10} {}",
                    d.path,
                    if d.media {
                        human_size(d.size)
                    } else {
                        t!("no medium")
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
                .context(t!("cannot list drives"))?
                .with_context(|| t!("{} is not a drive (see s0 drives)", drive))?;
            if !yes
                && !info.system
                && !confirm(&t!(
                    "This replaces everything on {} with {}.",
                    tr(&info.describe()),
                    image.display()
                ))?
            {
                bail!(t!("cancelled"));
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
            .with_context(|| t!("cannot write {} to {}", image.display(), drive));
            bar.done();
            let r = r?;
            if json {
                return print_json(&r);
            }
            if r.verified {
                println!(
                    "{}",
                    t!(
                        "wrote {} bytes to {}, verified",
                        r.bytes,
                        tr(&r.drive.describe())
                    )
                );
            } else {
                println!(
                    "{}",
                    t!("wrote {} bytes to {}", r.bytes, tr(&r.drive.describe()))
                );
            }
        }
        Cmd::WipeDrive {
            drive,
            random,
            no_verify,
            allow_fixed,
            yes,
        } => {
            let info = drive_info(&drive)
                .context(t!("cannot list drives"))?
                .with_context(|| t!("{} is not a drive (see s0 drives)", drive))?;
            if !yes
                && !info.system
                && !confirm(&t!(
                    "This erases everything on {}, partitions included.",
                    tr(&info.describe())
                ))?
            {
                bail!(t!("cancelled"));
            }
            let mut bar = Progress::new(json);
            let mut phase = String::new();
            let r = wipe_drive(
                &drive,
                &DriveWipeOptions {
                    pattern: if random {
                        WipePattern::Random
                    } else {
                        WipePattern::Zero
                    },
                    verify: !no_verify,
                    allow_fixed,
                },
                &mut |p, done, total| {
                    if p != phase {
                        bar.done();
                        phase = p.to_string();
                    }
                    bar.show(p, done, total)
                },
            )
            .with_context(|| t!("cannot wipe {}", drive));
            bar.done();
            let r = r?;
            if json {
                return print_json(&r);
            }
            if r.verified {
                println!(
                    "{}",
                    t!(
                        "wiped {} bytes on {}, verified",
                        r.bytes,
                        tr(&r.drive.describe())
                    )
                );
            } else {
                println!(
                    "{}",
                    t!("wiped {} bytes on {}", r.bytes, tr(&r.drive.describe()))
                );
            }
        }
        Cmd::Eject { drive } => {
            sectorzero_core::eject_drive(&drive).with_context(|| t!("cannot eject {}", drive))?;
            if !json {
                println!("{}", t!("ejected {}", drive));
            }
        }
        Cmd::Formats => {
            if json {
                return print_json(&serde_json::json!({
                    "floppy": FLOPPY_FORMATS,
                    "media": MEDIA_FORMATS,
                }));
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
            println!("\n{:<8} {:>13}  {:>6}  layout", "id", "bytes", "sector");
            for m in MEDIA_FORMATS {
                println!(
                    "{:<8} {:>13}  {:>6}  {:<22} {}",
                    m.id,
                    m.size_bytes(),
                    m.sector_size,
                    match m.layout {
                        MediaLayout::Superfloppy => t!("no partition table"),
                        MediaLayout::Mbr { slot, start } => t!("MBR slot {} at {}", slot, start),
                    },
                    m.name
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
            encrypt,
            force,
        } => {
            if let Some(pw) = password_arg(encrypt)? {
                sectorzero_core::set_password(&image, Some(&pw));
            }
            let opts = CreateOptions {
                label,
                fs: match fs {
                    FsArg::Fat => FsKind::Fat,
                    FsArg::Ext2 => FsKind::Ext2,
                    FsArg::Ext3 => FsKind::Ext3,
                    FsArg::Ext4 => FsKind::Ext4,
                    FsArg::Exfat => FsKind::Exfat,
                    FsArg::Hfs => FsKind::Hfs,
                    FsArg::Hfsplus => FsKind::HfsPlus,
                },
                container: container.map(Into::into),
                scheme: match table {
                    Table::None => PartitionScheme::None,
                    Table::Mbr => PartitionScheme::Mbr,
                    Table::Gpt => PartitionScheme::Gpt,
                    Table::Apm => PartitionScheme::Apm,
                },
                dynamic: !fixed,
                overwrite: force,
            };
            Volume::create(&image, spec.spec()?, &opts)
                .with_context(|| t!("cannot create {}", image.display()))?;
            let info = Volume::open(&image, &OpenOptions::default())?.info()?;
            if json {
                return print_json(&info);
            }
            println!(
                "{}",
                t!(
                    "created {} ({}, {}, {} bytes free)",
                    image.display(),
                    tr(info.container.kind.name()),
                    info.floppy_format
                        .or(info.media_format)
                        .unwrap_or(&info.fs_type),
                    info.free_bytes
                )
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
            row(&t!("image"), img.image.display());
            row(
                &t!("container"),
                t!(
                    "{} ({}), {} bytes",
                    tr(container.kind.name()),
                    container.variant,
                    container.virtual_size
                ),
            );
            if let Some(m) = &container.member {
                row(
                    &t!("member"),
                    t!(
                        "{} ({} image(s) in the archive)",
                        m,
                        container.members.len()
                    ),
                );
            }
            if let Some(c) = &container.comment {
                row(&t!("comment"), c);
            }
            if container.encrypted {
                row(&t!("encryption"), t!("password (AES-128)"));
            }
            print_layout(&layout);
            match vol {
                Ok(v) => print_volume(&v.info()?),
                Err(e) => row(&t!("volume"), t!("not readable: {}", error_text(&e))),
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
                            println!("{}", t!("wrote {}", o.display()));
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
            forks,
        } => {
            let forks = forks.mode();
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
                    vol.extract_with(p, &out, force, forks)
                        .with_context(|| t!("extracting {}", format!("{:?}", p)))?,
                );
            }
            report(json, Verb::Extracted, &total, Some(&out))?;
        }
        Cmd::Inject {
            img,
            files,
            to,
            force,
            forks,
        } => {
            let vol = open(&img, true)?;
            let mut total = Stats::default();
            for f in &files {
                add(
                    &mut total,
                    vol.inject_with(f, &to, force, forks.mode())
                        .with_context(|| t!("injecting {}", f.display()))?,
                );
            }
            let free = vol.info()?.free_bytes;
            vol.close()?;
            report(json, Verb::Injected, &total, None)?;
            if !json {
                println!("{}", t!("{} bytes free", free));
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
                        .with_context(|| t!("removing {}", format!("{:?}", p)))?,
                );
            }
            vol.close()?;
            report(json, Verb::Removed, &total, None)?;
        }
        Cmd::Mv { img, from, to } => {
            let vol = open(&img, true)?;
            vol.rename(&from, &to)?;
            vol.close()?;
        }
        Cmd::Password { img, set, remove } => {
            let new = if remove { None } else { password_arg(set)? };
            open_image(&img, true)?
                .set_password(new.as_deref())
                .with_context(|| t!("cannot change the password of {}", img.image.display()))?;
            if json {
                return print_json(&serde_json::json!({ "encrypted": new.is_some() }));
            }
            if new.is_some() {
                println!("{}", t!("{} is now encrypted", img.image.display()));
            } else {
                println!("{}", t!("{} is now unencrypted", img.image.display()));
            }
        }
        Cmd::Label { img, label, clear } => {
            if label.is_some() || clear {
                open_image(&img, true)?.set_label(label.as_deref())?;
            }
            let l = open(&img, false)?.label()?;
            if json {
                return print_json(&serde_json::json!({ "label": l }));
            }
            println!("{}", if l.is_empty() { t!("(no label)") } else { l });
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
                bail!(t!(
                    "--oem/--serial/--heads/… edit the volume boot sector, not the MBR"
                ));
            }
            let image = open_image(&img, import.is_some() || editing)?;
            if let Some(f) = &import {
                let data = std::fs::read(f).with_context(|| t!("cannot read {}", f.display()))?;
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
                    println!("{}", t!("saved {}", f.display()));
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
                let rows = [
                    (
                        t!("disk signature"),
                        format!(
                            "{:08X}",
                            u32::from_le_bytes(s[440..444].try_into().unwrap())
                        ),
                    ),
                    (
                        t!("boot code"),
                        if s[..440].iter().any(|&b| b != 0) {
                            t!("present")
                        } else {
                            t!("none")
                        },
                    ),
                ];
                for (k, v) in rows {
                    println!("{}{v}", pad(&k, 16));
                }
                print_layout(image.layout());
            } else {
                let b = image.boot_sector_info()?;
                if json {
                    return print_json(&b);
                }
                let rows: [(String, String); 19] = [
                    (t!("jump"), b.jump),
                    (t!("oem name"), format!("{:?}", b.oem_name)),
                    (t!("bytes/sector"), b.bytes_per_sector.to_string()),
                    (t!("sectors/cluster"), b.sectors_per_cluster.to_string()),
                    (t!("reserved sectors"), b.reserved_sectors.to_string()),
                    (t!("FATs"), b.fats.to_string()),
                    (t!("root entries"), b.root_entries.to_string()),
                    (t!("total sectors"), b.total_sectors.to_string()),
                    (t!("media"), b.media),
                    (t!("sectors/FAT"), b.sectors_per_fat.to_string()),
                    (t!("sectors/track"), b.sectors_per_track.to_string()),
                    (t!("heads"), b.heads.to_string()),
                    (t!("hidden sectors"), b.hidden_sectors.to_string()),
                    (t!("drive number"), b.drive_number),
                    (t!("serial"), b.volume_id.unwrap_or_else(|| "-".into())),
                    (t!("label"), b.label.unwrap_or_else(|| "-".into())),
                    (t!("fs type field"), b.fs_type.unwrap_or_else(|| "-".into())),
                    (t!("file system"), b.fat_type),
                    (
                        t!("boot code"),
                        format!(
                            "{}{}",
                            if b.has_boot_code {
                                t!("present")
                            } else {
                                t!("none")
                            },
                            if b.signature_55aa {
                                String::new()
                            } else {
                                t!(", no 55AA signature")
                            }
                        ),
                    ),
                ];
                for (k, v) in rows {
                    println!("{}{v}", pad(&k, 18));
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
            let w = 14;
            println!("{}{} / {}", pad(&t!("files/folders"), w), r.files, r.dirs);
            println!(
                "{}{}",
                pad(&t!("fragmented"), w),
                t!("{} ({} extents)", r.fragmented, r.fragments)
            );
            println!(
                "{}{}",
                pad(&t!("clusters"), w),
                t!(
                    "{} used, {} free, {} lost ({} bytes each)",
                    r.used_clusters,
                    r.free_clusters,
                    r.lost_clusters,
                    r.cluster_size
                )
            );
            if !analyze {
                println!(
                    "{}{}",
                    pad(&t!("moved"), w),
                    t!("{} clusters", r.moved_clusters)
                );
            }
        }
        Cmd::Resize {
            img,
            spec,
            keep_partitions,
        } => {
            let partitioned = matches!(
                sectorzero_core::probe(&img.image)
                    .with_context(|| t!("cannot open {}", img.image.display()))?
                    .1,
                Layout::Mbr(_) | Layout::Gpt(_)
            );
            if partitioned {
                let size = match (
                    &spec.size,
                    &spec.format,
                    spec.like.is_some() || spec.geo.any(),
                ) {
                    (Some(s), None, false) => parse_size(s)?,
                    _ => bail!(t!(
                        "{} is a partitioned disk: give its new size with --size",
                        img.image.display()
                    )),
                };
                let r = Volume::resize_disk(&img.image, &img.opts(true), size, !keep_partitions)
                    .with_context(|| t!("cannot resize {}", img.image.display()))?;
                if json {
                    return print_json(&r);
                }
                print!(
                    "{}",
                    t!(
                        "resized the disk from {} to {} bytes",
                        r.old_size,
                        r.new_size
                    )
                );
                match (&r.partition, &r.stats) {
                    (Some(p), Some(s)) => println!(
                        "{}",
                        t!(
                            "; partition {} from {} to {} bytes, rebuilt with {} file(s), {} folder(s)",
                            p.index,
                            p.old_bytes,
                            p.new_bytes,
                            s.files,
                            s.dirs
                        )
                    ),
                    _ => println!("{}", t!("; partitions unchanged")),
                }
                return Ok(());
            }
            if keep_partitions {
                bail!(t!(
                    "--keep-partitions only applies to partitioned (MBR/GPT) disks"
                ));
            }
            let s = Volume::change_format(&img.image, &img.opts(true), spec.spec()?)
                .with_context(|| t!("cannot change the format of {}", img.image.display()))?;
            let info = open(&img, false)?.info()?;
            if json {
                return print_json(&info);
            }
            println!(
                "{}",
                t!(
                    "rebuilt as {} with {} file(s), {} folder(s); {} bytes free",
                    info.floppy_format
                        .or(info.media_format)
                        .unwrap_or(&info.fs_type),
                    s.files,
                    s.dirs,
                    info.free_bytes
                )
            );
        }
        Cmd::Convert {
            source,
            dest,
            container,
            fixed,
            member,
            encrypt,
            force,
        } => {
            if let Some(pw) = password_arg(encrypt)? {
                sectorzero_core::set_password(&dest, Some(&pw));
            }
            let mut bar = Progress::new(json);
            let r = convert_with_progress(
                &source,
                member.as_deref(),
                &dest,
                container.map(Into::into),
                !fixed,
                force,
                &mut |done, total| bar.show(&t!("copying"), done, total),
            )
            .with_context(|| t!("cannot convert {} to {}", source.display(), dest.display()));
            bar.done();
            let r = r?;
            if json {
                return print_json(&r);
            }
            println!(
                "{}",
                t!(
                    "wrote {} ({} {}, {} bytes, {} stored)",
                    dest.display(),
                    tr(r.container.kind.name()),
                    r.container.variant,
                    r.bytes,
                    r.stored
                )
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
                .with_context(|| t!("cannot build {}", output.display()))?;
            if json {
                return print_json(&r);
            }
            if r.bootable {
                println!(
                    "{}",
                    t!(
                        "built {} ({} bytes): {} file(s), {} folder(s), bootable",
                        output.display(),
                        r.image_size,
                        r.files,
                        r.dirs
                    )
                );
            } else {
                println!(
                    "{}",
                    t!(
                        "built {} ({} bytes): {} file(s), {} folder(s)",
                        output.display(),
                        r.image_size,
                        r.files,
                        r.dirs
                    )
                );
            }
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
                        other => bail!(t!(
                            "unknown attribute {} (use R, H, S or A)",
                            format!("{:?}", other)
                        )),
                    };
                    if slot.is_some_and(|v| v != on) {
                        bail!(t!("attribute {} is both set and cleared", c));
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
                        .with_context(|| t!("{}: not found (or the root folder)", p))?;
                    shown.push(e);
                }
                if json {
                    return print_json(&shown);
                }
                for e in shown {
                    println!(
                        "{}",
                        t!(
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
                        )
                    );
                }
                return Ok(());
            }
            let image = open_image(&img, true)?;
            let mut changed = 0;
            for p in &paths {
                changed += image
                    .edit_entry(p, &edit, recursive)
                    .with_context(|| t!("cannot change {}", p))?;
            }
            if json {
                return print_json(&serde_json::json!({ "changed": changed }));
            }
            if changed == 1 {
                println!("{}", t!("changed 1 entry"));
            } else {
                println!("{}", t!("changed {} entries", changed));
            }
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
                (Some(p), _) => t!("file /{}", p.trim_start_matches(['/', '\\'])),
                (None, true) => t!("image file as stored"),
                (None, false) => t!("virtual disk"),
            };
            print_checksums(&what, &sums);
        }
        Cmd::Diff {
            parent,
            child,
            force,
        } => {
            let info = create_differencing(&child, &parent, force)
                .with_context(|| t!("cannot create {}", child.display()))?;
            if json {
                return print_json(&info);
            }
            println!(
                "{}",
                t!(
                    "created {} ({} bytes) over {}",
                    child.display(),
                    info.virtual_size,
                    parent.display()
                )
            );
        }
        Cmd::Truncate { image } => {
            let r = truncate_image(&image)
                .with_context(|| t!("cannot truncate {}", image.display()))?;
            if json {
                return print_json(&r);
            }
            if r.new_size < r.old_size {
                println!(
                    "{}",
                    t!(
                        "truncated {} from {} to {} bytes",
                        image.display(),
                        r.old_size,
                        r.new_size
                    )
                );
            } else {
                println!(
                    "{}",
                    t!("{} has no unused space at the end", image.display())
                );
            }
        }
        Cmd::IsoReplace { img, path, file } => {
            let data =
                std::fs::read(&file).with_context(|| t!("cannot read {}", file.display()))?;
            open_image(&img, true)?
                .iso_replace_file(&path, &data)
                .with_context(|| t!("cannot replace {}", path))?;
            if json {
                return print_json(&serde_json::json!({ "path": path, "bytes": data.len() }));
            }
            println!("{}", t!("replaced {} ({} bytes)", path, data.len()));
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
            println!("{}", t!("wrote {} ({} bytes)", out.display(), size));
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
                "{}",
                t!(
                    "saved boot image {} to {} ({} bytes)",
                    entry,
                    out.display(),
                    data.len()
                )
            );
        }
    }
    Ok(())
}

fn print_layout(layout: &Layout) {
    match layout {
        Layout::BareVolume => row(&t!("layout"), t!("bare volume (no partition table)")),
        Layout::Iso => row(&t!("layout"), t!("ISO 9660 CD/DVD image")),
        Layout::Unknown => row(&t!("layout"), t!("unknown")),
        Layout::Mbr(parts) | Layout::Gpt(parts) | Layout::Apm(parts) => {
            row(
                &t!("layout"),
                match layout {
                    Layout::Gpt(_) => "GPT".to_string(),
                    Layout::Apm(_) => t!("Apple partition map"),
                    _ => "MBR".to_string(),
                },
            );
            for p in parts {
                println!(
                    "  {:<4}  {:<6} {:<18} {} {:>10}  {:>14}  {}{}{}",
                    p.index,
                    if p.scheme == "mbr" {
                        p.type_id.as_str()
                    } else {
                        ""
                    },
                    tr(&p.type_name),
                    t!("start"),
                    p.start_lba,
                    t!("{} bytes", p.size_bytes()),
                    p.fs.unwrap_or("-"),
                    if p.bootable {
                        t!("  boot")
                    } else {
                        String::new()
                    },
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
        row(&t!("opened"), t!("partition {}", p.index));
    }
    row(
        &t!("file system"),
        if i.fs_writable {
            i.fs_type.clone()
        } else {
            t!("{} (read-only)", i.fs_type)
        },
    );
    if let Some(why) = &i.read_only_reason {
        row(&t!("read-only"), tr(why));
    }
    if let Some(f) = i.floppy_format.or(i.media_format) {
        row(&t!("format"), tr(f));
    }
    row(
        &t!("label"),
        if i.label.is_empty() {
            t!("(none)")
        } else {
            i.label.clone()
        },
    );
    if !i.volume_id.is_empty() {
        let what = if i.fs_type.starts_with("ext") {
            t!("uuid")
        } else {
            t!("serial")
        };
        row(&what, &i.volume_id);
    }
    if !i.oem_name.is_empty() {
        let what = if i.iso.is_some() {
            t!("system")
        } else {
            t!("oem")
        };
        row(&what, &i.oem_name);
    }
    row(&t!("sector size"), i.bytes_per_sector);
    row(&t!("cluster size"), i.cluster_size);
    row(
        &t!("capacity"),
        t!("{} bytes ({} clusters)", i.total_bytes, i.total_clusters),
    );
    if i.iso.is_none() {
        row(
            &t!("free"),
            t!("{} bytes ({} clusters)", i.free_bytes, i.free_clusters),
        );
    }
    if let Some(iso) = &i.iso {
        if !iso.application.is_empty() {
            row(&t!("application"), &iso.application);
        }
        if !iso.created.is_empty() {
            row(&t!("created"), &iso.created);
        }
        for b in &iso.boot_entries {
            let what = t!(
                "{} {}, {} bytes at block {}",
                tr(&b.platform),
                tr(&b.media),
                b.image_size,
                b.lba
            );
            row(
                &t!("boot {}", b.index),
                if b.bootable {
                    what
                } else {
                    t!("{} (not bootable)", what)
                },
            );
        }
    }
}

/// One `label  value` line of `s0 info`, the values lined up in a column.
fn row(label: &str, value: impl std::fmt::Display) {
    println!("{}{value}", pad(label, label_width()));
}

/// The label column of `info`: wide enough for the longest label in the current language.
fn label_width() -> usize {
    static WIDTH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *WIDTH.get_or_init(|| {
        let labels = [
            t!("image"),
            t!("container"),
            t!("member"),
            t!("comment"),
            t!("encryption"),
            t!("volume"),
            t!("layout"),
            t!("opened"),
            t!("file system"),
            t!("read-only"),
            t!("format"),
            t!("label"),
            t!("uuid"),
            t!("serial"),
            t!("system"),
            t!("oem"),
            t!("sector size"),
            t!("cluster size"),
            t!("capacity"),
            t!("free"),
            t!("application"),
            t!("created"),
        ];
        let longest = labels.iter().map(|l| columns(l)).max().unwrap_or(0);
        (longest + 1).clamp(13, 24)
    })
}

/// Terminal columns `s` takes (East Asian wide characters count twice).
fn columns(s: &str) -> usize {
    let wide = |c: char| {
        matches!(c as u32, 0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6)
    };
    s.chars().map(|c| if wide(c) { 2 } else { 1 }).sum()
}

/// `label` and spaces up to `width` columns (at least one space).
fn pad(label: &str, width: usize) -> String {
    format!(
        "{label}{}",
        " ".repeat(width.saturating_sub(columns(label)).max(1))
    )
}

/// An error and its causes, each translated, as `outer: inner: …`.
fn error_text(e: &anyhow::Error) -> String {
    e.chain()
        .map(|c| tr(&c.to_string()))
        .collect::<Vec<_>>()
        .join(": ")
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
    println!(
        "{}",
        t!("{} file(s), {} bytes; {} folder(s)", files, bytes, dirs)
    );
}

fn print_checksums(what: &str, c: &Checksums) {
    println!("{}", t!("{}, {} bytes", what, c.bytes));
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

enum Verb {
    Extracted,
    Injected,
    Removed,
}

fn report(json: bool, verb: Verb, s: &Stats, dest: Option<&Path>) -> Result<()> {
    if json {
        return print_json(s);
    }
    let line = match (verb, dest) {
        (Verb::Extracted, Some(d)) => t!(
            "extracted {} file(s), {} folder(s), {} bytes to {}",
            s.files,
            s.dirs,
            s.bytes,
            d.display()
        ),
        (Verb::Extracted, None) => t!(
            "extracted {} file(s), {} folder(s), {} bytes",
            s.files,
            s.dirs,
            s.bytes
        ),
        (Verb::Injected, _) => t!(
            "injected {} file(s), {} folder(s), {} bytes",
            s.files,
            s.dirs,
            s.bytes
        ),
        (Verb::Removed, _) => t!(
            "removed {} file(s), {} folder(s), {} bytes",
            s.files,
            s.dirs,
            s.bytes
        ),
    };
    println!("{line}");
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
    r.map_err(|_| t!("{} is not a byte value like 0x80", format!("{:?}", s)))
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
        .with_context(|| t!("bad size {}", format!("{:?}", s)))?;
    if n <= 0.0 {
        bail!(t!("size must be positive"));
    }
    Ok((n * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key: engine messages and command line text from the sources, then the help.
    fn all_keys() -> Vec<(String, String)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut keys = sectorzero_i18n::extract::workspace_keys(&root);
        for h in help_keys() {
            let key = h.replace('{', "{{").replace('}', "}}");
            if !keys.iter().any(|(k, _)| *k == key) {
                keys.push((key, "s0 --help".into()));
            }
        }
        keys
    }

    /// Every catalog translates every key with the same placeholders and has no keys that
    /// are no longer used. The keys a catalog lacks are written to
    /// `target/i18n/missing-<lang>.json` for translators (all keys: `target/i18n/keys.json`).
    #[test]
    fn catalogs_are_complete() {
        let keys = all_keys();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/i18n");
        std::fs::create_dir_all(&dir).unwrap();
        let as_json = |list: &[&(String, String)]| {
            let map: serde_json::Map<String, serde_json::Value> = list
                .iter()
                .map(|(k, at)| (k.clone(), at.clone().into()))
                .collect();
            serde_json::to_string_pretty(&map).unwrap()
        };
        std::fs::write(
            dir.join("keys.json"),
            as_json(&keys.iter().collect::<Vec<_>>()),
        )
        .unwrap();
        let mut problems = Vec::new();
        for (lang, _) in &sectorzero_i18n::LANGUAGES[1..] {
            let have = sectorzero_i18n::catalog_keys(lang);
            let missing: Vec<_> = keys.iter().filter(|(k, _)| !have.contains(k)).collect();
            let file = dir.join(format!("missing-{lang}.json"));
            if missing.is_empty() {
                let _ = std::fs::remove_file(&file);
            } else {
                std::fs::write(&file, as_json(&missing)).unwrap();
                problems.push(format!("{lang}: {} keys missing", missing.len()));
            }
            for k in &have {
                if !keys.iter().any(|(key, _)| key == k) {
                    problems.push(format!("{lang}: unused key {k:?}"));
                }
            }
            for k in &have {
                let translation = sectorzero_i18n::translation(lang, k).unwrap_or_default();
                if let Some(e) = sectorzero_i18n::check_translation(k, &translation) {
                    problems.push(format!("{lang}: {k:?}: {e}"));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
