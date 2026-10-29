use std::error::Error;
use std::path::Path;

/// Opens `word`, a folder, in the desktop app. Without it, or over SSH, the
/// folder opens in the terminal UI instead.
pub(crate) fn run(word: &Path) -> Result<(), Box<dyn Error>> {
    let folder = word
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir())
        .ok_or_else(|| {
            format!(
                "{} is not a command or a folder; run atur-term --help",
                word.display()
            )
        })?;
    if crate::terminal::over_ssh() || !desktop_app(Some(&folder)) {
        crate::tui::run(None, Some(&folder))?;
    }
    Ok(())
}

/// Opens the desktop app, with `folder` as a project if given, returning
/// false if it can't.
#[cfg(target_os = "macos")]
pub(crate) fn desktop_app(folder: Option<&Path>) -> bool {
    use std::process::{Command, Stdio};
    let Ok(executable) = muxy_core::executable::current_path() else {
        return false;
    };
    let mut open = Command::new("/usr/bin/open");
    match containing_app(&executable) {
        Some(app) => open.arg("-a").arg(app),
        None => open.args([
            "-b",
            muxy_core::release::Channel::current().bundle_identifier(),
        ]),
    };
    open.args(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// On Linux the app is installed beside this tool. It needs a desktop
/// session to open in.
#[cfg(not(target_os = "macos"))]
pub(crate) fn desktop_app(folder: Option<&Path>) -> bool {
    let Ok(executable) = muxy_core::executable::current_path() else {
        return false;
    };
    desktop_session(|name| std::env::var_os(name))
        && launch(&executable.with_file_name("atur-terminal"), folder)
}

#[cfg(not(target_os = "macos"))]
fn desktop_session(variable: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|name| variable(name).is_some_and(|value| !value.is_empty()))
}

/// Starts the app with `folder`, away from this terminal. An app that is
/// already running takes the folder from the new one, which then leaves:
/// that, or an app still running after a moment, is a folder opened.
#[cfg(not(target_os = "macos"))]
fn launch(app: &Path, folder: Option<&Path>) -> bool {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let Ok(mut child) = Command::new(app)
        .args(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    else {
        return false;
    };
    let settled = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < settled => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => return true,
        }
    }
}

#[cfg(target_os = "macos")]
fn containing_app(executable: &Path) -> Option<&Path> {
    let executables = executable.parent()?;
    let contents = executables.parent()?;
    let app = contents.parent()?;
    (executables.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then_some(app)
}

#[cfg(all(test, not(target_os = "macos")))]
#[allow(clippy::expect_used, reason = "Regression fixture setup")]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;

    fn stub(root: &Path, script: &str) -> PathBuf {
        let app = root.join("atur-terminal");
        let status = Command::new("/bin/sh")
            .args(["-c", r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#, "sh"])
            .arg(format!("#!/bin/sh\n{script}\n"))
            .arg(&app)
            .status()
            .expect("stub");
        assert!(status.success(), "stub: {status}");
        app
    }

    #[test]
    fn the_desktop_app_opens_only_in_a_desktop_session() {
        let session = |set: &'static [(&'static str, &'static str)]| {
            desktop_session(move |name| {
                set.iter()
                    .find(|(variable, _)| *variable == name)
                    .map(|(_, value)| OsString::from(value))
            })
        };
        assert!(session(&[("WAYLAND_DISPLAY", "wayland-0")]));
        assert!(session(&[("DISPLAY", ":0")]));
        assert!(!session(&[]), "a console or a service has no desktop");
        assert!(!session(&[("DISPLAY", ""), ("WAYLAND_DISPLAY", "")]));
    }

    #[test]
    fn the_folder_reaches_the_app_byte_for_byte_and_never_through_a_shell() {
        let root = tempfile::tempdir().expect("root");
        let seen = root.path().join("seen");
        let app = stub(
            root.path(),
            &format!(
                "printf '%s' \"$1\" > '{}'; echo $# >> '{}'",
                seen.display(),
                seen.display()
            ),
        );
        let injected = root.path().join("injected");
        let mut name = format!(
            "/srv/My Project/$(touch {0}); `touch {0}` 'q' café ",
            injected.display()
        )
        .into_bytes();
        name.push(0xff);
        let folder = PathBuf::from(OsString::from_vec(name));
        assert!(launch(&app, Some(&folder)));
        let mut expected = folder.as_os_str().as_bytes().to_vec();
        expected.extend_from_slice(b"1\n");
        assert_eq!(std::fs::read(&seen).expect("seen"), expected);
        assert!(!injected.exists());
    }

    #[test]
    fn an_app_that_cannot_start_leaves_the_folder_to_the_terminal_ui() {
        let root = tempfile::tempdir().expect("root");
        assert!(!launch(
            &root.path().join("atur-terminal"),
            Some(Path::new("/srv"))
        ));
        let failing = stub(root.path(), "exit 3");
        assert!(!launch(&failing, Some(Path::new("/srv"))));
        let starting = stub(root.path(), "exec sleep 2");
        assert!(launch(&starting, Some(Path::new("/srv"))), "still running");
    }
}
