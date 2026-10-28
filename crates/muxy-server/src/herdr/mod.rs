//! Terminals owned by a herdr server.
//!
//! In this mode the server runs no PTYs. Each Muxy session is a herdr tab with
//! one pane in the workspace of its project; a control stream mirrors the pane's
//! screen into the session owner's terminal, and input, resize and mouse go back
//! through it. Projects, git, files, exec, settings and activity stay Muxy's.

mod api;
mod base64;
mod control;
mod store;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use muxy_protocol::{ErrorCode, ExitReason, MouseEvent, SessionId, SessionInfo, Size};
use muxy_terminal::CellSize;
use muxy_terminal::pty::ExitStatus;
use serde_json::{Value, json};

use api::Api;
use control::{Control, Launch};
use store::Store;

use crate::error::ServerError;
use crate::session::backing::Fault;
use crate::session::{Backing, OwnerEvent};

/// Where herdr lives; defaults come from the environment, like herdr's own CLI.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HerdrConfig {
    /// The herdr binary, `herdr` on `PATH` when unset.
    pub bin: Option<PathBuf>,
    /// The API socket, `$HERDR_SOCKET_PATH` or herdr's default when unset.
    pub socket: Option<PathBuf>,
}

impl HerdrConfig {
    fn bin(&self) -> PathBuf {
        self.bin.clone().unwrap_or_else(|| PathBuf::from("herdr"))
    }

    fn socket(&self) -> PathBuf {
        self.socket
            .clone()
            .or_else(|| std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from))
            .unwrap_or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::home_dir().map(|home| home.join(".config")))
                    .unwrap_or_else(|| PathBuf::from("/"))
                    .join("herdr")
                    .join("herdr.sock")
            })
    }
}

fn unavailable(error: impl std::fmt::Display) -> ServerError {
    ServerError::new(ErrorCode::SpawnFailed, format!("herdr: {error}"))
}

/// The label of a session's herdr tab; also how a session is found again after
/// herdr restarts and renumbers its terminals.
fn tab_label(session: SessionId) -> String {
    format!("muxy-{:08x}", session.get() >> 32)
}

#[derive(Debug)]
pub(crate) struct Herdr {
    api: Arc<Api>,
    config: HerdrConfig,
    store: Store,
}

/// What a new session needs from its project.
#[derive(Debug)]
pub(crate) struct NewTerminal<'a> {
    pub(crate) info: &'a SessionInfo,
    pub(crate) project_name: &'a str,
    pub(crate) directory: &'a Path,
    pub(crate) size: Size,
    pub(crate) environment: Vec<(String, String)>,
}

