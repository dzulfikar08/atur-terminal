//! The Mac source and the hub, against stand-ins for `atur-term-screen`: shell
//! scripts that write status files, and a thread of the test that plays the
//! program's socket.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::screen::Hub;
use crate::screen::tests::{Collected, eventually};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// What every stand-in starts with: it notes its arguments and process number
/// in the folder of its status file and can write a status.
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

/// Waits for the socket, which the test provides, then reports ready.
const READY_WHEN_LISTENING: &str =
    "while [ ! -S \"$socket\" ]; do sleep 0.02; done\nreport ready true true \"\"\n";

/// Reports that the program ended once the test creates `end` in its folder.
const ENDS_ON_REQUEST: &str = "while [ ! -e \"$folder/end\" ]; do sleep 0.02; done\nreport ended true true \"Screen recording was turned off.\"\nexit 0\n";

fn scratch() -> PathBuf {
    let folder = std::env::temp_dir().join(format!(
        "atur-source-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&folder);
    fs::create_dir_all(&folder).unwrap();
    folder
}

fn fake_helper(scratch: &Path, body: &str) -> HelperApp {
    let path = scratch.join("helper.sh");
    fs::write(&path, format!("{PRELUDE}{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    HelperApp::from_executable(path)
}

fn alive(pid: u32) -> bool {
    rustix::process::Pid::from_raw(i32::try_from(pid).unwrap())
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

fn pid_of(source: &Source) -> u32 {
    source.running().as_ref().unwrap().pid()
}

/// Where a source's private folder is made.
fn folder_of(source: &Source) -> PathBuf {
    std::env::temp_dir().join(&source.folder)
}

/// The program's socket, played by the test: waits until the stand-in has
/// started in `folder` (it notes its arguments first, which is after the
/// launcher cleared the folder), listens on `folder/s` and hands every
/// connection to `serve`. Dropping it stops the listening.
struct Listening {
    done: Arc<AtomicBool>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
    }
}

fn listen(folder: PathBuf, serve: impl Fn(UnixStream) + Send + Sync + 'static) -> Listening {
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    let serve = Arc::new(serve);
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !folder.join("args").exists() {
            if flag.load(Ordering::Acquire) || Instant::now() >= deadline {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let Ok(listener) = UnixListener::bind(folder.join("s")) else {
            return;
        };
        let _ = listener.set_nonblocking(true);
        while !flag.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let serve = Arc::clone(&serve);
                    thread::spawn(move || {
                        let _ = stream.set_nonblocking(false);
                        serve(stream);
                    });
                }
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
    });
    Listening { done }
}

/// Greets with an RFB version line and echoes until told to hang up.
fn echo(hang_up: Arc<AtomicBool>) -> impl Fn(UnixStream) + Send + Sync + 'static {
    move |mut stream| {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
        let _ = stream.write_all(b"RFB 003.008\n");
        let mut buffer = [0_u8; 256];
        loop {
            if hang_up.load(Ordering::Acquire) {
                return;
            }
            match stream.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => {
                    if stream.write_all(&buffer[..count]).is_err() {
                        return;
                    }
                }
                Err(_) => {}
            }
        }
    }
}

struct Shared(Arc<Source>);

impl Capture for Shared {
    fn start(&self) -> Result<Endpoint, ServerError> {
        self.0.start()
    }

    fn stop(&self) {
        self.0.stop();
    }

    fn failed_to_serve(&self) {
        self.0.failed_to_serve();
    }

    fn ended_reason(&self) -> Option<String> {
        self.0.ended_reason()
    }
}

fn hub(source: &Arc<Source>, grace: Duration) -> Arc<Hub> {
    Arc::new(Hub::new(Box::new(Shared(Arc::clone(source))), grace))
}

#[test]
fn a_helper_without_permission_is_explained_with_the_mac_and_the_pane() {
    let scratch = scratch();
    let source = Source::with_helper(fake_helper(
        &scratch,
        "report needs-permission false null \"Allow it\"\nexit 0\n",
    ));
    let error = source.start().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unsupported);
    assert!(error.message().contains(&computer_name()), "{error}");
    assert!(
        error.message().contains("Screen & System Audio Recording"),
        "{error}"
    );
    assert!(
        error.message().contains("turn on Atur Terminal Screen."),
        "{error}"
    );
    assert!(
        error.message().ends_with("choose Retry connection."),
        "{error}"
    );
    assert!(source.running().is_none());
    assert!(!folder_of(&source).exists());
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_helper_that_fails_to_start_is_reported_as_a_failed_spawn() {
    let scratch = scratch();
    let source = Source::with_helper(fake_helper(
        &scratch,
        "report failed null null \"No display is attached\"\nexit 1\n",
    ));
    let error = source.start().unwrap_err();
    assert_eq!(error.code(), ErrorCode::SpawnFailed);
    assert_eq!(error.message(), "No display is attached");
    assert!(!folder_of(&source).exists());
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_ready_helper_is_dialled_on_a_unix_socket_and_reused_until_it_is_stopped() {
    let scratch = scratch();
    let source = Source::with_helper(fake_helper(
        &scratch,
        "report ready true true \"\"\nexec sleep 30\n",
    ));
    let Endpoint::Unix(socket) = source.start().unwrap() else {
        panic!("not a Unix socket");
    };
    assert_eq!(socket, folder_of(&source).join("s"));
    let pid = pid_of(&source);
    assert!(alive(pid));
    assert_eq!(source.start().unwrap(), Endpoint::Unix(socket));
    assert_eq!(pid_of(&source), pid);
    source.stop();
    assert!(!alive(pid));
    assert!(source.running().is_none());
    assert!(!folder_of(&source).exists());
    source.stop();
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_helper_that_died_is_started_again() {
    let scratch = scratch();
    let source = Source::with_helper(fake_helper(
        &scratch,
        "report ready true true \"\"\nexec sleep 30\n",
    ));
    source.start().unwrap();
    let first = pid_of(&source);
    let pid = rustix::process::Pid::from_raw(i32::try_from(first).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    assert!(eventually(|| {
        source
            .running()
            .as_mut()
            .is_some_and(|running| !running.is_running())
    }));
    source.start().unwrap();
    assert_ne!(pid_of(&source), first);
    source.stop();
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn the_end_reason_waits_for_a_helper_that_is_still_saying_it() {
    let scratch = scratch();
    let nothing = Source::with_helper(fake_helper(&scratch, "exit 0\n"));
    assert_eq!(nothing.ended_reason(), None);
    let source = Source::with_helper(fake_helper(
        &scratch,
        &format!("report ready true true \"\"\n{ENDS_ON_REQUEST}"),
    ));
    source.start().unwrap();
    // Still serving: nothing to report once the wait is over.
    assert_eq!(source.ended_reason(), None);
    fs::write(folder_of(&source).join("end"), b"").unwrap();
    assert_eq!(
        source.ended_reason().as_deref(),
        Some("Screen recording was turned off.")
    );
    source.stop();
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn options_come_from_the_environment_and_what_is_unreadable_is_left_out() {
    let given = |pairs: &[(&'static str, &'static str)]| {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        options(move |name| map.get(name).map(|value| (*value).to_owned()))
    };
    let all = given(&[
        ("ATUR_TERM_SCREEN_FPS", "30"),
        ("ATUR_TERM_SCREEN_MAX_EDGE", " 1920 "),
        ("ATUR_TERM_SCREEN_KEYS", "literal"),
        ("ATUR_TERM_SCREEN_SYNTHETIC", "640x360"),
    ]);
    assert_eq!(all.fps, Some(30));
    assert_eq!(all.max_edge, Some(1920));
    assert_eq!(all.keys, Some(Keys::Literal));
    assert_eq!(all.synthetic, Some((640, 360)));
    let broken = given(&[
        ("ATUR_TERM_SCREEN_FPS", "0"),
        ("ATUR_TERM_SCREEN_MAX_EDGE", "wide"),
        ("ATUR_TERM_SCREEN_KEYS", "shout"),
        ("ATUR_TERM_SCREEN_SYNTHETIC", "640x"),
    ]);
    assert_eq!(broken, ServeOptions::default());
    assert_eq!(given(&[]), ServeOptions::default());
}

#[test]
fn viewers_reach_the_helper_through_the_hub_and_it_stops_after_the_last_one() {
    let scratch = scratch();
    let source = Arc::new(Source::with_helper(fake_helper(
        &scratch,
        &format!("{READY_WHEN_LISTENING}exec sleep 30\n"),
    )));
    let _listening = listen(folder_of(&source), echo(Arc::new(AtomicBool::new(false))));
    let hub = hub(&source, Duration::from_millis(50));
    let collected = Arc::new(Collected::default());
    let viewer = hub.connect(collected.clone()).expect("viewer connects");
    assert!(collected.wait_bytes(b"RFB 003.008\n"));
    assert!(viewer.write(b"ping".to_vec()));
    assert!(collected.wait_bytes(b"ping"));
    let pid = pid_of(&source);
    assert!(alive(pid));
    drop(viewer);
    assert!(eventually(|| source.running().is_none()));
    assert!(eventually(|| !alive(pid)));
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_stream_the_helper_ended_carries_the_reason_it_wrote() {
    let scratch = scratch();
    let source = Arc::new(Source::with_helper(fake_helper(
        &scratch,
        &format!("{READY_WHEN_LISTENING}{ENDS_ON_REQUEST}"),
    )));
    let hang_up = Arc::new(AtomicBool::new(false));
    let _listening = listen(folder_of(&source), echo(Arc::clone(&hang_up)));
    let hub = hub(&source, Duration::from_millis(50));
    let collected = Arc::new(Collected::default());
    let viewer = hub.connect(collected.clone()).expect("viewer connects");
    assert!(collected.wait_bytes(b"RFB 003.008\n"));
    fs::write(folder_of(&source).join("end"), b"").unwrap();
    hang_up.store(true, Ordering::Release);
    assert_eq!(
        collected.wait_finished().as_deref(),
        Some("Screen recording was turned off.")
    );
    drop(viewer);
    assert!(eventually(|| source.running().is_none()));
    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn a_helper_that_hangs_up_before_sending_anything_still_says_why() {
    let scratch = scratch();
    let source = Arc::new(Source::with_helper(fake_helper(
        &scratch,
        &format!("{READY_WHEN_LISTENING}{ENDS_ON_REQUEST}"),
    )));
    let folder = folder_of(&source);
    let ending = folder.clone();
    let _listening = listen(folder, move |stream| {
        let _ = fs::write(ending.join("end"), b"");
        drop(stream);
    });
    let hub = hub(&source, Duration::from_millis(50));
    let collected = Arc::new(Collected::default());
    let viewer = hub.connect(collected.clone()).expect("viewer connects");
    assert_eq!(
        collected.wait_finished().as_deref(),
        Some("Screen recording was turned off.")
    );
    drop(viewer);
    assert!(eventually(|| source.running().is_none()));
    fs::remove_dir_all(&scratch).unwrap();
}
