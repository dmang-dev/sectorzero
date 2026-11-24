//! SectorZero desktop app: a thin Tauri shell over `sectorzero-core`.
//!
//! The app remembers which image is open; each command reopens it for the duration of
//! the call, so nothing holds the file open between actions.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sectorzero_core::{
    BootPlatform, BootSectorEdit, BootSectorInfo, Checksums, ContainerKind, ConvertReport,
    CreateOptions, DefragReport, DriveInfo, DriveWriteOptions, DriveWriteReport, Entry, EntryEdit,
    Error, FLOPPY_FORMATS, FloppyFormat, FsKind, Geometry, Image, IsoBuildOptions, IsoBuildReport,
    ListingFormat, NewImage, OpenOptions, PartitionScheme, Stats, TruncateReport, Volume,
    VolumeInfo, build_iso, checksum_file, convert, convert_with_progress, create_differencing,
    list_drives, render_listing, truncate_image, write_image_to_drive,
};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, State};

#[derive(Default)]
struct Current {
    path: Option<PathBuf>,
    partition: Option<usize>,
    member: Option<String>,
}

#[derive(Default)]
struct AppState(Mutex<Current>);

/// Errors sent to the UI: `kind` lets it react (e.g. offer to overwrite on `exists`).
#[derive(Debug, Serialize)]
struct CmdError {
    kind: &'static str,
    message: String,
}

impl From<Error> for CmdError {
    fn from(e: Error) -> Self {
        let kind = match &e {
            Error::NotFound(_) => "not_found",
            Error::Exists(_) => "exists",
            Error::InvalidPath(_) => "invalid_path",
            Error::Unsupported(_) => "unsupported",
            Error::NotFat(_) => "not_fat",
            Error::Invalid(_) => "invalid",
            Error::Io(_) => "io",
        };
        CmdError {
            kind,
            message: e.to_string(),
        }
    }
}

impl From<std::io::Error> for CmdError {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e).into()
    }
}

fn invalid(message: String) -> CmdError {
    CmdError {
        kind: "invalid",
        message,
    }
}

fn no_image() -> CmdError {
    CmdError {
        kind: "no_image",
        message: "No image is open.".into(),
    }
}

type CmdResult<T> = Result<T, CmdError>;

#[derive(Serialize)]
struct Opened {
    path: String,
    name: String,
    info: VolumeInfo,
}

fn opened(path: &Path, vol: &Volume) -> CmdResult<Opened> {
    Ok(Opened {
        path: path.display().to_string(),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        info: vol.info()?,
    })
}

fn current_opts(state: &State<AppState>, writable: bool) -> CmdResult<(PathBuf, OpenOptions)> {
    let cur = state.0.lock().unwrap();
    let path = cur.path.clone().ok_or_else(no_image)?;
    Ok((
        path,
        OpenOptions {
            writable,
            partition: cur.partition,
            member: cur.member.clone(),
        },
    ))
}

fn open_current(state: &State<AppState>, writable: bool) -> CmdResult<Volume> {
    let (path, opts) = current_opts(state, writable)?;
    Ok(Volume::open(&path, &opts)?)
}

fn image_current(state: &State<AppState>, writable: bool) -> CmdResult<Image> {
    let (path, opts) = current_opts(state, writable)?;
    Ok(Image::open(&path, &opts)?)
}

fn reopened(state: &State<AppState>) -> CmdResult<Opened> {
    let (path, opts) = current_opts(state, false)?;
    opened(&path, &Volume::open(&path, &opts)?)
}

fn remember(
    state: &State<AppState>,
    path: PathBuf,
    partition: Option<usize>,
    member: Option<String>,
) {
    *state.0.lock().unwrap() = Current {
        path: Some(path),
        partition,
        member,
    };
}

#[tauri::command]
fn formats() -> Vec<FloppyFormat> {
    FLOPPY_FORMATS.to_vec()
}

#[tauri::command]
fn media_formats() -> Vec<sectorzero_core::MediaFormat> {
    sectorzero_core::MEDIA_FORMATS.to_vec()
}

