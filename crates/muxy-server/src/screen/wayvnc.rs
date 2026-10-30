//! Runs `wayvnc` against the user's Wayland desktop.
//!
//! It is started with an address that only this user can reach: a socket in a
//! private folder of the runtime directory. A `wayvnc` too old to listen on a
//! socket gets 127.0.0.1 and a free port instead, and one that cannot use the
//! GPU is started without it; what failed is remembered, so a later start goes
//! straight to what works.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use muxy_protocol::ErrorCode;

use super::{Capture, Endpoint, Stream};
use crate::ServerError;

/// How long a freshly started `wayvnc` may take to accept connections.
const READY: Duration = Duration::from_secs(6);
const POLL: Duration = Duration::from_millis(40);
/// Name of the private folder, followed by the server's process number.
const FOLDER: &str = "atur-term-screen-";

/// The frame rate ceiling passed to `wayvnc`, whose own default is 30.
const MAX_FPS: &str = "60";

pub(super) struct Wayvnc {
    /// The program to run; `ATUR_TERM_WAYVNC` replaces the default for testing.
    program: Option<PathBuf>,
    gpu_allowed: bool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    running: Option<Running>,
    avoid_gpu: bool,
    avoid_socket: bool,
}

struct Running {
    child: Child,
    endpoint: Endpoint,
    folder: PathBuf,
    gpu: bool,
    socket: bool,
    stderr: Arc<Mutex<String>>,
}

impl Wayvnc {
    pub(super) fn from_environment() -> Self {
        Self {
            program: std::env::var_os("ATUR_TERM_WAYVNC").map(PathBuf::from),
            gpu_allowed: !std::env::var_os("ATUR_TERM_SCREEN_GPU")
                .is_some_and(|value| value == "0"),
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn program(&self) -> Result<PathBuf, ServerError> {
        if let Some(program) = &self.program {
            return Ok(program.clone());
        }
        find_on_path("wayvnc").ok_or_else(|| {
            ServerError::new(
                ErrorCode::Unsupported,
                "wayvnc is not installed on this computer. Install it (for example \
                 `pacman -S wayvnc` or `apt install wayvnc`) to mirror its screen.",
            )
        })
    }
}

impl Capture for Wayvnc {
    fn start(&self) -> Result<Endpoint, ServerError> {
        let mut state = self.state();
        if let Some(running) = &mut state.running {
            if matches!(running.child.try_wait(), Ok(None)) {
                return Ok(running.endpoint.clone());
            }
            let ended = state.running.take();
            if let Some(ended) = ended {
                ended.clean_up();
            }
        }
        let program = self.program()?;
        let desktop = Desktop::find().ok_or_else(|| {
            ServerError::new(
                ErrorCode::Unsupported,
                "No Wayland desktop session is running for this user on this computer. \
                 Log in to the desktop there first.",
            )
        })?;
        sweep_stale(&desktop.runtime);
        let mut last = String::from("wayvnc did not start");
        for (gpu, socket) in attempts(self.gpu_allowed, state.avoid_gpu, state.avoid_socket) {
            match launch(&program, &desktop, gpu, socket) {
                Ok(running) => {
                    let endpoint = running.endpoint.clone();
                    state.running = Some(running);
                    return Ok(endpoint);
                }
                Err(reason) => {
                    log::info!(
                        "wayvnc ({}, {}) did not start: {reason}",
                        if gpu { "gpu" } else { "no gpu" },
                        if socket { "socket" } else { "loopback" },
                    );
                    last = reason;
                }
            }
        }
        Err(ServerError::new(ErrorCode::SpawnFailed, last))
    }

    fn alive(&self) -> bool {
        self.state()
            .running
            .as_mut()
            .is_some_and(|running| matches!(running.child.try_wait(), Ok(None)))
    }

    fn stop(&self) {
        let running = self.state().running.take();
        if let Some(running) = running {
            running.clean_up();
        }
    }

    fn failed_to_serve(&self) {
        let mut state = self.state();
        if let Some(running) = state.running.take() {
            if running.gpu {
                state.avoid_gpu = true;
            } else if running.socket {
                state.avoid_socket = true;
            }
            running.clean_up();
        }
    }
}

impl Running {
    fn clean_up(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.folder);
    }

    fn tail(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .trim()
            .to_owned()
    }
}

/// The ways to start, best first, without those known not to work here.
fn attempts(gpu_allowed: bool, avoid_gpu: bool, avoid_socket: bool) -> Vec<(bool, bool)> {
    let mut attempts = Vec::new();
    for gpu in [true, false] {
        if gpu && (!gpu_allowed || avoid_gpu || !has_render_node()) {
            continue;
        }
        for socket in [true, false] {
            if socket && avoid_socket {
                continue;
            }
            attempts.push((gpu, socket));
        }
    }
    attempts
}

fn has_render_node() -> bool {
    fs::read_dir("/dev/dri").is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().as_bytes().starts_with(b"renderD"))
    })
}

/// The user's running Wayland desktop.
struct Desktop {
    runtime: PathBuf,
    display: OsString,
}

