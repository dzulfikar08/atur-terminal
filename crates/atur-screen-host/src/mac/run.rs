//! The program: modes `serve`, `synthetic`, `check` and `request`.
//!
//! The main thread runs the application's run loop, which `ScreenCaptureKit` and
//! the permission prompts need; the work happens on threads of its own and ends
//! with `process::exit`.
#![allow(
    unsafe_code,
    reason = "kill(2) is used to see whether the server that started this program is alive"
)]

use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use atur_screen_source::status::{State, Status};

use super::capture::{self, Capture, StartError, Stopped};
use super::display::{self, Display};
use super::host::MacHost;
use super::input::{self, Injector};
use crate::options::{Mode, Serve};
use crate::serve::{Broadcast, Host, Server};
use crate::synthetic;

/// How often the program looks at its surroundings.
const WATCH: Duration = Duration::from_secs(1);
/// How long `request` stays around, so that the system's prompts have appeared
/// before the process that asked for them is gone.
const PROMPT_GRACE: Duration = Duration::from_secs(12);
const APP_NAME: &str = "Atur Terminal Screen";

/// Runs the program for `mode`; never returns.
pub fn main(mode: Mode) -> ! {
    // The test pattern and the permission check need neither a window server
    // connection nor a run loop.
    let needs_application = matches!(mode, Mode::Serve(_) | Mode::Request { .. });
    if needs_application {
        capture::init_application();
    }
    // The keyboard layout can only be read on the main thread.
    let injector = match &mode {
        Mode::Serve(serve) => Some(Injector::new(serve.keys)),
        _ => None,
    };
    let worker = thread::Builder::new()
        .name("atur-screen-main".into())
        .spawn(move || {
            let code = match mode {
                Mode::Check { status } => check(&status),
                Mode::Request { status } => request(&status),
                Mode::Serve(serve) => serve_screen(&serve, injector),
                Mode::Synthetic(serve, size) => serve_test_pattern(&serve, size),
            };
            process::exit(code);
        });
    if let Err(error) = worker {
        log::error!("cannot start the worker thread: {error}");
        process::exit(1);
    }
    if needs_application {
        capture::run_application();
    }
    loop {
        thread::park();
    }
}

fn permission_status(state: State, recording: bool, accessibility: bool) -> Status {
    let mut status = Status::new(state, process::id());
    status.screen_recording = Some(recording);
    status.accessibility = Some(accessibility);
    status
}

fn write(path: &Path, status: &Status) {
    if let Err(error) = status.write(path) {
        log::warn!("cannot write the status file {}: {error}", path.display());
    }
}

/// `check`: reports the permissions without asking.
fn check(path: &Path) -> i32 {
    let recording = display::screen_recording_allowed();
    let accessibility = input::accessibility_trusted(false);
    let state = if recording {
        State::Ready
    } else {
        State::NeedsPermission
    };
    write(path, &permission_status(state, recording, accessibility));
    0
}

/// `request`: asks macOS for both permissions and reports what it has now.
fn request(path: &Path) -> i32 {
    let recording = display::request_screen_recording();
    let accessibility = input::accessibility_trusted(true);
    let state = if recording {
        State::Ready
    } else {
        State::NeedsPermission
    };
    write(path, &permission_status(state, recording, accessibility));
    thread::sleep(PROMPT_GRACE);
    0
}

fn needs_permission(path: &Path, accessibility: bool) -> i32 {
    let mut status = permission_status(State::NeedsPermission, false, accessibility);
    status.message = format!(
        "Screen recording is not allowed for {APP_NAME}. Turn it on in System Settings → \
         Privacy & Security → Screen & System Audio Recording."
    );
    write(path, &status);
    // The prompt the system just showed belongs to this process: give it time.
    thread::sleep(PROMPT_GRACE);
    3
}

fn failed(path: &Path, message: impl Into<String>) -> i32 {
    let mut status = Status::new(State::Failed, process::id());
    status.message = message.into();
    write(path, &status);
    1
}

fn computer_name() -> String {
    let mut buffer = [0_u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL terminates it on success.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len() - 1) };
    if result == 0 {
        let end = buffer.iter().position(|&byte| byte == 0).unwrap_or(0);
        let name = String::from_utf8_lossy(&buffer[..end]).into_owned();
        if !name.is_empty() {
            return name;
        }
    }
    "Mac".to_owned()
}

fn desktop_name(serve: &Serve, accessibility: bool) -> String {
    let base = serve.name.clone().unwrap_or_else(computer_name);
    if serve.view_only {
        format!("{base} (view only)")
    } else if accessibility {
        base
    } else {
        format!("{base} (view only: allow {APP_NAME} under Privacy & Security → Accessibility)")
    }
}

