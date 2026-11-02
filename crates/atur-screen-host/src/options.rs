//! The command line of `atur-term-screen`.
//!
//! The program is started through Launch Services, which passes no environment,
//! so every setting is an argument; `atur-screen-source` builds them.

use std::path::PathBuf;

use crate::keymap::KeyStyle;

/// What the program was asked to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Capture the main display and serve it.
    Serve(Serve),
    /// Serve a moving test pattern instead of the screen (tests).
    Synthetic(Serve, (u32, u32)),
    /// Report the permissions without asking for them.
    Check { status: PathBuf },
    /// Ask macOS for the permissions, then report them.
    Request { status: PathBuf },
}

/// Settings of a serving program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Serve {
    pub socket: PathBuf,
    pub status: PathBuf,
    /// The server that started this program; it stops when that process is gone.
    pub owner: u32,
    pub fps: u32,
    /// Longest side of the shared picture in pixels.
    pub max_edge: u32,
    pub keys: KeyStyle,
    pub name: Option<String>,
    pub view_only: bool,
}

pub const DEFAULT_FPS: u32 = 60;
pub const DEFAULT_MAX_EDGE: u32 = 2560;
const MAX_FPS: u32 = 120;
const MIN_EDGE: u32 = 320;
const MAX_EDGE: u32 = 8192;

/// Parses the arguments after the program name.
///
/// # Errors
/// A sentence saying what is wrong with the arguments.
pub fn parse<I>(arguments: I) -> Result<Mode, String>
where
    I: IntoIterator<Item = String>,
{
    let mut words = arguments.into_iter();
    let mode = words.next().ok_or("no mode given")?;
    let mut socket = None;
    let mut status = None;
    let mut owner = None;
    let mut fps = DEFAULT_FPS;
    let mut max_edge = DEFAULT_MAX_EDGE;
    let mut keys = KeyStyle::default();
    let mut name = None;
    let mut view_only = false;
    let mut size = (1280, 720);
    while let Some(flag) = words.next() {
        match flag.as_str() {
            "--view-only" => view_only = true,
            "--socket" | "--status" | "--owner" | "--fps" | "--max-edge" | "--keys" | "--name"
            | "--size" => {
                let value = words
                    .next()
                    .ok_or_else(|| format!("{flag} needs a value"))?;
                match flag.as_str() {
                    "--socket" => socket = Some(PathBuf::from(value)),
                    "--status" => status = Some(PathBuf::from(value)),
                    "--owner" => owner = Some(number(&flag, &value)?),
                    "--fps" => fps = number(&flag, &value)?.clamp(1, MAX_FPS),
                    "--max-edge" => max_edge = number(&flag, &value)?.clamp(MIN_EDGE, MAX_EDGE),
                    "--keys" => {
                        keys = KeyStyle::parse(&value).ok_or_else(|| {
                            format!("--keys must be command or literal, not {value}")
                        })?;
                    }
                    "--name" => name = Some(value),
                    _ => size = parse_size(&value)?,
                }
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let status = status.ok_or("--status is required")?;
    match mode.as_str() {
        "check" => Ok(Mode::Check { status }),
        "request" => Ok(Mode::Request { status }),
        "serve" | "synthetic" => {
            let serve = Serve {
                socket: socket.ok_or("--socket is required")?,
                status,
                owner: owner.ok_or("--owner is required")?,
                fps,
                max_edge,
                keys,
                name,
                view_only,
            };
            if mode == "serve" {
                Ok(Mode::Serve(serve))
            } else {
                Ok(Mode::Synthetic(serve, size))
            }
        }
        other => Err(format!("unknown mode {other}")),
    }
}

fn number(flag: &str, value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} needs a number, not {value}"))
}

fn parse_size(value: &str) -> Result<(u32, u32), String> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| format!("--size must look like 1280x720, not {value}"))?;
    let (width, height) = (number("--size", width)?, number("--size", height)?);
    if !(16..=8192).contains(&width) || !(16..=8192).contains(&height) {
        return Err(format!("--size {value} is out of range"));
    }
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn serve_takes_what_the_launcher_passes() {
        let mode = parse(args(
            "serve --socket /t/s --status /t/status --owner 42 --fps 30 --max-edge 1920 \
             --keys literal --name Air",
        ))
        .unwrap();
        let Mode::Serve(serve) = mode else {
            panic!("not serve");
        };
        assert_eq!(serve.socket, PathBuf::from("/t/s"));
        assert_eq!((serve.owner, serve.fps, serve.max_edge), (42, 30, 1920));
        assert_eq!(serve.keys, KeyStyle::Literal);
        assert_eq!(serve.name.as_deref(), Some("Air"));
        assert!(!serve.view_only);
    }

    #[test]
    fn defaults_and_clamps() {
        let Mode::Serve(serve) =
            parse(args("serve --socket s --status f --owner 1 --fps 9000")).unwrap()
        else {
            panic!("not serve");
        };
        assert_eq!(serve.fps, MAX_FPS);
        assert_eq!(serve.max_edge, DEFAULT_MAX_EDGE);
        assert_eq!(serve.keys, KeyStyle::Command);
    }

    #[test]
    fn check_and_request_need_only_a_status_file() {
        assert_eq!(
            parse(args("check --status /t/f")).unwrap(),
            Mode::Check {
                status: PathBuf::from("/t/f")
            }
        );
        assert!(matches!(
            parse(args("request --status /t/f")),
            Ok(Mode::Request { .. })
        ));
    }

    #[test]
    fn synthetic_has_a_size() {
        let mode = parse(args(
            "synthetic --socket s --status f --owner 1 --size 640x360",
        ))
        .unwrap();
        assert!(matches!(mode, Mode::Synthetic(_, (640, 360))));
        assert!(parse(args("synthetic --socket s --status f --owner 1 --size 4x4")).is_err());
    }

    #[test]
    fn mistakes_are_named() {
        assert!(parse(args("")).is_err());
        assert!(
            parse(args("serve --status f --owner 1"))
                .unwrap_err()
                .contains("--socket")
        );
        assert!(parse(args("serve --socket s --status f --owner x")).is_err());
        assert!(parse(args("serve --socket s --status f --owner 1 --keys odd")).is_err());
        assert!(
            parse(args("dance --status f"))
                .unwrap_err()
                .contains("dance")
        );
        assert!(
            parse(args("check --status"))
                .unwrap_err()
                .contains("needs a value")
        );
    }
}
