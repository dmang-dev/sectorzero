//! Floppy disk geometries and the "new image" specification.

use serde::Serialize;

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

/// What to create with [`crate::Volume::create`].
#[derive(Debug, Clone, Copy)]
pub enum NewImage {
    Floppy(&'static FloppyFormat),
    /// A bare FAT volume of this many bytes, with no partition table: WinImage's
    /// "hard disk / removable" image. FAT type and cluster size are picked automatically.
    Volume {
        size_bytes: u64,
    },
}

impl NewImage {
    pub fn size_bytes(&self) -> u64 {
        match self {
            NewImage::Floppy(f) => f.size_bytes(),
            NewImage::Volume { size_bytes } => size_bytes / 512 * 512,
        }
    }
}
