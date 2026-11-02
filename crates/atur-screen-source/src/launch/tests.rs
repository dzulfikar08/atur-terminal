use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// What every stand-in program starts with: it notes its arguments and its
/// process number in the folder of its status file, and can report a status.
const PRELUDE: &str = r#"#!/bin/sh
mode="$1"
shift
all="$*"
while [ $# -gt 0 ]; do
  case "$1" in
    --status) status="$2"; shift 2 ;;
    --socket) socket="$2"; shift 2 ;;
    *) shift ;;
  esac
done
folder=$(dirname "$status")
echo "$mode $all" > "$folder/args"
echo "$$" > "$folder/pid"
report() {
  printf '{"state":"%s","pid":%s,"screen_recording":%s,"accessibility":%s,"message":"%s","width":1280,"height":720}' \
    "$1" "$$" "$2" "$3" "$4" > "$status.tmp"
  mv "$status.tmp" "$status"
}
"#;

/// A scratch folder with an empty `run` folder inside.
fn scratch(label: &str) -> PathBuf {
    let folder = std::env::temp_dir().join(format!(
        "atur-launch-{}-{}-{label}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&folder);
    fs::create_dir_all(folder.join("run")).unwrap();
    folder
}

fn write_script(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn fake_helper(scratch: &Path, body: &str) -> HelperApp {
    let path = scratch.join("helper.sh");
    write_script(&path, &format!("{PRELUDE}{body}"));
    HelperApp::from_executable(path)
}

/// A bundle whose `open` is a script of ours that starts the program in the
/// background, as Launch Services would, and records how it was called.
fn fake_bundle(scratch: &Path, opener_body: &str, body: &str) -> HelperApp {
    let app = scratch.join("Atur Terminal Screen.app");
    write_script(
        &app.join("Contents/MacOS").join(EXECUTABLE),
        &format!("{PRELUDE}{body}"),
    );
    let opener = scratch.join("open.sh");
    write_script(&opener, &format!("#!/bin/sh\n{opener_body}"));
    HelperApp {
        target: Target::Bundle { app, opener },
    }
}

fn quick() -> ServeOptions {
    ServeOptions {
        ready_timeout: Duration::from_secs(10),
        ..ServeOptions::default()
    }
}

fn pid_in(run: &Path) -> u32 {
    fs::read_to_string(run.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn texts(arguments: &[OsString]) -> Vec<String> {
    arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

/// Puts an app with its helper bundle and the helper's executable in `folder`.
fn install(folder: &Path, app: &str) {
    let helper = helper_in_app(&folder.join(app)).unwrap();
    let executable = helper.join("Contents/MacOS").join(EXECUTABLE);
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(executable, b"").unwrap();
}

#[test]
fn the_helper_sits_inside_the_app_that_holds_the_executable() {
    assert_eq!(
        helper_in_bundle(Path::new(
            "/Applications/Atur Terminal.app/Contents/MacOS/atur-term-server"
        )),
        Some(PathBuf::from(
            "/Applications/Atur Terminal.app/Contents/Helpers/Atur Terminal Screen.app"
        ))
    );
    assert_eq!(
        helper_in_bundle(Path::new(
            "/Users/me/Applications/Atur Terminal Beta.app/Contents/MacOS/atur-term"
        )),
        Some(PathBuf::from(
            "/Users/me/Applications/Atur Terminal Beta.app/Contents/Helpers/Atur Terminal Beta Screen.app"
        ))
    );
    for outside in [
        "/usr/local/bin/atur-term",
        "/Applications/Atur Terminal.app/Contents/Resources/atur-term",
        "/Applications/Other/Contents/MacOS/atur-term",
        "atur-term",
        "/atur-term",
    ] {
        assert_eq!(helper_in_bundle(Path::new(outside)), None, "{outside}");
    }
}

#[test]
fn the_environment_names_a_plain_executable_and_nothing_else_is_searched() {
    let scratch = scratch("env");
    let system = scratch.join("system");
    install(&system, "Atur Terminal.app");
    let found = HelperApp::locate(Some("/somewhere/helper".into()), None, None, &system).unwrap();
    assert_eq!(found, HelperApp::from_executable("/somewhere/helper"));
    assert_eq!(found.display_name(), "Atur Terminal Screen");
    // An empty value is no value.
    let found = HelperApp::locate(Some(OsString::new()), None, None, &system).unwrap();
    assert_eq!(found.display_name(), "Atur Terminal Screen");
    assert!(matches!(found.target, Target::Bundle { .. }));
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn the_bundle_of_the_running_app_is_preferred_to_installed_apps() {
    let scratch = scratch("own");
    let system = scratch.join("system");
    let own = scratch.join("own");
    install(&system, "Atur Terminal.app");
    install(&own, "Atur Terminal Beta.app");
    let executable = own.join("Atur Terminal Beta.app/Contents/MacOS/atur-term-server");
    let found = HelperApp::locate(None, Some(&executable), None, &system).unwrap();
    assert_eq!(found.display_name(), "Atur Terminal Beta Screen");
    // A server that is not in an app falls back to the installed ones.
    let outside = scratch.join("bin/atur-term-server");
    let found = HelperApp::locate(None, Some(&outside), None, &system).unwrap();
    assert_eq!(found.display_name(), "Atur Terminal Screen");
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn installed_apps_are_tried_in_a_fixed_order() {
    let scratch = scratch("order");
    let system = scratch.join("system");
    let home = scratch.join("home");
    install(&system, "Atur Terminal Beta.app");
    install(&home.join("Applications"), "Atur Terminal.app");
    let name = |found: Result<HelperApp, FindError>| found.unwrap().display_name().to_owned();
    // The stable app wins over the beta, wherever each one is.
    assert_eq!(
        name(HelperApp::locate(None, None, Some(&home), &system)),
        "Atur Terminal Screen"
    );
    // Only the beta is there.
    assert_eq!(
        name(HelperApp::locate(None, None, None, &system)),
        "Atur Terminal Beta Screen"
    );
    // The system folder goes before the user's folder for the same app.
    install(&system, "Atur Terminal.app");
    let found = HelperApp::locate(None, None, Some(&home), &system).unwrap();
    assert!(matches!(
        &found.target,
        Target::Bundle { app, .. } if app.starts_with(&system)
    ));
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn nothing_found_says_what_to_do() {
    let scratch = scratch("none");
    // A helper folder without the program in it does not count.
    fs::create_dir_all(
        helper_in_app(&scratch.join("Atur Terminal.app"))
            .unwrap()
            .join("Contents/MacOS"),
    )
    .unwrap();
    let error = HelperApp::locate(None, None, Some(&scratch), &scratch).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Atur Terminal's screen sharing program was not found on this Mac. Install the Atur \
         Terminal app in the Applications folder to share this Mac's screen."
    );
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn the_command_line_is_built_from_the_options() {
    let socket = Path::new("/tmp/f/s");
    let status = Path::new("/tmp/f/status");
    let owner = std::process::id().to_string();
    assert_eq!(
        texts(&ServeOptions::default().arguments(socket, status)),
        [
            "serve",
            "--socket",
            "/tmp/f/s",
            "--status",
            "/tmp/f/status",
            "--owner",
            owner.as_str()
        ]
    );
    let all = ServeOptions {
        fps: Some(30),
        max_edge: Some(1920),
        keys: Some(Keys::Command),
        name: Some("Studio Mac".into()),
        view_only: true,
        ..ServeOptions::default()
    };
    assert_eq!(
        texts(&all.arguments(socket, status)),
        [
            "serve",
            "--socket",
            "/tmp/f/s",
            "--status",
            "/tmp/f/status",
            "--owner",
            owner.as_str(),
            "--fps",
            "30",
            "--max-edge",
            "1920",
            "--keys",
            "command",
            "--name",
            "Studio Mac",
            "--view-only"
        ]
    );
    let synthetic = ServeOptions {
        synthetic: Some((640, 360)),
        fps: Some(5),
        ..ServeOptions::default()
    };
    assert_eq!(
        texts(&synthetic.arguments(socket, status)),
        [
            "synthetic",
            "--socket",
            "/tmp/f/s",
            "--status",
            "/tmp/f/status",
            "--owner",
            owner.as_str(),
            "--size",
            "640x360"
        ]
    );
}

#[test]
fn keys_are_named_command_and_literal() {
    assert_eq!(Keys::parse("command"), Some(Keys::Command));
    assert_eq!(Keys::parse("literal"), Some(Keys::Literal));
    assert_eq!(Keys::parse("Command"), None);
    assert_eq!(Keys::parse(""), None);
}

#[test]
fn the_permission_message_names_the_mac_and_the_pane() {
    assert_eq!(
        permission_message("Studio", "Atur Terminal Screen"),
        "Screen recording permission needed on Studio: open System Settings → Privacy & Security → \
         Screen & System Audio Recording and turn on Atur Terminal Screen. A prompt was shown on \
         that Mac; once you have allowed it, choose Retry connection."
    );
    assert!(
        permission_message("Studio", "Atur Terminal Beta Screen")
            .contains("turn on Atur Terminal Beta Screen.")
    );
}

#[test]
fn a_helper_that_becomes_ready_runs_until_it_is_stopped() {
    let scratch = scratch("ready");
    let run = scratch.join("run");
    let helper = fake_helper(
        &scratch,
        "report starting null null \"\"\nsleep 0.1\nreport ready true false \"Turn on Accessibility\"\nexec sleep 30\n",
    );
    let options = ServeOptions {
        fps: Some(30),
        max_edge: Some(1920),
        keys: Some(Keys::Literal),
        name: Some("Test Mac".into()),
        ..quick()
    };
    let mut running = helper.start(&run, &options).unwrap();
    assert_eq!(running.socket(), run.join("s"));
    assert_eq!(running.folder(), run);
    let status = running.status().unwrap();
    assert_eq!(status.state, State::Ready);
    assert_eq!(status.accessibility, Some(false));
    assert_eq!(running.pid(), status.pid);
    assert_eq!(running.pid(), pid_in(&run));
    assert!(running.is_running());
    let arguments = fs::read_to_string(run.join("args")).unwrap();
    assert_eq!(
        arguments.trim(),
        format!(
            "serve --socket {0}/s --status {0}/status --owner {1} --fps 30 --max-edge 1920 \
             --keys literal --name Test Mac",
            run.display(),
            std::process::id()
        )
    );
    let pid = running.pid();
    running.stop();
    assert!(!alive(pid));
    assert!(!run.exists());
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_helper_that_is_gone_is_not_running() {
    let scratch = scratch("gone");
    let run = scratch.join("run");
    let helper = fake_helper(&scratch, "report ready true true \"\"\nexec sleep 30\n");
    let mut running = helper.start(&run, &quick()).unwrap();
    let pid = running.pid();
    send(pid, Signal::KILL);
    let deadline = Instant::now() + Duration::from_secs(5);
    while running.is_running() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!running.is_running());
    running.stop();
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_helper_without_screen_recording_needs_permission() {
    let scratch = scratch("permission");
    let run = scratch.join("run");
    let helper = fake_helper(
        &scratch,
        "report needs-permission false null \"Allow it\"\nexit 0\n",
    );
    let error = helper.start(&run, &quick()).unwrap_err();
    assert_eq!(error, StartError::NeedsPermission("Allow it".into()));
    assert!(!alive(pid_in(&run)));
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_helper_that_fails_says_why() {
    let scratch = scratch("failed");
    let run = scratch.join("run");
    let helper = fake_helper(&scratch, "report failed null null \"No display\"\nexit 1\n");
    assert_eq!(
        helper.start(&run, &quick()).unwrap_err(),
        StartError::Failed("No display".into())
    );
    let helper = fake_helper(&scratch, "report failed null null \"\"\nexit 1\n");
    assert_eq!(
        helper.start(&run, &quick()).unwrap_err(),
        StartError::Failed(STOPPED_AT_START.into())
    );
    let helper = fake_helper(
        &scratch,
        "report ended null null \"Gone to sleep\"\nexit 0\n",
    );
    assert_eq!(
        helper.start(&run, &quick()).unwrap_err(),
        StartError::Failed("Gone to sleep".into())
    );
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_helper_that_never_reports_is_stopped_and_reported_late() {
    let scratch = scratch("silent");
    let run = scratch.join("run");
    let helper = fake_helper(&scratch, "exec sleep 30\n");
    let options = ServeOptions {
        ready_timeout: Duration::from_millis(500),
        ..ServeOptions::default()
    };
    let started = Instant::now();
    let error = helper.start(&run, &options).unwrap_err();
    assert!(
        matches!(&error, StartError::Failed(text) if text.contains("did not start in time")),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(!alive(pid_in(&run)));
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_helper_that_exits_at_once_is_reported_with_its_last_words() {
    let scratch = scratch("exits");
    let run = scratch.join("run");
    let helper = fake_helper(&scratch, "echo \"no display here\" >&2\nexit 3\n");
    let error = helper.start(&run, &quick()).unwrap_err();
    assert!(
        matches!(&error, StartError::Failed(text)
            if text.contains("stopped before it was ready") && text.contains("no display here")),
        "{error:?}"
    );
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_stale_status_is_not_believed() {
    let scratch = scratch("stale");
    let run = scratch.join("run");
    let mut stale = Status::new(State::Ready, 1);
    stale.message = "old".into();
    stale.write(&run.join(STATUS)).unwrap();
    let helper = fake_helper(&scratch, "exit 0\n");
    let error = helper.start(&run, &quick()).unwrap_err();
    assert!(
        matches!(&error, StartError::Failed(text) if text != "old"),
        "{error:?}"
    );
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_helper_run_through_open_is_stopped_by_its_process_number() {
    let scratch = scratch("bundle");
    let run = scratch.join("run");
    let marker = scratch.join("termed");
    let log = scratch.join("open.log");
    let bundle = fake_bundle(
        &scratch,
        &format!(
            "echo \"$@\" > \"{}\"\napp=\"\"\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -a) app=\"$2\"; shift 2 ;;\n    --args) shift; break ;;\n    *) shift ;;\n  esac\ndone\n\"$app/Contents/MacOS/{EXECUTABLE}\" \"$@\" </dev/null >/dev/null 2>&1 &\nexit 0\n",
            log.display()
        ),
        &format!(
            "trap 'echo term > \"{}\"; exit 0' TERM\nreport ready true true \"\"\nwhile true; do sleep 0.05; done\n",
            marker.display()
        ),
    );
    assert_eq!(bundle.display_name(), "Atur Terminal Screen");
    let mut running = bundle.start(&run, &quick()).unwrap();
    assert_eq!(running.pid(), pid_in(&run));
    assert!(running.is_running());
    let called = fs::read_to_string(&log).unwrap();
    assert!(
        called.starts_with(&format!(
            "-n -g -j -a {} --args serve --socket {}/s --status {}/status --owner {}",
            scratch.join("Atur Terminal Screen.app").display(),
            run.display(),
            run.display(),
            std::process::id()
        )),
        "{called}"
    );
    running.stop();
    assert!(marker.exists(), "the program was not asked to stop");
    assert!(!run.exists());
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn an_open_that_refuses_is_reported_with_its_first_line() {
    let scratch = scratch("refused");
    let run = scratch.join("run");
    let bundle = fake_bundle(
        &scratch,
        "echo \"The application cannot be opened for an unexpected reason (-600).\" >&2\necho \"second line\" >&2\nexit 1\n",
        "exit 0\n",
    );
    let error = bundle.start(&run, &quick()).unwrap_err();
    let StartError::Failed(text) = &error else {
        panic!("{error:?}");
    };
    assert!(
        text.contains("The application cannot be opened for an unexpected reason (-600)."),
        "{text}"
    );
    assert!(!text.contains("second line"), "{text}");
    assert!(text.contains("logged in"), "{text}");
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn permissions_are_asked_without_serving() {
    let scratch = scratch("check");
    let run = scratch.join("run");
    let helper = fake_helper(&scratch, "report ready true false \"\"\nexit 0\n");
    let permissions = helper.check_permissions(&run).unwrap();
    assert_eq!(
        permissions,
        Permissions {
            screen_recording: true,
            accessibility: false
        }
    );
    assert!(!permissions.all_allowed());
    assert_eq!(
        fs::read_to_string(run.join("args")).unwrap().trim(),
        format!("check --status {}/status", run.display())
    );

    let helper = fake_helper(
        &scratch,
        "report needs-permission false false \"\"\nexit 0\n",
    );
    let permissions = helper.request_permissions(&run).unwrap();
    assert_eq!(
        permissions,
        Permissions {
            screen_recording: false,
            accessibility: false
        }
    );
    assert_eq!(
        fs::read_to_string(run.join("args")).unwrap().trim(),
        format!("request --status {}/status", run.display())
    );

    let helper = fake_helper(&scratch, "report ready true true \"\"\nexit 0\n");
    assert!(helper.check_permissions(&run).unwrap().all_allowed());

    let helper = fake_helper(&scratch, "report failed null null \"Broken\"\nexit 1\n");
    assert_eq!(
        helper.check_permissions(&run).unwrap_err(),
        StartError::Failed("Broken".into())
    );
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn the_end_reason_is_what_a_finished_helper_said() {
    let scratch = scratch("reason");
    let run = scratch.join("run");
    assert_eq!(ended_reason(&run), None);
    let mut status = Status::new(State::Ended, 5);
    status.write(&run.join(STATUS)).unwrap();
    assert_eq!(ended_reason(&run), None);
    status.message = "Screen Recording was turned off.".into();
    status.write(&run.join(STATUS)).unwrap();
    assert_eq!(
        ended_reason(&run).as_deref(),
        Some("Screen Recording was turned off.")
    );
    status.state = State::Ready;
    status.write(&run.join(STATUS)).unwrap();
    assert_eq!(ended_reason(&run), None);
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn the_private_folder_is_ours_alone_and_fresh() {
    let name = format!("atur-private-{}", std::process::id());
    let folder = private_folder(&name).unwrap();
    assert_eq!(
        fs::metadata(&folder).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::write(folder.join("stale"), b"x").unwrap();
    assert_eq!(private_folder(&name).unwrap(), folder);
    assert!(!folder.join("stale").exists());
    fs::remove_dir_all(&folder).unwrap();

    // A link in the way is replaced, and what it pointed to is left alone.
    let target = scratch("link-target");
    std::os::unix::fs::symlink(&target, &folder).unwrap();
    let created = private_folder(&name).unwrap();
    assert!(fs::symlink_metadata(&created).unwrap().is_dir());
    assert!(target.exists());
    fs::remove_dir_all(&created).unwrap();
    fs::remove_dir_all(&target).unwrap();
}