#[tauri::command(async)]
fn open_image(
    path: String,
    partition: Option<usize>,
    member: Option<String>,
    state: State<AppState>,
) -> CmdResult<Opened> {
    let path = PathBuf::from(path);
    let opts = OpenOptions {
        writable: false,
        partition,
        member: member.clone(),
    };
    let vol = Volume::open(&path, &opts)?;
    let out = opened(&path, &vol)?;
    remember(&state, path, partition, member);
    Ok(out)
}

fn spec_of(
    format: Option<&str>,
    size_mb: Option<f64>,
    geometry: Option<Geometry>,
) -> CmdResult<NewImage> {
    if let Some(g) = geometry {
        return Ok(NewImage::Custom(g));
    }
    match (format, size_mb) {
        (Some(f), _) => match (FloppyFormat::find(f), sectorzero_core::MediaFormat::find(f)) {
            (Some(f), _) => Ok(NewImage::Floppy(f)),
            (None, Some(m)) => Ok(NewImage::Media(m)),
            _ => Err(invalid(format!("Unknown format {f}"))),
        },
        (None, Some(mb)) if mb > 0.0 => Ok(NewImage::Volume {
            size_bytes: (mb * 1024.0 * 1024.0) as u64,
        }),
        _ => Err(invalid("Choose a floppy format or a disk size.".into())),
    }
}

fn container_of(name: Option<&str>) -> CmdResult<Option<ContainerKind>> {
    Ok(match name {
        None | Some("auto") => None,
        Some("raw") => Some(ContainerKind::Raw),
        Some("imz") => Some(ContainerKind::Imz),
        Some("vhd") => Some(ContainerKind::Vhd),
        Some("vhdx") => Some(ContainerKind::Vhdx),
        Some("vmdk") => Some(ContainerKind::Vmdk),
        Some("vdi") => Some(ContainerKind::Vdi),
        Some("qcow2") => Some(ContainerKind::Qcow2),
        Some("nhd") => Some(ContainerKind::Nhd),
        Some("hdi") => Some(ContainerKind::Hdi),
        Some("fdi") => Some(ContainerKind::Fdi),
        Some(other) => return Err(invalid(format!("Unknown container {other}"))),
    })
}

#[derive(Deserialize)]
struct NewSpec {
    path: String,
    /// Floppy format id, or None for a hard disk image of `size_mb`.
    format: Option<String>,
    size_mb: Option<f64>,
    label: Option<String>,
    /// "none", "mbr" or "gpt".
    scheme: Option<String>,
    /// "fat" (default), "ext2" or "ext3".
    #[serde(default)]
    fs: Option<String>,
    container: Option<String>,
    dynamic: bool,
    /// Custom layout; overrides `format` and `size_mb`.
    #[serde(default)]
    geometry: Option<Geometry>,
}

/// The save dialog already confirmed replacing an existing file, so this overwrites.
#[tauri::command(async)]
fn new_image(spec: NewSpec, state: State<AppState>) -> CmdResult<Opened> {
    let path = PathBuf::from(&spec.path);
    let opts = CreateOptions {
        label: spec.label.filter(|l| !l.trim().is_empty()),
        fs: FsKind::parse(spec.fs.as_deref().unwrap_or("fat"))
            .ok_or_else(|| Error::Invalid(format!("unknown file system {:?}", spec.fs)))?,
        container: container_of(spec.container.as_deref())?,
        scheme: match spec.scheme.as_deref() {
            Some("mbr") => PartitionScheme::Mbr,
            Some("gpt") => PartitionScheme::Gpt,
            _ => PartitionScheme::None,
        },
        dynamic: spec.dynamic,
        overwrite: true,
    };
    Volume::create(
        &path,
        spec_of(spec.format.as_deref(), spec.size_mb, spec.geometry)?,
        &opts,
    )?;
    let vol = Volume::open(&path, &OpenOptions::default())?;
    let out = opened(&path, &vol)?;
    remember(&state, path, None, None);
    Ok(out)
}

#[tauri::command]
fn close_image(state: State<AppState>) {
    *state.0.lock().unwrap() = Current::default();
}

