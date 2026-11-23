//! SectorZero desktop app: a thin Tauri shell over `sectorzero-core`.
//!
//! The app remembers which image is open; each command reopens it for the duration of
//! the call, so nothing holds the file open between actions.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sectorzero_core::{
    BootPlatform, BootSectorEdit, BootSectorInfo, ContainerKind, ConvertReport, CreateOptions,
    DefragReport, Entry, Error, FLOPPY_FORMATS, FloppyFormat, Image, IsoBuildOptions,
    IsoBuildReport, NewImage, OpenOptions, PartitionScheme, Stats, Volume, VolumeInfo, build_iso,
    convert,
};
use serde::{Deserialize, Serialize};
use tauri::State;

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

fn spec_of(format: Option<&str>, size_mb: Option<f64>) -> CmdResult<NewImage> {
    match (format, size_mb) {
        (Some(f), _) => Ok(NewImage::Floppy(
            FloppyFormat::find(f).ok_or_else(|| invalid(format!("Unknown format {f}")))?,
        )),
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
    container: Option<String>,
    dynamic: bool,
}

/// The save dialog already confirmed replacing an existing file, so this overwrites.
#[tauri::command(async)]
fn new_image(spec: NewSpec, state: State<AppState>) -> CmdResult<Opened> {
    let path = PathBuf::from(&spec.path);
    let opts = CreateOptions {
        label: spec.label.filter(|l| !l.trim().is_empty()),
        container: container_of(spec.container.as_deref())?,
        scheme: match spec.scheme.as_deref() {
            Some("mbr") => PartitionScheme::Mbr,
            Some("gpt") => PartitionScheme::Gpt,
            _ => PartitionScheme::None,
        },
        dynamic: spec.dynamic,
        overwrite: true,
    };
    Volume::create(&path, spec_of(spec.format.as_deref(), spec.size_mb)?, &opts)?;
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
    state: State<AppState>,
) -> CmdResult<Opened> {
    let (path, opts) = current_opts(&state, true)?;
    Volume::change_format(&path, &opts, spec_of(format.as_deref(), size_mb)?)?;
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

fn add(total: &mut Stats, s: Stats) {
    total.files += s.files;
    total.dirs += s.dirs;
    total.bytes += s.bytes;
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            formats,
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
            save_as,
            make_iso,
            export_boot_image,
        ])
        .run(tauri::generate_context!())
        .expect("error while running SectorZero");
}
