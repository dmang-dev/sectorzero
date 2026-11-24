//! Floppy disk geometries and the "new image" specification.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::util::{le16, le32};

/// A standard PC floppy geometry with the FAT parameters DOS/Windows use for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FloppyFormat {
    /// Short id used on the command line (`s0 new -f 1440`).
    pub id: &'static str,
    pub name: &'static str,
    pub total_sectors: u32,
    pub sectors_per_track: u16,
    pub heads: u16,
    pub bytes_per_cluster: u32,
    pub root_entries: u16,
    pub media: u8,
}

impl FloppyFormat {
    pub fn size_bytes(&self) -> u64 {
        u64::from(self.total_sectors) * 512
    }
}

#[allow(clippy::too_many_arguments)] // one row of the table below
const fn fmt(
    id: &'static str,
    name: &'static str,
    total_sectors: u32,
    heads: u16,
    sectors_per_track: u16,
    bytes_per_cluster: u32,
    root_entries: u16,
    media: u8,
) -> FloppyFormat {
    FloppyFormat {
        id,
        name,
        total_sectors,
        sectors_per_track,
        heads,
        bytes_per_cluster,
        root_entries,
        media,
    }
}

pub const FLOPPY_FORMATS: &[FloppyFormat] = &[
    fmt("160", "160 KB 5.25\" SS/DD", 320, 1, 8, 512, 64, 0xFE),
    fmt("180", "180 KB 5.25\" SS/DD", 360, 1, 9, 512, 64, 0xFC),
    fmt("320", "320 KB 5.25\" DS/DD", 640, 2, 8, 1024, 112, 0xFF),
    fmt("360", "360 KB 5.25\" DS/DD", 720, 2, 9, 1024, 112, 0xFD),
    fmt("720", "720 KB 3.5\" DS/DD", 1440, 2, 9, 1024, 112, 0xF9),
    fmt("1200", "1.2 MB 5.25\" DS/HD", 2400, 2, 15, 512, 224, 0xF9),
    fmt("1440", "1.44 MB 3.5\" DS/HD", 2880, 2, 18, 512, 224, 0xF0),
    fmt(
        "1680",
        "1.68 MB 3.5\" (21 sectors/track)",
        3360,
        2,
        21,
        512,
        224,
        0xF0,
    ),
    fmt(
        "dmf1024",
        "1.68 MB DMF, 1024-byte clusters",
        3360,
        2,
        21,
        1024,
        16,
        0xF0,
    ),
    fmt(
        "dmf2048",
        "1.68 MB DMF, 2048-byte clusters",
        3360,
        2,
        21,
        2048,
        16,
        0xF0,
    ),
    fmt(
        "1720",
        "1.72 MB 3.5\" (82 tracks)",
        3444,
        2,
        21,
        512,
        224,
        0xF0,
    ),
    fmt("2880", "2.88 MB 3.5\" DS/ED", 5760, 2, 36, 1024, 240, 0xF0),
];

impl FloppyFormat {
    /// Look up a format by id, accepting a few spellings (`1440`, `144`, `1.44`, `1.44m`, `DMF2048`).
    pub fn find(query: &str) -> Option<&'static FloppyFormat> {
        let q = query.trim().to_ascii_lowercase();
        let q = q.trim_end_matches(['k', 'b', 'm']).replace('.', "");
        let alias = match q.as_str() {
            "12" | "120" => "1200",
            "144" => "1440",
            "168" => "1680",
            "172" => "1720",
            "288" => "2880",
            "dmf" => "dmf2048",
            other => other,
        };
        FLOPPY_FORMATS.iter().find(|f| f.id == alias)
    }

    /// The preset matching a boot sector's parameters, if any. Cluster size and root
    /// directory size are compared too: 1.68 MB and the two DMF variants share a geometry.
    pub fn matching(
        total_sectors: u32,
        sectors_per_track: u16,
        heads: u16,
        media: u8,
        bytes_per_cluster: u32,
        root_entries: u16,
    ) -> Option<&'static FloppyFormat> {
        FLOPPY_FORMATS.iter().find(|f| {
            f.total_sectors == total_sectors
                && f.sectors_per_track == sectors_per_track
                && f.heads == heads
                && f.media == media
                && f.bytes_per_cluster == bytes_per_cluster
                && f.root_entries == root_entries
        })
    }
}

/// How a removable medium comes formatted for PCs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum MediaLayout {
    /// No partition table: the file system starts at sector 0 ("superfloppy").
    Superfloppy,
    /// An MBR holding one partition, in `slot` (1-4), from sector `start` to the end.
    Mbr { slot: u8, start: u32 },
}

