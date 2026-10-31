//! The saved state of a profile: one layout file every window shares, and one
//! view file per window.
//!
//! The layout file (`desktop-state.json`) holds the projects, tabs, panes,
//! tab groups, and workspaces. A window's view file (`desktop-state.view.json`
//! for the main window) holds what only that window shows: the focused pane,
//! zoom, selected tabs, and its bounds. A view only ever refers to the layout;
//! one that names a tab or pane the layout no longer has loses just that
//! reference. A missing or unreadable view is not an error: the window starts
//! on the first tab of Home.
//!
//! A layout file from before the split, or one a restore wrote, still holds
//! its window. That window is used once, and the next save moves it into the
//! view file.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::view::{self, MAIN_WINDOW, View};
use crate::{AppError, AppState};

pub fn default_path() -> Result<PathBuf, AppError> {
    muxy_core::dirs::muxy_dir()
        .map(|directory| directory.join("desktop-state.json"))
        .map_err(|source| AppError::Io {
            path: PathBuf::from("Atur Terminal state directory"),
            source,
        })
}

/// Where `window` keeps its view of the layout file at `path`.
pub fn view_path(path: impl AsRef<Path>, window: &str) -> Result<PathBuf, AppError> {
    view::validate_name(window)?;
    let path = path.as_ref();
    let mut name = path
        .file_stem()
        .map_or_else(|| OsString::from("state"), OsString::from);
    name.push(".view");
    if window != MAIN_WINDOW {
        name.push("-");
        name.push(window);
    }
    name.push(".json");
    Ok(path.with_file_name(name))
}

/// The state as the main window sees it.
pub fn load(path: impl AsRef<Path>) -> Result<AppState, AppError> {
    load_window(path, MAIN_WINDOW)
}

/// The state as `window` sees it: the layout file with the window's view.
pub fn load_window(path: impl AsRef<Path>, window: &str) -> Result<AppState, AppError> {
    let path = path.as_ref();
    let view_file = view_path(path, window)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if path
                .file_name()
                .is_some_and(|name| name == "desktop-state.json")
                && path
                    .with_file_name("state.json")
                    .try_exists()
                    .map_err(|source| AppError::Io {
                        path: path.into(),
                        source,
                    })?
            {
                return load_window(path.with_file_name("state.json"), window);
            }
            return AppState::bootstrap();
        }
        Err(source) => {
            return Err(AppError::Io {
                path: path.into(),
                source,
            });
        }
    };
    let json = |source| AppError::Json {
        path: path.into(),
        source,
    };
    AppState::from_saved(&bytes, || read_view(&view_file)).map_err(json)
}

/// A view that can't be read is dropped: it only records what a window showed.
fn read_view(path: &Path) -> Option<View> {
    let saved: View = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (saved.version <= view::VERSION).then_some(saved)
}

/// Saves the state as the main window sees it.
pub fn save(path: impl AsRef<Path>, state: &AppState) -> Result<(), AppError> {
    save_window(path, MAIN_WINDOW, state)
}

/// Saves the layout and `window`'s view. A file that would not change is not
/// written, so a window that only moved focus or zoomed leaves the layout
/// file, and the other windows' views, alone.
pub fn save_window(path: impl AsRef<Path>, window: &str, state: &AppState) -> Result<(), AppError> {
    let path = path.as_ref();
    let view_file = view_path(path, window)?;
    let json = |source| AppError::Json {
        path: path.into(),
        source,
    };
    let layout = state.layout_bytes().map_err(json)?;
    let view = state.view_bytes().map_err(json)?;
    write_if_changed(path, &layout)?;
    write_if_changed(&view_file, &view)
}

/// Adopts the layout another window saved, keeping this window's view of it.
/// Returns whether the layout changed. Layout changes this window has not
/// saved yet are replaced, so save first.
pub fn sync_window(
    path: impl AsRef<Path>,
    window: &str,
    state: &mut AppState,
) -> Result<bool, AppError> {
    let path = path.as_ref();
    view_path(path, window)?;
    let bytes = fs::read(path).map_err(|source| AppError::Io {
        path: path.into(),
        source,
    })?;
    let json = |source| AppError::Json {
        path: path.into(),
        source,
    };
    if state.layout_bytes().map_err(json)? == bytes {
        return Ok(false);
    }
    *state = state.with_layout_of(&bytes).map_err(json)?;
    Ok(true)
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    if fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    write(path, bytes)
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut file = open_temporary(&temporary).map_err(|source| AppError::Io {
        path: temporary.clone(),
        source,
    })?;
    let result = file
        .set_len(0)
        .and_then(|()| file.write_all(bytes))
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, path))
        .and_then(|()| File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all());
    if let Err(source) = result {
        let _ = fs::remove_file(&temporary);
        return Err(AppError::Io {
            path: path.into(),
            source,
        });
    }
    Ok(())
}

fn open_temporary(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    file.try_lock()?;
    let opened = file.metadata()?;
    let current = fs::metadata(path)?;
    if (opened.dev(), opened.ino()) != (current.dev(), current.ino()) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "temporary state file was replaced by another writer",
        ));
    }
    Ok(file)
}
