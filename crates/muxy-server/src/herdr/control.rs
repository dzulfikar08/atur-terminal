//! A writable herdr terminal session: `herdr terminal session control`, the
//! documented bridge interface. It prints newline-delimited JSON frames of base64
//! ANSI and reads JSON commands.

use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use muxy_protocol::{MouseAction, MouseButton, MouseEvent, ScrollDirection, Size};
use muxy_terminal::CellSize;
use muxy_terminal::pty::PtyEvent;
use serde_json::{Value, json};

use super::api::Api;
use super::base64;
use super::panes::Panes;
use super::target::Target;
use crate::session::OwnerEvent;

/// How long a write waits for a reconnecting control stream.
const RECONNECT_WAIT: Duration = Duration::from_secs(3);
/// How long an unreachable herdr is waited for before its terminals count as gone.
const HERDR_RESTART_WAIT: Duration = Duration::from_secs(60);
const RECONNECT_DELAY: Duration = Duration::from_millis(300);
/// Control streams that die this often within `STORM_WINDOW` stop being retried.
const STORM_LIMIT: usize = 8;
const STORM_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(super) struct Launch {
    pub(super) bin: PathBuf,
    pub(super) socket: PathBuf,
}

#[derive(Debug)]
struct Slot {
    stdin: Option<ChildStdin>,
    child: Option<Child>,
}

#[derive(Debug)]
pub(super) struct Shared {
    launch: Launch,
    target: Arc<Target>,
    api: Arc<Api>,
    /// The size herdr was last asked for, packed as columns << 16 | rows. A frame of
    /// another size predates the last resize and would corrupt the owner's terminal.
    expected: AtomicU32,
    slot: Mutex<Slot>,
    connected: Condvar,
    /// The terminal no longer exists in herdr.
    pub(super) gone: AtomicBool,
    /// This server stopped using the terminal; the stream ends quietly.
    detached: AtomicBool,
}

#[derive(Clone, Debug)]
pub(super) struct Control {
    shared: Arc<Shared>,
}

fn pack(size: Size) -> u32 {
    u32::from(size.cols) << 16 | u32::from(size.rows)
}

