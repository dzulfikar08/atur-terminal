//! Which terminal backend the server runs.
//!
//! By default the server spawns shells itself. `backend.toml` beside `server.toml`
//! or the environment hands terminals to a herdr server instead:
//!
//! ```toml
//! kind = "herdr"
//! herdr_bin = "/usr/local/bin/herdr"           # default: herdr on PATH
//! herdr_socket = "/home/me/.config/herdr/herdr.sock"  # default: $HERDR_SOCKET_PATH or herdr's own
//! ```
//!
//! `MUXY_BACKEND`, `MUXY_HERDR_BIN` and `MUXY_HERDR_SOCKET` override the file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use muxy_server::HerdrConfig;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BackendFile {
    kind: Option<String>,
    herdr_bin: Option<PathBuf>,
    herdr_socket: Option<PathBuf>,
}

/// The herdr configuration when the server should use herdr, `None` for local PTYs.
pub(crate) fn herdr(directory: &Path) -> io::Result<Option<HerdrConfig>> {
    let path = directory.join("backend.toml");
    let file = match fs::read_to_string(&path) {
        Ok(contents) => toml::from_str::<BackendFile>(&contents).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {error}", path.display()),
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => BackendFile::default(),
        Err(error) => return Err(error),
    };
    resolve(
        file,
        std::env::var("MUXY_BACKEND").ok(),
        std::env::var_os("MUXY_HERDR_BIN").map(PathBuf::from),
        std::env::var_os("MUXY_HERDR_SOCKET").map(PathBuf::from),
    )
}

fn resolve(
    file: BackendFile,
    kind: Option<String>,
    bin: Option<PathBuf>,
    socket: Option<PathBuf>,
) -> io::Result<Option<HerdrConfig>> {
    match kind.or(file.kind).as_deref() {
        None | Some("" | "local") => Ok(None),
        Some("herdr") => Ok(Some(HerdrConfig {
            bin: bin.or(file.herdr_bin),
            socket: socket.or(file.herdr_socket),
        })),
        Some(other) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown terminal backend {other:?}; use \"local\" or \"herdr\""),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_is_the_default_and_the_environment_wins() {
        assert_eq!(
            resolve(BackendFile::default(), None, None, None).ok(),
            Some(None)
        );
        let file = BackendFile {
            kind: Some("local".into()),
            herdr_bin: Some("/a".into()),
            herdr_socket: None,
        };
        let config = resolve(file, Some("herdr".into()), None, Some("/s".into()))
            .expect("valid")
            .expect("herdr");
        assert_eq!(config.bin.as_deref(), Some(Path::new("/a")));
        assert_eq!(config.socket.as_deref(), Some(Path::new("/s")));
    }

    #[test]
    fn rejects_unknown_backends() {
        assert!(resolve(BackendFile::default(), Some("tmux".into()), None, None).is_err());
    }
}
