use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::release::{Channel, Version};

/// Files carried over from an earlier install's data directory on first start. Sockets,
/// logs, locks and shell integration belong to the running install and are never
/// copied.
const MIGRATED_FILES: [&str; 5] = [
    "server.toml",
    "catalog.json",
    "remote.json",
    "backend.toml",
    "herdr.json",
];

/// The session catalog and the saved session archives, which live in `sessions/`.
const SESSIONS_DIRECTORY: &str = "sessions";
const SESSION_CATALOG: &str = "catalog.json";
const SESSION_ARCHIVE_EXTENSION: &str = "postcard";

pub fn muxy_dir() -> io::Result<PathBuf> {
    let linux = cfg!(target_os = "linux");
    let override_path = crate::env::var_os("ATUR_TERM_DIR");
    let overridden = override_path.is_some();
    let home = env::var_os("HOME");
    let state_home = env::var_os("XDG_STATE_HOME");
    let (name, legacy_name) =
        directory_names(env!("CARGO_PKG_VERSION"), cfg!(debug_assertions), linux);
    let directory =
        resolve_directory(override_path, home.clone(), state_home.clone(), name, linux)?;
    if !overridden && let Ok(legacy) = resolve_directory(None, home, state_home, legacy_name, linux)
    {
        let _ = migrate_legacy(&legacy, &directory);
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    Ok(directory)
}

/// Releases of each channel and local builds never share data. The second
/// name is the legacy directory the same channel used before the rename.
fn directory_names(version: &str, debug: bool, linux: bool) -> (&'static str, &'static str) {
    let channel = Version::parse(version)
        .filter(|_| !debug)
        .map(Version::channel);
    match (channel, linux) {
        (None, false) => ("Atur Terminal Dev", "Muxy Dev"),
        (None, true) => ("atur-terminal-dev", "muxy-dev"),
        (Some(Channel::Beta), false) => ("Atur Terminal Beta", "Muxy Beta"),
        (Some(Channel::Beta), true) => ("atur-terminal-beta", "muxy-beta"),
        (Some(Channel::Stable), false) => ("Atur Terminal", "Muxy 2"),
        (Some(Channel::Stable), true) => ("atur-terminal", "muxy"),
    }
}

/// Copies the allowlisted configuration of an earlier install into `directory`
/// when `directory` does not exist yet. The old directory is never modified.
/// Files are staged in a sibling directory and renamed into place, so a
/// crash leaves either no new directory or a complete one.
fn migrate_legacy(legacy: &Path, directory: &Path) -> io::Result<bool> {
    if directory.symlink_metadata().is_ok() || !legacy.is_dir() {
        return Ok(false);
    }
    let Some(parent) = directory.parent() else {
        return Ok(false);
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".atur-terminal-migrate-")
        .tempdir_in(parent)?;
    let mut copied = false;
    for name in MIGRATED_FILES {
        let source = legacy.join(name);
        if source.symlink_metadata().is_ok_and(|meta| meta.is_file()) {
            fs::copy(&source, staging.path().join(name))?;
            copied = true;
        }
    }
    copied |= copy_sessions(
        &legacy.join(SESSIONS_DIRECTORY),
        &staging.path().join(SESSIONS_DIRECTORY),
    )?;
    if !copied {
        return Ok(false);
    }
    fs::set_permissions(
        staging.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    let staged = staging.keep();
    match fs::rename(&staged, directory) {
        Ok(()) => Ok(true),
        Err(error) => {
            let _ = fs::remove_dir_all(&staged);
            Err(error)
        }
    }
}

/// Copies the session catalog and the session archives (regular files only) so the new
/// server finds the sessions the old one was running and can restore their history.
/// Each file is complete when read, because the server replaces its files by rename; a
/// copy taken while the old server runs can still pair a catalog with archives from a
/// moment earlier or later, so migrate only when the old server is stopped.
fn copy_sessions(source: &Path, destination: &Path) -> io::Result<bool> {
    if !source.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
        return Ok(false);
    }
    let mut copied = false;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let wanted = entry.file_name() == SESSION_CATALOG
            || path
                .extension()
                .is_some_and(|extension| extension == SESSION_ARCHIVE_EXTENSION);
        if !wanted || !path.symlink_metadata().is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        if !copied {
            fs::DirBuilder::new().mode(0o700).create(destination)?;
        }
        fs::copy(&path, destination.join(entry.file_name()))?;
        copied = true;
    }
    Ok(copied)
}