impl Desktop {
    fn find() -> Option<Self> {
        let runtime = runtime_dir();
        let named = std::env::var_os("WAYLAND_DISPLAY").filter(|name| !name.is_empty());
        if let Some(name) = named
            && is_socket(&runtime.join(&name))
        {
            return Some(Self {
                runtime,
                display: name,
            });
        }
        let mut found: Vec<(u32, OsString)> = fs::read_dir(&runtime)
            .ok()?
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let number = name.to_str()?.strip_prefix("wayland-")?.parse().ok()?;
                is_socket(&entry.path()).then_some((number, name))
            })
            .collect();
        found.sort();
        let (_, display) = found.into_iter().next()?;
        Some(Self { runtime, display })
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw())),
            PathBuf::from,
        )
}

fn is_socket(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/local/bin", "/usr/bin"].map(PathBuf::from))
        .filter(|folder| !folder.as_os_str().is_empty())
        .map(|folder| folder.join(program))
        .find(|file| {
            file.is_file() && rustix::fs::access(file, rustix::fs::Access::EXEC_OK).is_ok()
        })
}

fn launch(program: &Path, desktop: &Desktop, gpu: bool, socket: bool) -> Result<Running, String> {
    let folder = desktop
        .runtime
        .join(format!("{FOLDER}{}", std::process::id()));
    fs::create_dir_all(&desktop.runtime).map_err(|error| error.to_string())?;
    let _ = fs::remove_dir_all(&folder);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&folder)
        .map_err(|error| format!("create {}: {error}", folder.display()))?;
    let mut command = Command::new(program);
    command
        .args(["-r", "-L", "warning", "-f", MAX_FPS, "-S"])
        .arg(folder.join("control"));
    if gpu {
        command.arg("-g");
    }
    let endpoint = if socket {
        let path = folder.join("screen");
        let mut address = OsString::from("unix:");
        address.push(&path);
        command.arg(address);
        Endpoint::Unix(path)
    } else {
        let port = free_loopback_port().map_err(|error| error.to_string())?;
        command.arg("127.0.0.1").arg(port.to_string());
        Endpoint::Loopback(port)
    };
    let mut child = command
        .env("WAYLAND_DISPLAY", &desktop.display)
        .env("XDG_RUNTIME_DIR", &desktop.runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("start {}: {error}", program.display()))?;
    let _ = fs::write(folder.join("wayvnc.pid"), child.id().to_string());
    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(output) = child.stderr.take() {
        collect_tail(output, Arc::clone(&stderr));
    }
    let running = Running {
        child,
        endpoint,
        folder,
        gpu,
        socket,
        stderr,
    };
    wait_until_ready(running)
}

/// Waits until the capture accepts a connection, or ends or takes too long.
fn wait_until_ready(mut running: Running) -> Result<Running, String> {
    let deadline = Instant::now() + READY;
    loop {
        match running.child.try_wait() {
            Ok(Some(status)) => {
                let tail = running.tail();
                running.clean_up();
                return Err(if tail.is_empty() {
                    format!("wayvnc stopped at start ({status})")
                } else {
                    format!("wayvnc stopped at start: {tail}")
                });
            }
            Ok(None) => {}
            Err(error) => {
                running.clean_up();
                return Err(error.to_string());
            }
        }
        let ready = match &running.endpoint {
            Endpoint::Unix(path) => is_socket(path),
            Endpoint::Loopback(_) => Stream::connect(&running.endpoint).is_ok(),
        };
        if ready {
            return Ok(running);
        }
        if Instant::now() >= deadline {
            let tail = running.tail();
            running.clean_up();
            return Err(format!("wayvnc did not start listening in time {tail}")
                .trim()
                .to_owned());
        }
        thread::sleep(POLL);
    }
}

fn free_loopback_port() -> std::io::Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// Keeps the last couple of KiB `wayvnc` wrote to its error output.
fn collect_tail(mut output: impl Read + Send + 'static, tail: Arc<Mutex<String>>) {
    let _ = thread::Builder::new()
        .name("atur-screen-log".into())
        .spawn(move || {
            let mut buffer = [0_u8; 1024];
            while let Ok(count) = output.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let mut text = tail.lock().unwrap_or_else(PoisonError::into_inner);
                text.push_str(&String::from_utf8_lossy(&buffer[..count]));
                if text.len() > 2048 {
                    let mut cut = text.len() - 2048;
                    while !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    text.drain(..cut);
                }
            }
        });
}

/// Removes what a server that died left in the runtime folder: its private
/// folder, and the `wayvnc` it started.
fn sweep_stale(runtime: &Path) {
    let Ok(entries) = fs::read_dir(runtime) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(owner) = name
            .to_str()
            .and_then(|name| name.strip_prefix(FOLDER))
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if owner == std::process::id() || Path::new(&format!("/proc/{owner}")).exists() {
            continue;
        }
        let folder = entry.path();
        if let Ok(pid) = fs::read_to_string(folder.join("wayvnc.pid"))
            && let Ok(pid) = pid.trim().parse::<u32>()
            && fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|comm| comm.trim() == "wayvnc")
            && let Some(pid) = rustix::process::Pid::from_raw(pid.cast_signed())
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
        let _ = fs::remove_dir_all(folder);
    }
}
