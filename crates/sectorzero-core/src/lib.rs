//! sectorzero-core: the disk image engine behind the `s0` CLI and the desktop app.
//!
//! Layers, bottom up:
//! - `container`: the file format holding the virtual disk (raw, IMZ/WLZ, VHD, VHDX, VMDK),
//!   or a physical drive (`device`);
//! - `partition`: the layout of the disk (bare volume, ISO 9660, MBR, GPT);
//! - `window`: one volume's byte range as a stream;
//! - file systems: FAT12/16/32 (read/write, via fatfs, plus `fatraw` for label, boot
//!   sector and defragmentation) and ISO 9660/Joliet/Rock Ridge (read, and `iso_build`).

mod apfs;
mod checksum;
mod container;
mod device;
mod error;
mod exfat;
mod ext;
mod fatraw;
mod format;
mod hfs;
mod image;
mod iso;
mod iso_build;
mod listing;
mod ntfsfs;
mod partition;
mod util;
mod volume;
mod window;

pub use apfs::{ApfsInfo, ApfsVolume};
pub use checksum::{Checksums, checksum_file};
pub use container::{
    ContainerInfo, ContainerKind, create_differencing, set_fallback_password, set_password,
};
pub use device::{
    DriveInfo, DriveWipeOptions, DriveWriteOptions, DriveWriteReport, WipePattern, drive_info,
    eject_drive, is_device_path, list_drives, wipe_drive, write_image_to_drive,
};
pub use error::{Error, Result};
pub use fatraw::{BootSectorEdit, BootSectorInfo, DefragReport, EntryEdit};
pub use format::{
    FLOPPY_FORMATS, FloppyFormat, Geometry, MEDIA_FORMATS, MediaFormat, MediaLayout, NewImage,
};
pub use image::{
    ConvertReport, Image, OpenOptions, TruncateReport, checksum_virtual_disk, convert,
    convert_with_progress, truncate_image,
};
pub use iso::{BootEntry, IsoInfo};
pub use iso_build::{BootPlatform, IsoBuildOptions, IsoBuildReport, build_iso};
pub use listing::{ListingFormat, render_listing};
pub use partition::{Layout, PartitionInfo, PartitionScheme};
pub use volume::{
    Attributes, CreateOptions, DiskResize, Entry, ForkMode, FsKind, MacInfo, PartitionResize,
    Stats, Volume, VolumeInfo,
};

/// Inspect an image's container and layout without mounting anything.
pub fn probe(path: &std::path::Path) -> Result<(ContainerInfo, Layout)> {
    let mut disk = container::open(path, false, None)?;
    let layout = partition::detect(disk.as_mut())?;
    Ok((disk.info(), layout))
}