#[tauri::command(async)]
fn image_info(state: State<AppState>) -> CmdResult<VolumeInfo> {
    Ok(open_current(&state, false)?.info()?)
}

#[tauri::command(async)]
fn list_dir(dir: String, state: State<AppState>) -> CmdResult<Vec<Entry>> {
    Ok(open_current(&state, false)?.list(&dir)?)
}

#[tauri::command(async)]
fn inject(
    paths: Vec<String>,
    dest: String,
    overwrite: bool,
    state: State<AppState>,
) -> CmdResult<Stats> {
    let vol = open_current(&state, true)?;
    let mut total = Stats::default();
    for p in &paths {
        add(&mut total, vol.inject(Path::new(p), &dest, overwrite)?);
    }
    vol.close()?;
    Ok(total)
}

/// `paths` empty means everything in the image.
#[tauri::command(async)]
fn extract(
    paths: Vec<String>,
    dest: String,
    overwrite: bool,
    state: State<AppState>,
) -> CmdResult<Stats> {
    let vol = open_current(&state, false)?;
    let paths = if paths.is_empty() {
        vec![String::new()]
    } else {
        paths
    };
    let mut total = Stats::default();
    for p in &paths {
        add(&mut total, vol.extract(p, Path::new(&dest), overwrite)?);
    }
    Ok(total)
}

/// The UI confirms before calling, so folders are removed with their contents.
#[tauri::command(async)]
fn delete(paths: Vec<String>, state: State<AppState>) -> CmdResult<Stats> {
    let vol = open_current(&state, true)?;
    let mut total = Stats::default();
    for p in &paths {
        add(&mut total, vol.remove(p, true)?);
    }
    vol.close()?;
    Ok(total)
}

#[tauri::command(async)]
fn make_dir(path: String, state: State<AppState>) -> CmdResult<()> {
    let vol = open_current(&state, true)?;
    vol.mkdir(&path)?;
    Ok(vol.close()?)
}

#[tauri::command(async)]
fn rename(from: String, to: String, state: State<AppState>) -> CmdResult<()> {
    let vol = open_current(&state, true)?;
    vol.rename(&from, &to)?;
    Ok(vol.close()?)
}

#[tauri::command(async)]
fn set_label(label: Option<String>, state: State<AppState>) -> CmdResult<Opened> {
    image_current(&state, true)?.set_label(label.as_deref())?;
    reopened(&state)
}

#[tauri::command(async)]
fn boot_info(state: State<AppState>) -> CmdResult<BootSectorInfo> {
    Ok(image_current(&state, false)?.boot_sector_info()?)
}

#[tauri::command(async)]
fn edit_boot(edit: BootSectorEdit, state: State<AppState>) -> CmdResult<BootSectorInfo> {
    let img = image_current(&state, true)?;
    img.edit_boot_sector(&edit)?;
    Ok(img.boot_sector_info()?)
}

/// The save dialog already confirmed replacing the file.
#[tauri::command(async)]
fn export_boot(dest: String, state: State<AppState>) -> CmdResult<()> {
    std::fs::write(dest, image_current(&state, false)?.boot_sector()?)?;
    Ok(())
}

#[tauri::command(async)]
fn import_boot(src: String, state: State<AppState>) -> CmdResult<BootSectorInfo> {
    let data = std::fs::read(src)?;
    let img = image_current(&state, true)?;
    img.import_boot_sector(&data)?;
    Ok(img.boot_sector_info()?)
}

#[tauri::command(async)]
fn defrag(analyze: bool, zero_free: bool, state: State<AppState>) -> CmdResult<DefragReport> {
    let img = image_current(&state, !analyze)?;
    Ok(if analyze {
        img.analyze_fragmentation()?
    } else {
        img.defragment(zero_free)?
    })
}

#[tauri::command(async)]
fn change_format(
    format: Option<String>,
    size_mb: Option<f64>,
    geometry: Option<Geometry>,
    state: State<AppState>,
) -> CmdResult<Opened> {
    let (path, opts) = current_opts(&state, true)?;
    Volume::change_format(&path, &opts, spec_of(format.as_deref(), size_mb, geometry)?)?;
    reopened(&state)
}

