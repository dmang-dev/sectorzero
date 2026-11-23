//! sectorzero-core: the disk image engine behind the `s0` CLI and the desktop app.
//!
//! Layers, bottom up:
//! - `container`: the file format holding the virtual disk (raw, IMZ/WLZ, VHD, VHDX, VMDK);
//! - `partition`: the layout of the disk (bare volume, ISO 9660, MBR, GPT);
//! - `window`: one volume's byte range as a stream;
//! - file systems: FAT12/16/32 (read/write, via fatfs, plus `fatraw` for label, boot
//!   sector and defragmentation) and ISO 9660/Joliet/Rock Ridge (read, and `iso_build`).

mod container;
mod error;
mod fatraw;
mod format;
mod image;
mod iso;
mod iso_build;
mod partition;
mod util;
mod volume;
mod window;

pub use container::{ContainerInfo, ContainerKind};
pub use error::{Error, Result};
pub use fatraw::{BootSectorEdit, BootSectorInfo, DefragReport};
pub use format::{FLOPPY_FORMATS, FloppyFormat, NewImage};
pub use image::{ConvertReport, Image, OpenOptions, convert};
pub use iso::{BootEntry, IsoInfo};
pub use iso_build::{BootPlatform, IsoBuildOptions, IsoBuildReport, build_iso};
pub use partition::{Layout, PartitionInfo, PartitionScheme};
pub use volume::{Attributes, CreateOptions, Entry, Stats, Volume, VolumeInfo};

/// Inspect an image's container and layout without mounting anything.
pub fn probe(path: &std::path::Path) -> Result<(ContainerInfo, Layout)> {
    let mut disk = container::open(path, false, None)?;
    let layout = partition::detect(disk.as_mut())?;
    Ok((disk.info(), layout))
}
