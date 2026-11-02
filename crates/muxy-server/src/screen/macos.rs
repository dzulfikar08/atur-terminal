//! Shares a Mac's screen through `atur-term-screen`, the program that is to a
//! Mac what `wayvnc` is to a Wayland desktop.
//!
//! macOS holds the application a process belongs to, not the process, to the
//! Screen Recording and Accessibility permissions, and a server started over
//! ssh belongs to `sshd`. The program is therefore a small application inside
//! the app bundle, started through Launch Services (see
//! [`atur_screen_source::launch`]), and the server only dials the private
//! socket it listens on, as it dials `wayvnc`'s. The module builds on every
//! Unix so that the hub path can be tested everywhere; only a Mac starts it.

use std::fs;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use atur_screen_source::launch::{self, HelperApp, Keys, Running, ServeOptions, StartError};
use muxy_protocol::ErrorCode;

use super::{Capture, Endpoint};
use crate::ServerError;

/// Name of the private folder, followed by the server's process number.
const FOLDER: &str = "atur-screen-";

/// How long a stream that ended unasked waits for the program to say why: it
/// writes its reason and closes its sockets, and the viewer may see the
/// second before the first.
const REASON_WAIT: Duration = Duration::from_millis(500);
const REASON_POLL: Duration = Duration::from_millis(20);

pub(super) struct Source {
    /// The program to run; found when first needed when `None`.
    helper: Option<HelperApp>,
    /// Name of the private folder in the temporary directory.
    folder: String,
    options: ServeOptions,
    running: Mutex<Option<Running>>,
}

impl Source {
    /// The program is found, and the options are read from the environment
    /// (`ATUR_TERM_SCREEN_KEYS`, `_FPS`, `_MAX_EDGE`, `_SYNTHETIC`).
    pub(super) fn from_environment() -> Self {
        Self {
            helper: None,
            folder: format!("{FOLDER}{}", std::process::id()),
            options: options(|name| std::env::var(name).ok()),
            running: Mutex::new(None),
        }
    }

    /// A source that runs `helper`, with a folder of its own so that sources
    /// in one test process do not meet.
    #[cfg(test)]
    fn with_helper(helper: HelperApp) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        Self {
            helper: Some(helper),
            folder: format!(
                "{FOLDER}{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
            options: ServeOptions::default(),
            running: Mutex::new(None),
        }
    }

    fn running(&self) -> MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn helper(&self) -> Result<HelperApp, ServerError> {
        match &self.helper {
            Some(helper) => Ok(helper.clone()),
            None => HelperApp::find()
                .map_err(|error| ServerError::new(ErrorCode::Unsupported, error.to_string())),
        }
    }
}

impl Capture for Source {
    fn start(&self) -> Result<Endpoint, ServerError> {
        let mut running = self.running();
        if let Some(current) = running.as_mut() {
            if current.is_running() {
                return Ok(Endpoint::Unix(current.socket().to_path_buf()));
            }
            if let Some(gone) = running.take() {
                gone.stop();
            }
        }
        let helper = self.helper()?;
        let folder = launch::private_folder(&self.folder).map_err(|error| {
            ServerError::spawn_failed(format!(
                "The folder for screen sharing could not be created: {error}."
            ))
        })?;
        match helper.start(&folder, &self.options) {
            Ok(started) => {
                let endpoint = Endpoint::Unix(started.socket().to_path_buf());
                *running = Some(started);
                Ok(endpoint)
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&folder);
                Err(explain(&helper, error))
            }
        }
    }

    fn stop(&self) {
        let running = self.running().take();
        if let Some(running) = running {
            running.stop();
        }
    }

    fn failed_to_serve(&self) {
        self.stop();
    }

    fn ended_reason(&self) -> Option<String> {
        let deadline = Instant::now() + REASON_WAIT;
        loop {
            let (reason, serving) = {
                let mut guard = self.running();
                let running = guard.as_mut()?;
                (launch::ended_reason(running.folder()), running.is_running())
            };
            if reason.is_some() || !serving || Instant::now() >= deadline {
                return reason;
            }
            thread::sleep(REASON_POLL);
        }
    }
}

/// What the user is told when the program could not be started.
fn explain(helper: &HelperApp, error: StartError) -> ServerError {
    match error {
        StartError::NeedsPermission(message) => {
            log::info!("the screen sharing program may not record the screen: {message}");
            ServerError::new(
                ErrorCode::Unsupported,
                launch::permission_message(&computer_name(), helper.display_name()),
            )
        }
        StartError::Failed(message) => ServerError::new(ErrorCode::SpawnFailed, message),
    }
}

/// The Mac's name as System Settings shows it, else its network name.
fn computer_name() -> String {
    Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            Some(
                rustix::system::uname()
                    .nodename()
                    .to_string_lossy()
                    .trim()
                    .to_owned(),
            )
            .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "this Mac".to_owned())
}

/// The options the environment asks for; what is missing or unreadable is left
/// to the program's own defaults.
fn options(get: impl Fn(&str) -> Option<String>) -> ServeOptions {
    let number = |name: &str| {
        get(name)
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|value| *value > 0)
    };
    ServeOptions {
        fps: number("ATUR_TERM_SCREEN_FPS"),
        max_edge: number("ATUR_TERM_SCREEN_MAX_EDGE"),
        keys: get("ATUR_TERM_SCREEN_KEYS").and_then(|value| Keys::parse(value.trim())),
        synthetic: get("ATUR_TERM_SCREEN_SYNTHETIC").and_then(|value| size(&value)),
        ..ServeOptions::default()
    }
}

/// `WIDTHxHEIGHT`.
fn size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.trim().split_once('x')?;
    let edge = |text: &str| text.parse::<u32>().ok().filter(|edge| *edge > 0);
    Some((edge(width)?, edge(height)?))
}

#[cfg(test)]
mod tests;
