//! `atur-term screen`: whether this Mac lets Atur Terminal share its screen,
//! and asking for the permissions it needs.
//!
//! macOS grants Screen Recording and Accessibility to an application, here
//! the small screen sharing program inside the Atur Terminal app, which is
//! why this command asks that program (through [`HelperApp`]) and not the
//! system directly. It only means something on a Mac.

use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use atur_screen_source::launch::{self, HelperApp, Permissions};

use crate::args::Screen;

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

/// How often `permission` looks again, and how long it waits for the user.
struct Timing {
    poll: Duration,
    wait: Duration,
}

const TIMING: Timing = Timing {
    poll: Duration::from_secs(2),
    wait: Duration::from_secs(120),
};

/// A pane of System Settings, Privacy & Security.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pane {
    ScreenRecording,
    Accessibility,
}

impl Pane {
    fn name(self) -> &'static str {
        match self {
            Self::ScreenRecording => "Screen & System Audio Recording",
            Self::Accessibility => "Accessibility",
        }
    }

    fn address(self) -> &'static str {
        match self {
            Self::ScreenRecording => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
        }
    }
}

/// The private folder the program's status file is kept in, removed afterwards.
struct Folder(PathBuf);

impl Folder {
    fn create() -> io::Result<Self> {
        launch::private_folder(&format!("atur-screen-check-{}", std::process::id())).map(Self)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn run(command: Screen) -> Result {
    let mut out = io::stdout().lock();
    if !cfg!(target_os = "macos") {
        writeln!(out, "Screen sharing permissions are only needed on a Mac.")?;
        return Ok(());
    }
    let helper = HelperApp::find()?;
    let folder = Folder::create()?;
    match command {
        Screen::Status => status(&mut out, &helper, &folder.0),
        Screen::Permission => permission(&mut out, &helper, &folder.0, &TIMING, open_pane),
    }
}

/// What is allowed now.
fn status(out: &mut impl Write, helper: &HelperApp, folder: &Path) -> Result {
    let permissions = helper.check_permissions(folder)?;
    report(out, permissions)?;
    verdict(permissions)
}

/// Shows the system's prompts, opens the Settings pane for what is still
/// missing, and waits until both permissions are allowed or the time is up.
fn permission(
    out: &mut impl Write,
    helper: &HelperApp,
    folder: &Path,
    timing: &Timing,
    mut open: impl FnMut(Pane),
) -> Result {
    let name = helper.display_name();
    writeln!(
        out,
        "{name} needs two permissions on this Mac to share its screen.\n\
         A prompt appears on this Mac's screen for each one. In each prompt, click Open System \
         Settings and turn on {name}: first under Screen & System Audio Recording, then under \
         Accessibility.\n\
         macOS may offer to quit and reopen the program; choose Later or Quit & Reopen. Nothing \
         else is needed.\n\
         Waiting for you to allow them; press Ctrl-C to stop."
    )?;
    out.flush()?;
    let mut now = helper.request_permissions(folder)?;
    let started = Instant::now();
    let mut opened: Option<Pane> = None;
    while !now.all_allowed() && started.elapsed() < timing.wait {
        let pane = if now.screen_recording {
            Pane::Accessibility
        } else {
            Pane::ScreenRecording
        };
        if opened != Some(pane) {
            writeln!(
                out,
                "Opening System Settings → Privacy & Security → {}…",
                pane.name()
            )?;
            out.flush()?;
            open(pane);
            opened = Some(pane);
        }
        thread::sleep(timing.poll);
        let next = helper.check_permissions(folder)?;
        if next.screen_recording && !now.screen_recording {
            writeln!(out, "Screen recording is now allowed.")?;
        }
        if next.accessibility && !now.accessibility {
            writeln!(out, "Accessibility is now allowed.")?;
        }
        out.flush()?;
        now = next;
    }
    report(out, now)?;
    verdict(now)
}

fn open_pane(pane: Pane) {
    let _ = Command::new("/usr/bin/open")
        .arg(pane.address())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn report(out: &mut impl Write, permissions: Permissions) -> io::Result<()> {
    writeln!(
        out,
        "Screen recording: {}",
        word(permissions.screen_recording)
    )?;
    writeln!(
        out,
        "Accessibility (mouse and keyboard): {}",
        word(permissions.accessibility)
    )
}

fn word(allowed: bool) -> &'static str {
    if allowed { "allowed" } else { "not allowed" }
}

/// Success only when everything needed is allowed.
fn verdict(permissions: Permissions) -> Result {
    if permissions.all_allowed() {
        return Ok(());
    }
    let message = if permissions.screen_recording {
        "Accessibility is not allowed yet, so a viewer could watch this Mac but not use its mouse \
         and keyboard. Run atur-term screen permission to allow it."
    } else {
        "Screen recording is not allowed yet, so this Mac's screen cannot be shared. Run atur-term \
         screen permission to allow it."
    };
    Err(message.into())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A stand-in for the program: counts how often it was run in the folder of
    /// its status file and writes the status the case below asks for.
    const SCRIPT: &str = r#"#!/bin/sh
mode="$1"
shift
while [ $# -gt 0 ]; do
  case "$1" in
    --status) status="$2"; shift 2 ;;
    *) shift ;;
  esac
done
folder=$(dirname "$status")
n=$(cat "$folder/count" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$folder/count"
report() {
  printf '{"state":"%s","pid":%s,"screen_recording":%s,"accessibility":%s,"message":"","width":0,"height":0}' \
    "$1" "$$" "$2" "$3" > "$status.tmp"
  mv "$status.tmp" "$status"
}
"#;

    fn fake(directory: &Path, cases: &str) -> (HelperApp, PathBuf) {
        let path = directory.join("helper.sh");
        fs::write(&path, format!("{SCRIPT}{cases}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let run = directory.join("run");
        fs::create_dir_all(&run).unwrap();
        (HelperApp::from_executable(path), run)
    }

    fn quick(wait: Duration) -> Timing {
        Timing {
            poll: Duration::from_millis(1),
            wait,
        }
    }

    fn text(out: Vec<u8>) -> String {
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_panes_are_opened_one_after_the_other_as_each_permission_is_given() {
        let directory = tempfile::tempdir().unwrap();
        let (helper, run) = fake(
            directory.path(),
            "case \"$mode:$n\" in\n  request:1) report needs-permission false false ;;\n  check:2) report ready true false ;;\n  *) report ready true true ;;\nesac\n",
        );
        let mut out = Vec::new();
        let mut opened = Vec::new();
        permission(
            &mut out,
            &helper,
            &run,
            &quick(Duration::from_secs(30)),
            |pane| opened.push(pane),
        )
        .unwrap();
        assert_eq!(opened, [Pane::ScreenRecording, Pane::Accessibility]);
        let out = text(out);
        assert!(out.contains("needs two permissions on this Mac"), "{out}");
        assert!(out.contains("Open System Settings"), "{out}");
        assert!(out.contains("Later or Quit & Reopen"), "{out}");
        assert!(
            out.contains(
                "Opening System Settings → Privacy & Security → Screen & System Audio Recording…"
            ),
            "{out}"
        );
        assert!(
            out.contains("Opening System Settings → Privacy & Security → Accessibility…"),
            "{out}"
        );
        assert!(out.contains("Screen recording is now allowed."), "{out}");
        assert!(out.contains("Accessibility is now allowed."), "{out}");
        assert!(
            out.ends_with(
                "Screen recording: allowed\nAccessibility (mouse and keyboard): allowed\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn nothing_is_opened_when_everything_is_already_allowed() {
        let directory = tempfile::tempdir().unwrap();
        let (helper, run) = fake(directory.path(), "report ready true true\n");
        let mut out = Vec::new();
        let mut opened = Vec::new();
        permission(
            &mut out,
            &helper,
            &run,
            &quick(Duration::from_secs(30)),
            |pane| {
                opened.push(pane);
            },
        )
        .unwrap();
        assert!(opened.is_empty());
        assert!(text(out).ends_with("Accessibility (mouse and keyboard): allowed\n"));
    }

    #[test]
    fn waiting_ends_with_what_is_still_missing() {
        let directory = tempfile::tempdir().unwrap();
        let (helper, run) = fake(directory.path(), "report needs-permission false false\n");
        let mut out = Vec::new();
        let mut opened = Vec::new();
        let error = permission(
            &mut out,
            &helper,
            &run,
            &quick(Duration::from_millis(100)),
            |pane| opened.push(pane),
        )
        .unwrap_err();
        assert_eq!(opened, [Pane::ScreenRecording]);
        assert!(
            error
                .to_string()
                .starts_with("Screen recording is not allowed yet"),
            "{error}"
        );
        assert!(text(out).ends_with(
            "Screen recording: not allowed\nAccessibility (mouse and keyboard): not allowed\n"
        ));
    }

    #[test]
    fn status_prints_both_permissions_and_succeeds_only_when_both_are_allowed() {
        let directory = tempfile::tempdir().unwrap();
        let (helper, run) = fake(directory.path(), "report ready true false\n");
        let mut out = Vec::new();
        let error = status(&mut out, &helper, &run).unwrap_err();
        assert_eq!(
            text(out),
            "Screen recording: allowed\nAccessibility (mouse and keyboard): not allowed\n"
        );
        assert!(
            error
                .to_string()
                .starts_with("Accessibility is not allowed yet"),
            "{error}"
        );

        let (helper, run) = fake(directory.path(), "report ready true true\n");
        let mut out = Vec::new();
        status(&mut out, &helper, &run).unwrap();
        assert_eq!(
            text(out),
            "Screen recording: allowed\nAccessibility (mouse and keyboard): allowed\n"
        );

        let (helper, run) = fake(directory.path(), "report needs-permission false false\n");
        let error = status(&mut Vec::<u8>::new(), &helper, &run).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("Screen recording is not allowed yet"),
            "{error}"
        );
    }
}
