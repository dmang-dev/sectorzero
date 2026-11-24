//! NTFS, read-only, through the `ntfs` crate (MIT OR Apache-2.0): folders, file data
//! (resident, non-resident, sparse, attribute lists), names, attributes and timestamps.
//! Compressed and encrypted files are reported as unsupported rather than read wrongly.

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::{NtfsFileAttributeFlags, NtfsFileName, NtfsFileNamespace};
use ntfs::{Ntfs, NtfsAttributeFlags, NtfsError, NtfsFile, NtfsTime};

use crate::error::{Error, Result, display};
use crate::volume::{Attributes, Entry, Stats, host_safe_name, join, split_parent};
use crate::window::Window;

/// File records below this number are NTFS metadata ($MFT, $Bitmap, …) or reserved.
const FIRST_USER_RECORD: u64 = 24;
const ROOT_RECORD: u64 = 5;
const BITMAP_RECORD: u64 = 6;

fn err(e: NtfsError) -> Error {
    Error::Invalid(format!("NTFS: {e}"))
}

pub(crate) struct NtfsInfo {
    pub label: String,
    pub serial: u64,
    pub cluster_size: u32,
    pub sector_size: u16,
    pub size: u64,
    pub free_clusters: Option<u64>,
}

pub(crate) struct NtfsFs {
    w: RefCell<Window>,
    ntfs: Ntfs,
}

struct Found {
    record: u64,
    name: String,
    is_dir: bool,
    size: u64,
    attrs: NtfsFileAttributeFlags,
    created: NtfsTime,
    modified: NtfsTime,
    accessed: NtfsTime,
}

impl Found {
    fn of(record: u64, key: &NtfsFileName) -> Found {
        Found {
            record,
            name: key.name().to_string_lossy(),
            is_dir: key.is_directory(),
            size: key.data_size(),
            attrs: key.file_attributes(),
            created: key.creation_time(),
            modified: key.modification_time(),
            accessed: key.access_time(),
        }
    }

    fn entry(&self, parent: &str) -> Entry {
        let a = self.attrs;
        Entry {
            path: join(parent, &self.name),
            short_name: self.name.clone(),
            is_dir: self.is_dir,
            size: if self.is_dir { 0 } else { self.size },
            modified: format_nt(self.modified, true),
            created: format_nt(self.created, true),
            accessed: format_nt(self.accessed, false),
            attributes: Attributes {
                read_only: a.contains(NtfsFileAttributeFlags::READ_ONLY),
                hidden: a.contains(NtfsFileAttributeFlags::HIDDEN),
                system: a.contains(NtfsFileAttributeFlags::SYSTEM),
                archive: a.contains(NtfsFileAttributeFlags::ARCHIVE),
            },
            name: self.name.clone(),
        }
    }
}

/// Seconds since 1970 of an NTFS timestamp (100 ns units since 1601).
fn nt_secs(t: NtfsTime) -> i64 {
    (t.nt_timestamp() / 10_000_000) as i64 - 11_644_473_600
}

