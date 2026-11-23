//! SectorZero desktop app: a thin Tauri shell over `sectorzero-core`.
//!
//! The app remembers which image is open; each command reopens it for the duration of
//! the call, so nothing holds the file open between actions.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sectorzero_core::{
    Entry, Error, FLOPPY_FORMATS, FloppyFormat, NewImage, OpenOptions, Stats, Volume, VolumeInfo,
};
use serde::Serialize;
use tauri::State;

#[derive(Default)]
struct Current {
    path: Option<PathBuf>,
    partition: Option<usize>,
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

fn open_current(state: &State<AppState>, writable: bool) -> CmdResult<Volume> {
    let cur = state.0.lock().unwrap();
    let path = cur.path.as_ref().ok_or_else(no_image)?;
    Ok(Volume::open(
        path,
        OpenOptions {
            writable,
            partition: cur.partition,
        },
    )?)
}

fn remember(state: &State<AppState>, path: PathBuf, partition: Option<usize>) {
    *state.0.lock().unwrap() = Current {
        path: Some(path),
        partition,
    };
}

#[tauri::command]
fn formats() -> Vec<FloppyFormat> {
    FLOPPY_FORMATS.to_vec()
}

#[tauri::command(async)]
fn open_image(path: String, partition: Option<usize>, state: State<AppState>) -> CmdResult<Opened> {
    let path = PathBuf::from(path);
    let vol = Volume::open(
        &path,
        OpenOptions {
            writable: false,
            partition,
        },
    )?;
    let out = opened(&path, &vol)?;
    remember(&state, path, partition);
    Ok(out)
}

/// The save dialog already confirmed replacing an existing file, so this overwrites.
#[tauri::command(async)]
fn new_image(
    path: String,
    format: String,
    label: Option<String>,
    state: State<AppState>,
) -> CmdResult<Opened> {
    let fmt = FloppyFormat::find(&format).ok_or_else(|| CmdError {
        kind: "invalid",
        message: format!("Unknown format {format}"),
    })?;
    let path = PathBuf::from(path);
    let label = label.filter(|l| !l.trim().is_empty());
    Volume::create(&path, NewImage::Floppy(fmt), label.as_deref(), true)?;
    let vol = Volume::open(&path, OpenOptions::default())?;
    let out = opened(&path, &vol)?;
    remember(&state, path, None);
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running SectorZero");
}