impl Herdr {
    pub(crate) fn open(config: &HerdrConfig, state: &Path) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            api: Arc::new(Api::new(config.socket())),
            config: config.clone(),
            store: Store::open(state)?,
        }))
    }

    fn launch(&self) -> Launch {
        Launch {
            bin: self.config.bin(),
            socket: self.api.socket().to_path_buf(),
        }
    }

    /// Checks that herdr answers.
    pub(crate) fn ping(&self) -> Result<(), ServerError> {
        self.api
            .call("ping", &json!({}))
            .map(drop)
            .map_err(unavailable)
    }

    /// Creates the session's tab, and its project's workspace if needed.
    pub(crate) fn create(
        self: &Arc<Self>,
        request: &NewTerminal<'_>,
    ) -> Result<HerdrBacking, ServerError> {
        let env: serde_json::Map<String, Value> = request
            .environment
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        let cwd = request.directory.to_string_lossy();
        let label = tab_label(request.info.id);
        let project = request.info.project;
        let existing = self
            .store
            .workspace(project)
            .filter(|workspace| self.workspace_exists(workspace));
        let reply = if let Some(workspace) = existing {
            self.api
                .call(
                    "tab.create",
                    &json!({"workspace_id": workspace, "cwd": cwd, "label": label, "env": env, "focus": false}),
                )
                .map_err(unavailable)?
        } else {
            let reply = self
                .api
                .call(
                    "workspace.create",
                    &json!({"cwd": cwd, "label": request.project_name, "env": env, "focus": false}),
                )
                .map_err(unavailable)?;
            if let Some(workspace) = reply["workspace"]["workspace_id"].as_str() {
                self.store.set_workspace(project, workspace);
            }
            if let Some(tab) = reply["tab"]["tab_id"].as_str() {
                let _ = self
                    .api
                    .call("tab.rename", &json!({"tab_id": tab, "label": label}));
            }
            reply
        };
        let terminal = reply["root_pane"]["terminal_id"]
            .as_str()
            .ok_or_else(|| unavailable("a new pane without a terminal id"))?
            .to_owned();
        self.store.set_session(request.info.id, project, &terminal);
        Ok(HerdrBacking::new(Arc::clone(self), terminal, request.size))
    }

    fn workspace_exists(&self, workspace: &str) -> bool {
        self.api
            .call("workspace.get", &json!({"workspace_id": workspace}))
            .is_ok()
    }

    /// The sessions this server created that herdr still runs, with their size.
    pub(crate) fn survivors(self: &Arc<Self>) -> Vec<(SessionId, HerdrBacking)> {
        let panes = match self.api.call("pane.list", &json!({})) {
            Ok(list) => list["panes"].as_array().cloned().unwrap_or_default(),
            Err(error) => {
                log::warn!("herdr sessions were not adopted: {error}");
                return Vec::new();
            }
        };
        let tabs = self
            .api
            .call("tab.list", &json!({}))
            .ok()
            .and_then(|list| list["tabs"].as_array().cloned())
            .unwrap_or_default();
        let mut found = Vec::new();
        for (session, entry) in self.store.sessions() {
            let by_terminal = panes
                .iter()
                .find(|pane| pane["terminal_id"].as_str() == Some(entry.terminal.as_str()));
            // herdr numbers terminals afresh after it restarts; the tab label survives.
            let by_label = || {
                let label = tab_label(session);
                let tab = tabs
                    .iter()
                    .find(|tab| tab["label"].as_str() == Some(label.as_str()))?;
                panes
                    .iter()
                    .find(|pane| pane["tab_id"].as_str() == tab["tab_id"].as_str())
            };
            let Some(pane) = by_terminal.or_else(by_label) else {
                continue;
            };
            let Some(terminal) = pane["terminal_id"].as_str() else {
                continue;
            };
            let size = pane["pane_id"]
                .as_str()
                .and_then(|pane| self.pane_size(pane))
                .unwrap_or(Size {
                    cols: 120,
                    rows: 40,
                });
            if terminal != entry.terminal {
                self.store.set_session(session, entry.project, terminal);
            }
            found.push((
                session,
                HerdrBacking::new(Arc::clone(self), terminal.to_owned(), size),
            ));
        }
        found
    }

    fn pane_size(&self, pane: &str) -> Option<Size> {
        let layout = self
            .api
            .call("pane.layout", &json!({"pane_id": pane}))
            .ok()?;
        let rect = layout["layout"]["panes"]
            .as_array()?
            .iter()
            .find(|candidate| candidate["pane_id"].as_str() == Some(pane))?["rect"]
            .clone();
        Some(Size {
            cols: u16::try_from(rect["width"].as_u64()?).ok()?,
            rows: u16::try_from(rect["height"].as_u64()?).ok()?,
        })
    }

    pub(crate) fn forget(&self, session: SessionId) {
        self.store.remove_session(session);
    }
}

struct ProcessInfo {
    shell: Option<u32>,
    foreground: Option<u32>,
    pane: Option<String>,
    fetched: Option<Instant>,
}

/// How long a foreground-process answer is reused. The owner asks on every tick.
const PROCESS_TTL: Duration = Duration::from_millis(250);

pub(crate) struct HerdrBacking {
    herdr: Arc<Herdr>,
    terminal: String,
    size: Size,
    cell: CellSize,
    control: OnceLock<Control>,
    process: Mutex<ProcessInfo>,
}