fn format_nt(t: NtfsTime, with_time: bool) -> String {
    if t.nt_timestamp() == 0 {
        return String::new();
    }
    match Local.timestamp_opt(nt_secs(t), 0).single() {
        Some(t) if with_time => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        Some(t) => t.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

impl NtfsFs {
    pub(crate) fn open(mut w: Window) -> Result<NtfsFs> {
        let mut ntfs = Ntfs::new(&mut w).map_err(err)?;
        ntfs.read_upcase_table(&mut w).map_err(err)?;
        Ok(NtfsFs {
            w: RefCell::new(w),
            ntfs,
        })
    }

    pub(crate) fn info(&self) -> NtfsInfo {
        let w = &mut *self.w.borrow_mut();
        let label = match self.ntfs.volume_name(w) {
            Some(Ok(n)) => n.name().to_string_lossy(),
            _ => String::new(),
        };
        let free_clusters = self.count_free(w).ok();
        NtfsInfo {
            label,
            serial: self.ntfs.serial_number(),
            cluster_size: self.ntfs.cluster_size(),
            sector_size: self.ntfs.sector_size(),
            size: self.ntfs.size(),
            free_clusters,
        }
    }

    /// Free clusters, counted from $Bitmap.
    fn count_free(&self, w: &mut Window) -> Result<u64> {
        let file = self.ntfs.file(w, BITMAP_RECORD).map_err(err)?;
        let mut bitmap = Vec::new();
        read_data(&file, w, &mut bitmap)?;
        let clusters = self.ntfs.size() / u64::from(self.ntfs.cluster_size());
        let mut used = 0u64;
        for (i, &b) in bitmap.iter().enumerate() {
            let base = i as u64 * 8;
            if base >= clusters {
                break;
            }
            let valid = (clusters - base).min(8) as u32;
            let mask = if valid == 8 {
                0xFF
            } else {
                (1u16 << valid) as u8 - 1
            };
            used += u64::from((b & mask).count_ones());
        }
        Ok(clusters - used)
    }

    pub(crate) fn label(&self) -> String {
        let w = &mut *self.w.borrow_mut();
        match self.ntfs.volume_name(w) {
            Some(Ok(n)) => n.name().to_string_lossy(),
            _ => String::new(),
        }
    }

    /// The entry at `path` ("" is the root, which has no entry).
    fn lookup(&self, w: &mut Window, path: &str) -> Result<Option<(u64, Option<Found>)>> {
        if path.is_empty() {
            return Ok(Some((ROOT_RECORD, None)));
        }
        let mut record = ROOT_RECORD;
        let mut found = None;
        for name in path.split('/') {
            let file = self.ntfs.file(w, record).map_err(err)?;
            if !file.is_directory() {
                return Ok(None);
            }
            let index = file.directory_index(w).map_err(err)?;
            let mut finder = index.finder();
            let Some(entry) = NtfsFileNameIndex::find(&mut finder, &self.ntfs, w, name) else {
                return Ok(None);
            };
            let entry = entry.map_err(err)?;
            let r = entry.file_reference().file_record_number();
            let key = match entry.key() {
                Some(k) => k.map_err(err)?,
                None => return Ok(None),
            };
            record = r;
            found = Some(Found::of(r, &key));
        }
        Ok(Some((record, found)))
    }

    fn children(&self, w: &mut Window, record: u64) -> Result<Vec<Found>> {
        let file = self.ntfs.file(w, record).map_err(err)?;
        if !file.is_directory() {
            return Err(Error::Invalid("not a folder".into()));
        }
        let index = file.directory_index(w).map_err(err)?;
        let mut entries = index.entries();
        let mut out = Vec::new();
        while let Some(e) = entries.next(w) {
            let e = e.map_err(err)?;
            let r = e.file_reference().file_record_number();
            let Some(key) = e.key() else { continue };
            let key = key.map_err(err)?;
            if r < FIRST_USER_RECORD || key.namespace() == NtfsFileNamespace::Dos {
                continue;
            }
            out.push(Found::of(r, &key));
        }
        Ok(out)
    }

    pub(crate) fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let w = &mut *self.w.borrow_mut();
        let (record, found) = self
            .lookup(w, dir)?
            .ok_or_else(|| Error::NotFound(display(dir)))?;
        if found.as_ref().is_some_and(|f| !f.is_dir) {
            return Err(Error::Invalid(format!("not a folder: {}", display(dir))));
        }
        let mut out: Vec<Entry> = self
            .children(w, record)?
            .iter()
            .map(|f| f.entry(dir))
            .collect();
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    pub(crate) fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let w = &mut *self.w.borrow_mut();
        let (parent, _) = split_parent(path);
        Ok(self
            .lookup(w, path)?
            .and_then(|(_, f)| f)
            .map(|f| f.entry(parent)))
    }

    pub(crate) fn copy_out(&self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let w = &mut *self.w.borrow_mut();
        let (record, found) = self
            .lookup(w, path)?
            .ok_or_else(|| Error::NotFound(display(path)))?;
        if found.as_ref().is_none_or(|f| f.is_dir) {
            return Err(Error::Invalid(format!("{} is a folder", display(path))));
        }
        let file = self.ntfs.file(w, record).map_err(err)?;
        read_data(&file, w, out)
    }

    pub(crate) fn extract(&self, src: &str, dest: &Path, overwrite: bool) -> Result<Stats> {
        let w = &mut *self.w.borrow_mut();
        fs::create_dir_all(dest)?;
        let mut stats = Stats::default();
        let (record, found) = self
            .lookup(w, src)?
            .ok_or_else(|| Error::NotFound(display(src)))?;
        match found {
            None => self.extract_dir(w, record, dest, overwrite, &mut stats)?,
            Some(f) => self.extract_one(w, &f, dest, overwrite, &mut stats)?,
        }
        Ok(stats)
    }

    fn extract_dir(
        &self,
        w: &mut Window,
        record: u64,
        dest: &Path,
        overwrite: bool,
        stats: &mut Stats,
    ) -> Result<()> {
        for f in self.children(w, record)? {
            self.extract_one(w, &f, dest, overwrite, stats)?;
        }
        Ok(())
    }

    fn extract_one(
        &self,
        w: &mut Window,
        f: &Found,
        dest: &Path,
        overwrite: bool,
        stats: &mut Stats,
    ) -> Result<()> {
        let target = dest.join(host_safe_name(&f.name));
        if f.is_dir {
            fs::create_dir_all(&target)?;
            stats.dirs += 1;
            return self.extract_dir(w, f.record, &target, overwrite, stats);
        }
        if !overwrite && target.exists() {
            return Err(Error::Exists(target.display().to_string()));
        }
        let file = self.ntfs.file(w, f.record).map_err(err)?;
        let mut out = File::create(&target)?;
        stats.bytes += read_data(&file, w, &mut out)?;
        stats.files += 1;
        let secs = nt_secs(f.modified);
        if secs > 0 {
            let _ = out.set_modified(UNIX_EPOCH + Duration::from_secs(secs as u64));
        }
        Ok(())
    }
}

/// Copy the unnamed $DATA stream of a file to `out`.
fn read_data(file: &NtfsFile<'_>, w: &mut Window, out: &mut dyn Write) -> Result<u64> {
    let Some(item) = file.data(w, "") else {
        return Ok(0); // no data stream (a reparse point, for example)
    };
    let item = item.map_err(err)?;
    let attr = item.to_attribute().map_err(err)?;
    let flags = attr.flags();
    if flags.contains(NtfsAttributeFlags::COMPRESSED) {
        return Err(Error::Unsupported("NTFS-compressed files".into()));
    }
    if flags.contains(NtfsAttributeFlags::ENCRYPTED) {
        return Err(Error::Unsupported("EFS-encrypted NTFS files".into()));
    }
    let value = attr.value(w).map_err(err)?;
    let len = value.len();
    let mut r = value.attach(w).take(len);
    Ok(io::copy(&mut r, out)?)
}