/// A removable cartridge or disk (Zip, Jaz, SuperDisk, magneto-optical, …): its exact
/// capacity as the drive reports it, and how the vendor formatted it for PCs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MediaFormat {
    /// Short id used on the command line (`s0 new -m zip100`).
    pub id: &'static str,
    pub name: &'static str,
    pub total_sectors: u64,
    pub sector_size: u32,
    /// The CHS geometry the drive reports (used in the BPB and the MBR's CHS fields).
    pub heads: u16,
    pub sectors_per_track: u16,
    pub layout: MediaLayout,
    /// FAT type (12, 16 or 32); 0 picks it from the size.
    pub fat_bits: u8,
    /// FAT cluster size; 0 picks the default for the size.
    pub bytes_per_cluster: u32,
    /// Media descriptor byte.
    pub media: u8,
}

impl MediaFormat {
    pub fn size_bytes(&self) -> u64 {
        self.total_sectors * u64::from(self.sector_size)
    }

    /// Look up a preset by id, ignoring case, spaces, dashes and dots (`zip100`, `Zip-100`).
    pub fn find(query: &str) -> Option<&'static MediaFormat> {
        let norm = |s: &str| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase()
        };
        let q = norm(query);
        MEDIA_FORMATS.iter().find(|m| norm(m.id) == q)
    }

    /// The preset whose capacity is exactly `bytes`, if any.
    pub fn matching(bytes: u64) -> Option<&'static MediaFormat> {
        MEDIA_FORMATS.iter().find(|m| m.size_bytes() == bytes)
    }
}

const IOMEGA: MediaLayout = MediaLayout::Mbr { slot: 4, start: 32 };

/// Capacities are what the drives report (READ CAPACITY), from drive logs, manuals and
/// mailing-list `fdisk` output. Iomega's PC format puts one FAT16 partition in MBR slot 4
/// at sector 32 (slot 4 for Macintosh compatibility); SuperDisk and magneto-optical media
/// were formatted like floppies, without a partition table.
pub const MEDIA_FORMATS: &[MediaFormat] = &[
    MediaFormat {
        id: "zip100",
        name: "Iomega Zip 100",
        total_sectors: 196_608,
        sector_size: 512,
        heads: 64,
        sectors_per_track: 32,
        layout: IOMEGA,
        fat_bits: 16,
        bytes_per_cluster: 2048,
        media: 0xF8,
    },
    MediaFormat {
        id: "zip250",
        name: "Iomega Zip 250",
        total_sectors: 489_532,
        sector_size: 512,
        heads: 64,
        sectors_per_track: 32,
        layout: IOMEGA,
        fat_bits: 16,
        bytes_per_cluster: 4096,
        media: 0xF8,
    },
    MediaFormat {
        id: "jaz1g",
        name: "Iomega Jaz 1 GB",
        total_sectors: 2_091_050,
        sector_size: 512,
        heads: 64,
        sectors_per_track: 32,
        layout: IOMEGA,
        fat_bits: 16,
        bytes_per_cluster: 16384,
        media: 0xF8,
    },
    MediaFormat {
        id: "jaz2g",
        name: "Iomega Jaz 2 GB",
        total_sectors: 3_915_600,
        sector_size: 512,
        heads: 64,
        sectors_per_track: 32,
        layout: IOMEGA,
        fat_bits: 16,
        bytes_per_cluster: 32768,
        media: 0xF8,
    },
    MediaFormat {
        id: "ls120",
        name: "LS-120 SuperDisk 120 MB",
        total_sectors: 246_528,
        sector_size: 512,
        heads: 8,
        sectors_per_track: 32,
        layout: MediaLayout::Superfloppy,
        fat_bits: 16,
        bytes_per_cluster: 2048,
        media: 0xF0,
    },
    MediaFormat {
        id: "mo128",
        name: "3.5\" magneto-optical 128 MB",
        total_sectors: 248_826,
        sector_size: 512,
        heads: 64,
        sectors_per_track: 32,
        layout: MediaLayout::Superfloppy,
        fat_bits: 16,
        bytes_per_cluster: 2048,
        media: 0xF0,
    },
    MediaFormat {
        id: "mo230",
        name: "3.5\" magneto-optical 230 MB",
        total_sectors: 446_325,
        sector_size: 512,
        heads: 1,
        sectors_per_track: 25,
        layout: MediaLayout::Superfloppy,
        fat_bits: 16,
        bytes_per_cluster: 4096,
        media: 0xF0,
    },
    MediaFormat {
        id: "mo640",
        name: "3.5\" magneto-optical 640 MB",
        total_sectors: 310_352,
        sector_size: 2048,
        heads: 64,
        sectors_per_track: 32,
        layout: MediaLayout::Superfloppy,
        fat_bits: 16,
        bytes_per_cluster: 0,
        media: 0xF0,
    },
    MediaFormat {
        id: "mo1300",
        name: "3.5\" GigaMO 1.3 GB",
        total_sectors: 605_846,
        sector_size: 2048,
        heads: 64,
        sectors_per_track: 32,
        layout: MediaLayout::Superfloppy,
        fat_bits: 0,
        bytes_per_cluster: 0,
        media: 0xF0,
    },
];

