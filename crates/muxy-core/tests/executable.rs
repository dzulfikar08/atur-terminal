use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[test]
fn command_arguments_preserve_quoting_without_shell_expansion() -> Result {
    let arguments = muxy_core::executable::command_arguments(
        r#"'/Applications/VM Tool/bin/vm' ssh "dev vm" 'it'\''s mine' escaped\ space '' "$HOME" '*.txt' ';' '$(touch nope)' "a\qb""#,
    )?;
    assert_eq!(
        arguments,
        [
            "/Applications/VM Tool/bin/vm",
            "ssh",
            "dev vm",
            "it's mine",
            "escaped space",
            "",
            "$HOME",
            "*.txt",
            ";",
            "$(touch nope)",
            "a\\qb",
        ]
    );
    Ok(())
}

#[test]
fn malformed_commands_cannot_turn_into_a_different_invocation() {
    for source in [
        "", "  ", "'' ssh", "vm 'box", "vm \"box", "vm box\\", "vm\nssh", "vm\0ssh",
    ] {
        assert_eq!(
            muxy_core::executable::command_arguments(source)
                .err()
                .map(|error| error.kind()),
            Some(std::io::ErrorKind::InvalidInput),
            "{source:?}",
        );
    }
}

#[test]
fn current_path_keeps_the_running_generation_after_a_link_switch() -> Result {
    if let Some(directory) = std::env::var_os("ATUR_TERM_TEST_EXECUTABLE_PATH") {
        let directory = std::path::PathBuf::from(directory);
        fs::write(directory.join("ready"), b"")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !directory.join("continue").exists() {
            if Instant::now() >= deadline {
                return Err("executable-path probe timed out".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::write(
            directory.join("result"),
            muxy_core::executable::current_path()?
                .as_os_str()
                .as_bytes(),
        )?;
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    for generation in ["old", "new"] {
        let target = directory.path().join(generation);
        fs::create_dir(&target)?;
        // A child copy cannot leave writable executable FDs in concurrent forks.
        assert!(
            Command::new("cp")
                .arg(std::env::current_exe()?)
                .arg(target.join("probe"))
                .status()?
                .success()
        );
    }
    symlink("old", directory.path().join("current"))?;
    symlink("current/probe", directory.path().join("probe"))?;
    let mut child = Command::new(directory.path().join("probe"))
        .args([
            "--exact",
            "current_path_keeps_the_running_generation_after_a_link_switch",
        ])
        .env("ATUR_TERM_TEST_EXECUTABLE_PATH", directory.path())
        .stdout(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !directory.path().join("ready").exists() && Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Err("executable-path probe exited before readiness".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    symlink("new", directory.path().join("next"))?;
    fs::rename(
        directory.path().join("next"),
        directory.path().join("current"),
    )?;
    fs::write(directory.path().join("continue"), b"")?;
    assert!(child.wait()?.success());
    assert_eq!(
        fs::read(directory.path().join("result"))?,
        directory
            .path()
            .join("old/probe")
            .canonicalize()?
            .as_os_str()
            .as_bytes(),
    );
    Ok(())
}
