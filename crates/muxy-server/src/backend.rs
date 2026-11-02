//! Which terminal backend the server runs.
//!
//! By default the server spawns shells itself. `backend.toml` beside `server.toml`
//! or the environment hands terminals to a herdr server instead:
//!
//! ```toml
//! kind = "herdr"
//! herdr_bin = "/usr/local/bin/herdr"           # default: herdr on PATH
//! herdr_socket = "/home/me/.config/herdr/herdr.sock"  # default: $HERDR_SOCKET_PATH or herdr's own
//! images = true                                # default: show the images of herdr's panes
//! ```
//!
//! `ATUR_TERM_BACKEND`, `ATUR_TERM_HERDR_BIN` and `ATUR_TERM_HERDR_SOCKET` override the file.
//! `images = false` (or `ATUR_TERM_HERDR_IMAGES=0`) turns off the images of herdr's panes.

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
    images: Option<bool>,
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
        muxy_core::env::var("ATUR_TERM_BACKEND"),
        muxy_core::env::var_os("ATUR_TERM_HERDR_BIN").map(PathBuf::from),
        muxy_core::env::var_os("ATUR_TERM_HERDR_SOCKET").map(PathBuf::from),
        muxy_core::env::var("ATUR_TERM_HERDR_IMAGES")
            .map(|value| parse_switch(&value))
            .transpose()?,
    )
}

fn parse_switch(value: &str) -> io::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Ok(true),
        "0" | "false" | "off" | "no" => Ok(false),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ATUR_TERM_HERDR_IMAGES is {other:?}; use 1 or 0"),
        )),
    }
}

fn resolve(
    file: BackendFile,
    kind: Option<String>,
    bin: Option<PathBuf>,
    socket: Option<PathBuf>,
    images: Option<bool>,
) -> io::Result<Option<HerdrConfig>> {
    match kind.or(file.kind).as_deref() {
        None | Some("" | "local") => Ok(None),
        Some("herdr") => Ok(Some(HerdrConfig {
            bin: bin.or(file.herdr_bin),
            socket: socket.or(file.herdr_socket),
            images: images.or(file.images),
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
            resolve(BackendFile::default(), None, None, None, None).ok(),
            Some(None)
        );
        let file = BackendFile {
            kind: Some("local".into()),
            herdr_bin: Some("/a".into()),
            herdr_socket: None,
            images: Some(false),
        };
        let config = resolve(file, Some("herdr".into()), None, Some("/s".into()), None)
            .expect("valid")
            .expect("herdr");
        assert_eq!(config.bin.as_deref(), Some(Path::new("/a")));
        assert_eq!(config.socket.as_deref(), Some(Path::new("/s")));
        assert_eq!(config.images, Some(false));
    }

    #[test]
    fn images_are_on_unless_the_file_or_the_environment_turns_them_off() {
        let on = |file: Option<bool>, environment: Option<&str>| {
            let file = BackendFile {
                images: file,
                ..BackendFile::default()
            };
            let images = environment.map(|value| parse_switch(value).expect("a switch"));
            resolve(file, Some("herdr".into()), None, None, images)
                .expect("valid")
                .expect("herdr")
                .images
        };
        assert_eq!(on(None, None), None);
        assert_eq!(on(Some(false), None), Some(false));
        assert_eq!(on(Some(false), Some("1")), Some(true));
        assert_eq!(on(None, Some("off")), Some(false));
        assert!(parse_switch("maybe").is_err());
    }

    #[test]
    fn rejects_unknown_backends() {
        assert!(
            resolve(
                BackendFile::default(),
                Some("tmux".into()),
                None,
                None,
                None
            )
            .is_err()
        );
    }
}