/// Whether process `pid` still exists.
fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists and can be signalled.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The shared screen, until the server that started this program is gone.
fn serve_screen(serve: &Serve, injector: Option<Injector>) -> i32 {
    write(&serve.status, &Status::new(State::Starting, process::id()));
    let mut accessibility = input::accessibility_trusted(false);
    if !display::screen_recording_allowed() {
        // First visit: ask for both permissions at once.
        let _ = display::request_screen_recording();
        if !accessibility {
            accessibility = input::accessibility_trusted(true);
        }
        return needs_permission(&serve.status, accessibility);
    }
    let Some(shown) = Display::main() else {
        return failed(
            &serve.status,
            "This Mac has no display to share. Someone has to be logged in on its screen.",
        );
    };
    let size = shown.picture_size(serve.max_edge);
    let broadcast = Broadcast::new(size);
    broadcast.set_geometry(shown.geometry(size));
    let host = Arc::new(MacHost::new(
        desktop_name(serve, accessibility),
        serve.fps,
        injector,
        accessibility,
        Arc::clone(&broadcast),
    ));
    let status_path = serve.status.clone();
    let stopped = move |stopped: Stopped| end_of_capture(&status_path, &stopped);
    let capture = match Capture::start(shown.id, size, serve.fps, Arc::clone(&broadcast), stopped) {
        Ok(capture) => capture,
        Err(StartError::Permission(_)) => return needs_permission(&serve.status, accessibility),
        Err(StartError::Failed(message)) => return failed(&serve.status, message),
    };
    let dyn_host: Arc<dyn Host> = Arc::clone(&host);
    let server = match Server::bind(
        &serve.socket,
        dyn_host,
        Arc::clone(&broadcast),
        serve.view_only,
    ) {
        Ok(server) => server,
        Err(error) => {
            return failed(&serve.status, format!("Cannot listen for viewers: {error}"));
        }
    };
    let mut ready = permission_status(State::Ready, true, accessibility);
    ready.width = size.0;
    ready.height = size.1;
    if !accessibility {
        ready.message = format!(
            "Mouse and keyboard are off: allow {APP_NAME} in System Settings → \
             Privacy & Security → Accessibility."
        );
    }
    write(&serve.status, &ready);
    if let Err(error) = thread::Builder::new()
        .name("atur-screen-accept".into())
        .spawn(move || server.run())
    {
        return failed(&serve.status, format!("Cannot start the server: {error}"));
    }
    watch(
        serve,
        &broadcast,
        &host,
        capture,
        shown,
        size,
        accessibility,
    )
}

/// Looks at the surroundings once a second until this program should end.
fn watch(
    serve: &Serve,
    broadcast: &Arc<Broadcast>,
    host: &Arc<MacHost>,
    capture: Capture,
    mut shown: Display,
    mut size: (u32, u32),
    mut accessibility: bool,
) -> i32 {
    let folder: Option<PathBuf> = serve.status.parent().map(Path::to_path_buf);
    let mut capture = Some(capture);
    loop {
        thread::sleep(WATCH);
        if !alive(serve.owner) || folder.as_deref().is_some_and(|folder| !folder.exists()) {
            drop(capture.take());
            broadcast.close();
            return 0;
        }
        let trusted = input::accessibility_trusted(false);
        if trusted != accessibility {
            accessibility = trusted;
            host.set_trusted(trusted);
            let mut status = permission_status(State::Ready, true, trusted);
            status.width = size.0;
            status.height = size.1;
            write(&serve.status, &status);
        }
        let Some(now) = Display::main() else {
            continue;
        };
        if now == shown {
            continue;
        }
        // The resolution or arrangement changed: start over at the new size.
        drop(capture.take());
        let new_size = now.picture_size(serve.max_edge);
        broadcast.set_geometry(now.geometry(new_size));
        broadcast.set_size(new_size);
        let status_path = serve.status.clone();
        match Capture::start(
            now.id,
            new_size,
            serve.fps,
            Arc::clone(broadcast),
            move |stopped: Stopped| end_of_capture(&status_path, &stopped),
        ) {
            Ok(restarted) => {
                capture = Some(restarted);
                shown = now;
                size = new_size;
            }
            Err(StartError::Permission(_)) => {
                return needs_permission(&serve.status, accessibility);
            }
            Err(StartError::Failed(message)) => return failed(&serve.status, message),
        }
    }
}

/// The capture stopped by itself: say why, then end the program.
fn end_of_capture(path: &Path, stopped: &Stopped) {
    let mut status = Status::new(State::Ended, process::id());
    status.message = if stopped.permission {
        format!(
            "Screen recording was turned off for {APP_NAME}. Turn it on in System Settings → \
             Privacy & Security → Screen & System Audio Recording."
        )
    } else {
        format!("The screen capture stopped: {}", stopped.message)
    };
    write(path, &status);
    process::exit(2);
}

/// The test pattern, served like a screen but without permissions or display.
fn serve_test_pattern(serve: &Serve, size: (u32, u32)) -> i32 {
    write(&serve.status, &Status::new(State::Starting, process::id()));
    let broadcast = Broadcast::new(size);
    broadcast.set_geometry(Display::main().map_or_else(
        || crate::geometry::Geometry {
            origin_x: 0.0,
            origin_y: 0.0,
            points_width: f64::from(size.0),
            points_height: f64::from(size.1),
            pixels_width: size.0,
            pixels_height: size.1,
        },
        |shown| shown.geometry(size),
    ));
    let host = Arc::new(MacHost::new(
        serve.name.clone().unwrap_or_else(|| "Test pattern".into()),
        serve.fps,
        None,
        false,
        Arc::clone(&broadcast),
    ));
    let dyn_host: Arc<dyn Host> = host;
    let server = match Server::bind(
        &serve.socket,
        dyn_host,
        Arc::clone(&broadcast),
        serve.view_only,
    ) {
        Ok(server) => server,
        Err(error) => {
            return failed(&serve.status, format!("Cannot listen for viewers: {error}"));
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    {
        let broadcast = Arc::clone(&broadcast);
        let stop = Arc::clone(&stop);
        if thread::Builder::new()
            .name("atur-screen-pattern".into())
            .spawn(move || synthetic::run(&broadcast, size, &stop))
            .is_err()
        {
            return failed(&serve.status, "Cannot start the test pattern.");
        }
    }
    let mut ready = permission_status(State::Ready, true, false);
    ready.width = size.0;
    ready.height = size.1;
    write(&serve.status, &ready);
    let owner = serve.owner;
    let status_path = serve.status.clone();
    if thread::Builder::new()
        .name("atur-screen-accept".into())
        .spawn(move || server.run())
        .is_err()
    {
        return failed(&status_path, "Cannot start the server.");
    }
    while alive(owner) {
        thread::sleep(WATCH);
    }
    0
}