fn resolve_directory(
    override_path: Option<OsString>,
    home: Option<OsString>,
    state_home: Option<OsString>,
    name: &str,
    linux: bool,
) -> io::Result<PathBuf> {
    if let Some(path) = override_path {
        return if path.is_empty() {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ATUR_TERM_DIR must not be empty",
            ))
        } else {
            Ok(PathBuf::from(path))
        };
    }
    if linux && let Some(state_home) = state_home {
        let state_home = PathBuf::from(state_home);
        if !state_home.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "XDG_STATE_HOME must be absolute",
            ));
        }
        return Ok(state_home.join(name));
    }
    let home = home
        .filter(|home| !home.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join(if linux {
            ".local/state"
        } else {
            "Library/Application Support"
        })
        .join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_and_development_builds_do_not_share_data_on_either_platform() -> io::Result<()> {
        for linux in [false, true] {
            for (version, debug, macos_name, linux_name, macos_old, linux_old) in [
                (
                    "2.0.0-beta-0",
                    true,
                    "Atur Terminal Dev",
                    "atur-terminal-dev",
                    "Muxy Dev",
                    "muxy-dev",
                ),
                (
                    "2.0.0-beta-0",
                    false,
                    "Atur Terminal Dev",
                    "atur-terminal-dev",
                    "Muxy Dev",
                    "muxy-dev",
                ),
                (
                    "2.0.0-beta.1001",
                    true,
                    "Atur Terminal Dev",
                    "atur-terminal-dev",
                    "Muxy Dev",
                    "muxy-dev",
                ),
                (
                    "2.0.0-beta.1001",
                    false,
                    "Atur Terminal Beta",
                    "atur-terminal-beta",
                    "Muxy Beta",
                    "muxy-beta",
                ),
                (
                    "2.0.0",
                    true,
                    "Atur Terminal Dev",
                    "atur-terminal-dev",
                    "Muxy Dev",
                    "muxy-dev",
                ),
                (
                    "2.0.0",
                    false,
                    "Atur Terminal",
                    "atur-terminal",
                    "Muxy 2",
                    "muxy",
                ),
            ] {
                let (expected, old) = if linux {
                    (
                        format!(".local/state/{linux_name}"),
                        format!(".local/state/{linux_old}"),
                    )
                } else {
                    (
                        format!("Library/Application Support/{macos_name}"),
                        format!("Library/Application Support/{macos_old}"),
                    )
                };
                let (name, legacy) = directory_names(version, debug, linux);
                let resolve =
                    |name| resolve_directory(None, Some("/home/test".into()), None, name, linux);
                assert_eq!(resolve(name)?, PathBuf::from("/home/test").join(expected));
                assert_eq!(resolve(legacy)?, PathBuf::from("/home/test").join(old));
            }
        }
        Ok(())
    }

    fn legacy_install(root: &Path) -> io::Result<PathBuf> {
        let legacy = root.join("muxy");
        fs::create_dir_all(legacy.join("sessions"))?;
        for name in MIGRATED_FILES {
            fs::write(legacy.join(name), name)?;
        }
        for name in ["server.sock", "server.log", "server.lock", "tui.lock"] {
            fs::write(legacy.join(name), name)?;
        }
        fs::write(legacy.join("sessions/one"), "session")?;
        fs::write(legacy.join("sessions/catalog.json"), "session catalog")?;
        Ok(legacy)
    }

    #[test]
    fn migration_copies_only_allowlisted_files_and_leaves_the_old_directory() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let legacy = legacy_install(root.path())?;
        let directory = root.path().join("atur-terminal");
        assert!(migrate_legacy(&legacy, &directory)?);
        for name in MIGRATED_FILES {
            assert_eq!(fs::read_to_string(directory.join(name))?, name);
            assert_eq!(fs::read_to_string(legacy.join(name))?, name);
        }
        let mut entries = fs::read_dir(&directory)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort();
        let mut expected = MIGRATED_FILES.map(OsString::from).to_vec();
        expected.sort();
        expected.push(OsString::from("sessions"));
        expected.sort();
        assert_eq!(entries, expected);
        assert!(legacy.join("sessions/one").exists());
        assert!(!directory.join("sessions/one").exists());
        assert!(legacy.join("server.sock").exists());
        let siblings = fs::read_dir(root.path())?.count();
        assert_eq!(siblings, 2, "staging directory must not linger");
        Ok(())
    }

    #[test]
    fn migration_carries_the_session_catalog_and_archives_of_a_real_layout() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let legacy = root.path().join("muxy");
        let sessions = legacy.join("sessions");
        fs::create_dir_all(&sessions)?;
        fs::write(
            sessions.join("catalog.json"),
            r#"{"revision":7,"sessions":{}}"#,
        )?;
        fs::write(
            sessions.join("0434bbb961f02554.postcard"),
            b"MUXYSAV3 archive",
        )?;
        fs::write(
            sessions.join("5147a2e1ff668a0d.postcard"),
            b"MUXYSAV2 archive",
        )?;
        fs::write(sessions.join(".0434bbb961f02554-1.tmp"), "half written")?;
        fs::create_dir(sessions.join("catalog.backup"))?;
        fs::write(legacy.join("herdr.json"), "{}")?;
        fs::write(legacy.join("server.log"), "log")?;
        let directory = root.path().join("atur-terminal");
        assert!(migrate_legacy(&legacy, &directory)?);
        let mut names = fs::read_dir(directory.join("sessions"))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        assert_eq!(
            names,
            [
                "0434bbb961f02554.postcard",
                "5147a2e1ff668a0d.postcard",
                "catalog.json"
            ]
            .map(OsString::from)
        );
        assert_eq!(
            fs::read_to_string(directory.join("sessions/catalog.json"))?,
            r#"{"revision":7,"sessions":{}}"#
        );
        assert_eq!(
            fs::read(directory.join("sessions/0434bbb961f02554.postcard"))?,
            b"MUXYSAV3 archive"
        );
        assert!(
            sessions.join("catalog.json").exists(),
            "the old directory stays untouched"
        );
        assert!(!directory.join("server.log").exists());
        Ok(())
    }

    #[test]
    fn migration_works_with_only_sessions_present() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let legacy = root.path().join("muxy");
        fs::create_dir_all(legacy.join("sessions"))?;
        fs::write(legacy.join("sessions/catalog.json"), "{}")?;
        let directory = root.path().join("atur-terminal");
        assert!(migrate_legacy(&legacy, &directory)?);
        assert!(directory.join("sessions/catalog.json").is_file());
        Ok(())
    }

    #[test]
    fn migration_does_nothing_when_the_new_directory_exists() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let legacy = legacy_install(root.path())?;
        let directory = root.path().join("atur-terminal");
        fs::create_dir(&directory)?;
        assert!(!migrate_legacy(&legacy, &directory)?);
        assert_eq!(fs::read_dir(&directory)?.count(), 0);
        Ok(())
    }

    #[test]
    fn migration_does_nothing_without_an_old_directory() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let directory = root.path().join("atur-terminal");
        assert!(!migrate_legacy(&root.path().join("muxy"), &directory)?);
        assert!(!directory.exists());
        Ok(())
    }

    #[test]
    fn migration_skips_directories_and_symlinks_named_like_allowed_files() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let legacy = root.path().join("muxy");
        fs::create_dir_all(legacy.join("server.toml"))?;
        fs::write(root.path().join("secret"), "x")?;
        std::os::unix::fs::symlink(root.path().join("secret"), legacy.join("catalog.json"))?;
        let directory = root.path().join("atur-terminal");
        assert!(!migrate_legacy(&legacy, &directory)?);
        assert!(!directory.exists());
        Ok(())
    }

    #[test]
    fn override_resolves_to_itself_so_nothing_is_migrated_from_it() -> io::Result<()> {
        assert_eq!(
            resolve_directory(Some("/custom".into()), None, None, "atur-terminal", true)?,
            PathBuf::from("/custom")
        );
        Ok(())
    }
}