impl Control {
    /// Spawns the control stream and starts delivering its frames to `sink`.
    pub(super) fn start(
        launch: Launch,
        api: Arc<Api>,
        target: Arc<Target>,
        size: Size,
        sink: Sender<OwnerEvent>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            launch,
            target,
            api,
            expected: AtomicU32::new(pack(size)),
            slot: Mutex::new(Slot {
                stdin: None,
                child: None,
            }),
            connected: Condvar::new(),
            gone: AtomicBool::new(false),
            detached: AtomicBool::new(false),
        });
        let first = shared.connect()?;
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name(format!("herdr-{}", shared.target.terminal()))
            .spawn(move || worker.run(first, &sink))?;
        Ok(Self { shared })
    }

    pub(super) fn shared(&self) -> &Shared {
        &self.shared
    }

    pub(super) fn resize(&self, size: Size, cell: CellSize) -> io::Result<()> {
        self.shared.expected.store(pack(size), Ordering::Release);
        self.send(&json!({
            "type": "terminal.resize",
            "cols": size.cols,
            "rows": size.rows,
            "cell_width_px": cell.width,
            "cell_height_px": cell.height,
        }))
    }

    pub(super) fn input(&self, bytes: &[u8]) -> io::Result<()> {
        self.send(&json!({"type": "terminal.input", "bytes": base64::encode(bytes)}))
    }

    pub(super) fn mouse(&self, event: &MouseEvent) -> io::Result<()> {
        let modifiers = u8::from(event.modifiers.shift)
            | u8::from(event.modifiers.ctrl) << 1
            | u8::from(event.modifiers.alt) << 2;
        let button = match event.button {
            Some(MouseButton::Middle) => "middle",
            Some(MouseButton::Right) => "right",
            _ => "left",
        };
        let command = match (event.action, event.scroll) {
            (MouseAction::Scroll, Some(direction)) => json!({
                "type": "terminal.scroll",
                "direction": if direction == ScrollDirection::Up { "up" } else { "down" },
                "lines": 1,
                "column": event.column,
                "row": event.row,
                "modifiers": modifiers,
            }),
            (MouseAction::Scroll, None) => return Ok(()),
            (action, _) => json!({
                "type": "terminal.mouse",
                "action": match action {
                    MouseAction::Press => "down",
                    MouseAction::Release => "up",
                    _ if event.button.is_some() => "drag",
                    _ => "move",
                },
                "button": button,
                "column": event.column,
                "row": event.row,
                "modifiers": modifiers,
            }),
        };
        self.send(&command)
    }

    fn send(&self, command: &Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(command).map_err(io::Error::other)?;
        line.push(b'\n');
        self.shared.write(&line)
    }

    /// Stops the stream without touching the terminal.
    pub(super) fn detach(&self) {
        self.shared.detached.store(true, Ordering::Release);
        let mut slot = self.shared.lock();
        if let Some(stdin) = slot.stdin.as_mut() {
            let _ = stdin.write_all(b"{\"type\":\"terminal.release\"}\n");
        }
        slot.stdin = None;
        if let Some(mut child) = slot.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.shared.connected.notify_all();
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Spawns a control stream that takes over the terminal at the expected size.
    fn connect(&self) -> io::Result<BufReader<std::process::ChildStdout>> {
        let expected = self.expected.load(Ordering::Acquire);
        let mut child = Command::new(&self.launch.bin)
            .args([
                "terminal",
                "session",
                "control",
                &self.target.terminal(),
                "--takeover",
            ])
            .args(["--cols", &(expected >> 16).to_string()])
            .args(["--rows", &(expected & 0xffff).to_string()])
            .env("HERDR_SOCKET_PATH", &self.launch.socket)
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("herdr control has no stdout"))?;
        let stdin = child.stdin.take();
        let mut slot = self.lock();
        slot.stdin = stdin;
        slot.child = Some(child);
        self.connected.notify_all();
        Ok(BufReader::with_capacity(64 * 1024, stdout))
    }

    fn write(&self, line: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + RECONNECT_WAIT;
        let mut slot = self.lock();
        loop {
            if let Some(stdin) = slot.stdin.as_mut() {
                let result = stdin.write_all(line).and_then(|()| stdin.flush());
                if result.is_err() {
                    slot.stdin = None;
                }
                return result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero()
                || self.gone.load(Ordering::Acquire)
                || self.detached.load(Ordering::Acquire)
            {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            slot = self
                .connected
                .wait_timeout(slot, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Whether herdr still lists the terminal. A herdr that does not answer is
    /// restarting or handing its terminals to a new server, so it is waited for
    /// rather than taken as the terminal's end.
    fn alive(&self) -> bool {
        let deadline = Instant::now() + HERDR_RESTART_WAIT;
        loop {
            let listed = Panes::fetch(&self.api).map(|panes| self.target.locate(&panes).is_some());
            match listed {
                Some(listed) => return listed,
                None if Instant::now() >= deadline || self.detached.load(Ordering::Acquire) => {
                    return false;
                }
                None => thread::sleep(RECONNECT_DELAY),
            }
        }
    }

    fn run(&self, first: BufReader<std::process::ChildStdout>, sink: &Sender<OwnerEvent>) {
        let mut stream = Some(first);
        let mut deaths: Vec<Instant> = Vec::new();
        loop {
            if let Some(stdout) = stream.take() {
                if !self.pump(stdout, sink) {
                    return;
                }
                self.reap();
            }
            if self.detached.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            deaths.retain(|death| now.duration_since(*death) < STORM_WINDOW);
            deaths.push(now);
            if !self.alive() || deaths.len() > STORM_LIMIT {
                log::info!("herdr terminal {} is gone", self.target.terminal());
                self.gone.store(true, Ordering::Release);
                self.connected.notify_all();
                let _ = sink.send(PtyEvent::Closed.into());
                return;
            }
            thread::sleep(RECONNECT_DELAY);
            match self.connect() {
                Ok(stdout) => stream = Some(stdout),
                Err(error) => {
                    log::error!(
                        "herdr control for {} did not start: {error}",
                        self.target.terminal()
                    );
                    self.gone.store(true, Ordering::Release);
                    let _ = sink.send(PtyEvent::Closed.into());
                    return;
                }
            }
        }
    }

    fn reap(&self) {
        let mut slot = self.lock();
        slot.stdin = None;
        if let Some(mut child) = slot.child.take() {
            let _ = child.wait();
        }
    }

    /// Forwards frames until the stream ends. False when the owner is gone.
    fn pump(
        &self,
        mut stdout: BufReader<std::process::ChildStdout>,
        sink: &Sender<OwnerEvent>,
    ) -> bool {
        let mut line = String::new();
        loop {
            line.clear();
            match stdout.read_line(&mut line) {
                Ok(0) | Err(_) => return true,
                Ok(_) => {}
            }
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                log::warn!("herdr control sent invalid JSON");
                continue;
            };
            match record["type"].as_str() {
                Some("terminal.frame") => {
                    let size = record["width"].as_u64().zip(record["height"].as_u64());
                    let expected = self.expected.load(Ordering::Acquire);
                    if size != Some((u64::from(expected >> 16), u64::from(expected & 0xffff))) {
                        log::debug!("dropped a {size:?} frame; expected {expected:#x}");
                        continue;
                    }
                    let Some(bytes) = record["bytes"].as_str().and_then(base64::decode) else {
                        log::warn!("herdr control sent an unreadable frame");
                        continue;
                    };
                    if self.detached.load(Ordering::Acquire) {
                        return false;
                    }
                    if sink.send(PtyEvent::Output(bytes).into()).is_err() {
                        return false;
                    }
                }
                Some("terminal.closed") => return true,
                _ => {}
            }
        }
    }
}