/// A custom FAT geometry (WinImage's "custom format"). Sectors are 512 bytes. Zero in
/// `bytes_per_cluster`, `root_entries` or `fat_bits` means "pick automatically".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub total_sectors: u32,
    pub sectors_per_track: u16,
    pub heads: u16,
    pub bytes_per_cluster: u32,
    pub root_entries: u16,
    /// Number of FAT copies (1 or 2).
    pub fats: u8,
    pub media: u8,
    /// 12, 16, 32, or 0 for automatic.
    pub fat_bits: u8,
    pub hidden_sectors: u32,
    /// BIOS drive number: 0x00 for floppies, 0x80 for hard disks.
    pub drive_number: u8,
}

impl Geometry {
    pub fn size_bytes(&self) -> u64 {
        u64::from(self.total_sectors) * 512
    }

    /// The parameters of a standard floppy format, as a starting point for edits.
    pub fn from_floppy(f: &FloppyFormat) -> Geometry {
        Geometry {
            total_sectors: f.total_sectors,
            sectors_per_track: f.sectors_per_track,
            heads: f.heads,
            bytes_per_cluster: f.bytes_per_cluster,
            root_entries: f.root_entries,
            fats: 2,
            media: f.media,
            fat_bits: 12,
            hidden_sectors: 0,
            drive_number: 0x00,
        }
    }

    /// The geometry described by a FAT boot sector (from an image or a `.bin` file).
    pub fn from_boot_sector(s: &[u8]) -> Result<Geometry> {
        if s.len() < 512 {
            return Err(Error::Invalid("a boot sector is 512 bytes".into()));
        }
        let b = crate::fatraw::Bpb::parse(s)?;
        if b.bps != 512 {
            return Err(Error::Unsupported(format!(
                "{}-byte sectors (512 only)",
                b.bps
            )));
        }
        let total = u32::try_from(b.total_sectors)
            .map_err(|_| Error::Invalid("boot sector total sectors is too large".into()))?;
        Ok(Geometry {
            total_sectors: total,
            sectors_per_track: le16(s, 24),
            heads: le16(s, 26),
            bytes_per_cluster: b.cluster as u32,
            root_entries: le16(s, 17),
            fats: s[16],
            media: s[21],
            fat_bits: b.fat_bits,
            hidden_sectors: le32(s, 28),
            drive_number: s[if b.fat_bits == 32 { 64 } else { 36 }],
        })
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let bad = |m: &str| Err(Error::Invalid(format!("custom geometry: {m}")));
        if self.total_sectors < 64 {
            return bad("at least 64 sectors are needed");
        }
        if !matches!(self.fats, 1 | 2) {
            return bad("the number of FATs must be 1 or 2");
        }
        if !matches!(self.fat_bits, 0 | 12 | 16 | 32) {
            return bad("the FAT type must be 12, 16 or 32");
        }
        if self.bytes_per_cluster != 0
            && (!self.bytes_per_cluster.is_power_of_two()
                || !(512..=65536).contains(&self.bytes_per_cluster))
        {
            return bad("the cluster size must be a power of two from 512 to 65536 bytes");
        }
        if self.media < 0xF0 && self.media != 0 {
            return bad("the media descriptor must be 0xF0 to 0xFF");
        }
        if self.sectors_per_track == 0 || self.sectors_per_track > 63 || self.heads == 0 {
            return bad("sectors per track must be 1-63 and heads at least 1");
        }
        Ok(())
    }
}

/// What to create with [`crate::Volume::create`].
#[derive(Debug, Clone, Copy)]
pub enum NewImage {
    Floppy(&'static FloppyFormat),
    /// A bare FAT volume of this many bytes, with no partition table: WinImage's
    /// "hard disk / removable" image. FAT type and cluster size are picked automatically.
    Volume {
        size_bytes: u64,
    },
    /// A bare FAT volume with every BPB parameter chosen by the caller.
    Custom(Geometry),
    /// A removable medium preset, laid out the way its vendor formatted it.
    Media(&'static MediaFormat),
}

impl NewImage {
    pub fn size_bytes(&self) -> u64 {
        match self {
            NewImage::Floppy(f) => f.size_bytes(),
            NewImage::Volume { size_bytes } => size_bytes / 512 * 512,
            NewImage::Custom(g) => g.size_bytes(),
            NewImage::Media(m) => m.size_bytes(),
        }
    }
}