/// Resize a partitioned disk (see `Volume::resize_disk`).
#[tauri::command(async)]
fn resize_disk(size_mb: f64, keep_partitions: bool, state: State<AppState>) -> CmdResult<Opened> {
    let (path, opts) = current_opts(&state, true)?;
    if !(size_mb.is_finite() && size_mb >= 2.0) {
        return Err(Error::Invalid("the disk size must be 2 MB or more".into()).into());
    }
    let size = (size_mb * 1_048_576.0) as u64 / 512 * 512;
    Volume::resize_disk(&path, &opts, size, !keep_partitions)?;
    reopened(&state)
}

/// Save a copy in another container; the save dialog already confirmed replacing `dest`.
#[tauri::command(async)]
fn save_as(
    dest: String,
    container: Option<String>,
    dynamic: bool,
    state: State<AppState>,
) -> CmdResult<ConvertReport> {
    let (path, opts) = current_opts(&state, false)?;
    Ok(convert(
        &path,
        opts.member.as_deref(),
        Path::new(&dest),
        container_of(container.as_deref())?,
        dynamic,
        true,
    )?)
}

#[tauri::command(async)]
fn make_iso(
    folder: String,
    dest: String,
    label: String,
    boot: Option<String>,
    efi: bool,
) -> CmdResult<IsoBuildReport> {
    let opts = IsoBuildOptions {
        volume_id: label,
        boot_image: boot.map(PathBuf::from),
        boot_platform: if efi {
            BootPlatform::Uefi
        } else {
            BootPlatform::Bios
        },
        overwrite: true,
        ..Default::default()
    };
    Ok(build_iso(Path::new(&folder), Path::new(&dest), &opts)?)
}

#[tauri::command(async)]
fn export_boot_image(entry: usize, dest: String, state: State<AppState>) -> CmdResult<u64> {
    let data = open_current(&state, false)?.boot_image(entry)?;
    std::fs::write(dest, &data)?;
    Ok(data.len() as u64)
}

/// New differencing (undo) VHD over the open VHD; the save dialog confirmed replacing.
#[tauri::command(async)]
fn make_differencing(dest: String, state: State<AppState>) -> CmdResult<Opened> {
    let (parent, _) = current_opts(&state, false)?;
    let child = PathBuf::from(dest);
    create_differencing(&child, &parent, true)?;
    let vol = Volume::open(&child, &OpenOptions::default())?;
    let out = opened(&child, &vol)?;
    remember(&state, child, None, None);
    Ok(out)
}

/// The layout of an existing image or a saved 512-byte boot sector, for "same as" in the
/// New and Change format dialogs. `None` reads the open image.
#[tauri::command(async)]
fn geometry_of(path: Option<String>, state: State<AppState>) -> CmdResult<Geometry> {
    let boot = match path {
        Some(p) if std::fs::metadata(&p)?.len() == 512 => std::fs::read(&p)?,
        Some(p) => Image::open(Path::new(&p), &OpenOptions::default())?.boot_sector()?,
        None => image_current(&state, false)?.boot_sector()?,
    };
    Ok(Geometry::from_boot_sector(&boot)?)
}

/// Attributes and dates of one or more entries. Returns how many entries changed.
#[tauri::command(async)]
fn edit_entries(
    paths: Vec<String>,
    edit: EntryEdit,
    recursive: bool,
    state: State<AppState>,
) -> CmdResult<u64> {
    let img = image_current(&state, true)?;
    let mut n = 0;
    for p in &paths {
        n += img.edit_entry(p, &edit, recursive)?;
    }
    Ok(n)
}

/// `what`: "disk" (the disk inside the container), "image" (the image file as stored) or
/// "file" (one file inside the image, `path`).
#[tauri::command(async)]
fn checksum(what: String, path: Option<String>, state: State<AppState>) -> CmdResult<Checksums> {
    match what.as_str() {
        "disk" => Ok(image_current(&state, false)?.checksum_disk()?),
        "image" => {
            let (p, _) = current_opts(&state, false)?;
            Ok(checksum_file(&p)?)
        }
        "file" => {
            let path = path.ok_or_else(|| invalid("Choose a file first.".into()))?;
            Ok(open_current(&state, false)?.checksum(&path)?)
        }
        other => Err(invalid(format!("Unknown checksum target {other}"))),
    }
}