impl std::fmt::Debug for HerdrBacking {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HerdrBacking")
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl HerdrBacking {
    fn new(herdr: Arc<Herdr>, terminal: String, size: Size) -> Self {
        Self {
            herdr,
            terminal,
            size,
            cell: CellSize::default(),
            control: OnceLock::new(),
            process: Mutex::new(ProcessInfo {
                shell: None,
                foreground: None,
                pane: None,
                fetched: None,
            }),
        }
    }

    pub(crate) fn size(&self) -> Size {
        self.size
    }

    fn control(&self) -> io::Result<&Control> {
        self.control
            .get()
            .ok_or_else(|| io::Error::other("herdr terminal is not started"))
    }

    fn pane(&self) -> Option<String> {
        let list = self.herdr.api.call("pane.list", &json!({})).ok()?;
        list["panes"]
            .as_array()?
            .iter()
            .find(|pane| pane["terminal_id"].as_str() == Some(self.terminal.as_str()))?["pane_id"]
            .as_str()
            .map(str::to_owned)
    }

    fn refresh(&self, process: &mut ProcessInfo) {
        let info = loop {
            let Some(pane) = process.pane.clone().or_else(|| self.pane()) else {
                return;
            };
            match self
                .herdr
                .api
                .call("pane.process_info", &json!({"pane_id": pane}))
            {
                Ok(reply) => {
                    process.pane = Some(pane);
                    break reply;
                }
                // The pane was moved or renumbered: look it up once more.
                Err(error) if error.is_missing() && process.pane.take().is_some() => {}
                Err(_) => return,
            }
        };
        let info = &info["process_info"];
        process.shell = info["shell_pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok());
        process.foreground = info["foreground_process_group_id"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok());
        process.fetched = Some(Instant::now());
    }

    fn process(&self) -> (Option<u32>, Option<u32>) {
        let mut process = self.process.lock().unwrap_or_else(PoisonError::into_inner);
        if process.fetched.is_none_or(|at| at.elapsed() > PROCESS_TTL) {
            self.refresh(&mut process);
            process.fetched.get_or_insert_with(Instant::now);
        }
        (process.shell, process.foreground)
    }
}

struct InputWriter(Control);

impl Write for InputWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.input(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const GONE: ExitStatus = ExitStatus {
    code: None,
    signal: None,
};

impl Backing for HerdrBacking {
    fn start_reader(&self, sink: Sender<OwnerEvent>) -> Result<(), ServerError> {
        let control = Control::start(
            self.herdr.launch(),
            Arc::clone(&self.herdr.api),
            self.terminal.clone(),
            self.size,
            sink,
        )
        .map_err(ServerError::spawn_failed)?;
        self.control
            .set(control)
            .map_err(|_| ServerError::spawn_failed("herdr terminal already started"))
    }

    fn take_writer(&mut self) -> Result<Box<dyn Write + Send>, ServerError> {
        let control = self.control().map_err(ServerError::spawn_failed)?;
        Ok(Box::new(InputWriter(control.clone())))
    }

    fn resize(&mut self, size: Size) -> Result<(), Fault> {
        self.size = size;
        Ok(self.control()?.resize(size, self.cell)?)
    }

    fn set_cell_size(&mut self, cell: CellSize) -> Result<(), Fault> {
        self.cell = cell;
        Ok(self.control()?.resize(self.size, cell)?)
    }

    fn try_wait(&mut self) -> Option<ExitStatus> {
        self.control()
            .ok()
            .is_some_and(|control| {
                control
                    .shared()
                    .gone
                    .load(std::sync::atomic::Ordering::Acquire)
            })
            .then_some(GONE)
    }

    fn wait(&mut self) -> Result<ExitStatus, Fault> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(status) = self.try_wait() {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(GONE)
    }

    fn kill(&mut self) -> Result<(), Fault> {
        if let Some(pane) = self.pane() {
            self.herdr
                .api
                .call("pane.close", &json!({"pane_id": pane}))?;
        }
        Ok(())
    }

    fn release(&mut self, reason: ExitReason) -> Result<ExitStatus, Fault> {
        if reason == ExitReason::ServerStopped {
            // The terminal belongs to herdr and keeps running without this server.
            if let Ok(control) = self.control() {
                control.detach();
            }
            return Ok(GONE);
        }
        let _ = self.kill();
        let status = self.wait();
        if let Ok(control) = self.control() {
            control.detach();
        }
        status
    }

    fn child_pid(&self) -> u32 {
        self.process().0.unwrap_or(0)
    }

    fn foreground_pid(&self) -> Option<u32> {
        self.process().1
    }

    fn mouse(&mut self, event: &MouseEvent) -> Option<Result<(), Fault>> {
        Some(
            self.control()
                .and_then(|control| control.mouse(event))
                .map_err(Into::into),
        )
    }

    fn brackets_paste(&self) -> bool {
        true
    }

    fn persists_content(&self) -> bool {
        true
    }
}

impl Drop for HerdrBacking {
    fn drop(&mut self) {
        if let Some(control) = self.control.get() {
            control.detach();
        }
    }
}
