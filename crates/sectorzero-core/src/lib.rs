//! sectorzero-core: the disk image engine behind the `s0` CLI and the desktop app.
//!
//! Current scope: raw sector images (.ima/.img/.vfd/.flp/.dsk), either a bare FAT
//! volume ("superfloppy") or an MBR-partitioned disk, with FAT12/16/32 read and write.

mod disk;
mod error;
mod format;
mod volume;

pub use disk::{Layout, PartitionInfo, probe};
pub use error::{Error, Result};
pub use format::{FLOPPY_FORMATS, FloppyFormat, NewImage};
pub use volume::{Attributes, Entry, OpenOptions, Stats, Volume, VolumeInfo};