/// Write a listing of `dir` and everything under it. The save dialog confirmed replacing.
#[tauri::command(async)]
fn export_listing(
    dest: String,
    format: ListingFormat,
    dir: String,
    state: State<AppState>,
) -> CmdResult<usize> {
    let (path, _) = current_opts(&state, false)?;
    let entries = open_current(&state, false)?.walk(&dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let title = if dir.is_empty() {
        name
    } else {
        format!("{name} — /{dir}")
    };
    std::fs::write(dest, render_listing(&title, &entries, format))?;
    Ok(entries.len())
}

#[tauri::command(async)]
fn truncate(state: State<AppState>) -> CmdResult<TruncateReport> {
    let (path, opts) = current_opts(&state, false)?;
    if opts.partition.is_some() || opts.member.is_some() {
        return Err(invalid(
            "Only raw images holding a single volume can be truncated.".into(),
        ));
    }
    Ok(truncate_image(&path)?)
}

#[tauri::command(async)]
fn iso_replace(path: String, src: String, state: State<AppState>) -> CmdResult<()> {
    let data = std::fs::read(src)?;
    Ok(image_current(&state, true)?.iso_replace_file(&path, &data)?)
}

/// The physical drives (USB sticks, memory cards, floppy drives, disks).
#[tauri::command(async)]
fn drives() -> CmdResult<Vec<DriveInfo>> {
    Ok(list_drives()?)
}

/// Sent to the window as "progress" while a drive is read or written.
#[derive(Clone, Serialize)]
struct ProgressEvent {
    phase: String,
    done: u64,
    total: u64,
}

/// A progress callback that emits at most one event per percent.
fn emitter(app: &tauri::AppHandle) -> impl FnMut(&str, u64, u64) + '_ {
    let mut last = (String::new(), u64::MAX);
    move |phase, done, total| {
        let pct = done * 100 / total.max(1);
        if last.0 == phase && last.1 == pct {
            return;
        }
        last = (phase.to_string(), pct);
        let _ = app.emit(
            "progress",
            ProgressEvent {
                phase: phase.to_string(),
                done,
                total,
            },
        );
    }
}

/// Read a whole drive into a new image file.
#[tauri::command(async)]
fn read_drive(
    drive: String,
    dest: String,
    container: Option<String>,
    dynamic: bool,
    app: tauri::AppHandle,
) -> CmdResult<ConvertReport> {
    let mut progress = emitter(&app);
    Ok(convert_with_progress(
        Path::new(&drive),
        None,
        Path::new(&dest),
        container_of(container.as_deref())?,
        dynamic,
        true,
        &mut |done, total| progress("reading", done, total),
    )?)
}

/// Write an image onto a drive, replacing everything on it.
#[tauri::command(async)]
fn write_drive(
    image: String,
    drive: String,
    verify: bool,
    allow_fixed: bool,
    app: tauri::AppHandle,
    state: State<AppState>,
) -> CmdResult<DriveWriteReport> {
    let member = {
        let cur = state.0.lock().unwrap();
        cur.path
            .as_deref()
            .filter(|p| *p == Path::new(&image))
            .and(cur.member.clone())
    };
    let mut progress = emitter(&app);
    Ok(write_image_to_drive(
        Path::new(&image),
        &drive,
        &DriveWriteOptions {
            verify,
            allow_fixed,
            member,
        },
        &mut progress,
    )?)
}

/// Erase a whole drive: zeros, or random data and then zeros.
#[tauri::command(async)]
fn wipe_drive(
    drive: String,
    random: bool,
    verify: bool,
    allow_fixed: bool,
    app: tauri::AppHandle,
) -> CmdResult<DriveWriteReport> {
    let mut progress = emitter(&app);
    Ok(sectorzero_core::wipe_drive(
        &drive,
        &sectorzero_core::DriveWipeOptions {
            pattern: if random {
                sectorzero_core::WipePattern::Random
            } else {
                sectorzero_core::WipePattern::Zero
            },
            verify,
            allow_fixed,
        },
        &mut progress,
    )?)
}

/// Eject a removable drive's medium.
#[tauri::command(async)]
fn eject_drive(drive: String) -> CmdResult<()> {
    Ok(sectorzero_core::eject_drive(&drive)?)
}

/// Files copied out of images (for opening or dragging) go under here. The folder is
/// cleared when the app starts and when it exits.
fn temp_root() -> PathBuf {
    std::env::temp_dir().join("SectorZero")
}

fn clean_temp() {
    let _ = std::fs::remove_dir_all(temp_root());
}

/// Copy `paths` out of the image into a new folder under [`temp_root`]; returns where each
/// one landed.
fn extract_to_temp(
    kind: &str,
    paths: &[String],
    state: &State<AppState>,
) -> CmdResult<Vec<PathBuf>> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let vol = open_current(state, false)?;
    let dir = temp_root().join(format!(
        "{kind}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)?;
    let mut out = Vec::new();
    for p in paths {
        vol.extract(p, &dir, true)?;
        let name = p.trim_end_matches('/').rsplit('/').next().unwrap_or(p);
        out.push(dir.join(name));
    }
    Ok(out)
}

/// Double-click: copy the file out and open it with its default program.
#[tauri::command(async)]
fn open_entry(path: String, state: State<AppState>) -> CmdResult<()> {
    let file = extract_to_temp("open", std::slice::from_ref(&path), &state)?.remove(0);
    tauri_plugin_opener::open_path(&file, None::<&str>).map_err(|e| invalid(e.to_string()))
}

/// Drag entries out of the window: copy them out, then start a native file drag.
/// Resolves with true when they were dropped somewhere, false when the drag was cancelled.
#[tauri::command(async)]
fn drag_out(
    paths: Vec<String>,
    window: tauri::Window,
    app: tauri::AppHandle,
    state: State<AppState>,
) -> CmdResult<bool> {
    let files = extract_to_temp("drag", &paths, &state)?;
    let (tx, rx) = std::sync::mpsc::channel::<Result<bool, String>>();
    app.run_on_main_thread(move || {
        #[cfg(target_os = "linux")]
        let handle = window.gtk_window();
        #[cfg(not(target_os = "linux"))]
        let handle = tauri::Result::Ok(window.clone());
        let done = tx.clone();
        let started = match handle {
            Ok(w) => drag::start_drag(
                &w,
                drag::DragItem::Files(files),
                drag::Image::Raw(include_bytes!("../icons/32x32.png").to_vec()),
                move |result, _| {
                    let _ = done.send(Ok(matches!(result, drag::DragResult::Dropped)));
                },
                drag::Options::default(),
            )
            .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = started {
            let _ = tx.send(Err(e));
        }
    })
    .map_err(|e| invalid(e.to_string()))?;
    rx.recv()
        .map_err(|e| invalid(e.to_string()))?
        .map_err(invalid)
}

fn add(total: &mut Stats, s: Stats) {
    total.files += s.files;
    total.dirs += s.dirs;
    total.bytes += s.bytes;
}

fn main() {
    clean_temp();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            formats,
            media_formats,
            open_image,
            new_image,
            close_image,
            image_info,
            list_dir,
            inject,
            extract,
            delete,
            make_dir,
            rename,
            set_label,
            boot_info,
            edit_boot,
            export_boot,
            import_boot,
            defrag,
            change_format,
            resize_disk,
            save_as,
            make_iso,
            export_boot_image,
            geometry_of,
            edit_entries,
            checksum,
            export_listing,
            truncate,
            iso_replace,
            open_entry,
            drag_out,
            make_differencing,
            drives,
            read_drive,
            write_drive,
            wipe_drive,
            eject_drive,
        ])
        .build(tauri::generate_context!())
        .expect("error while starting SectorZero");
    app.run(|_, event| {
        if let tauri::RunEvent::Exit = event {
            clean_temp();
        }
    });
}
