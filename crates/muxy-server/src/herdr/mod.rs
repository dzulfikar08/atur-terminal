//! Terminals owned by a herdr server.
//!
//! In this mode the server runs no PTYs. Each Atur Terminal session is a herdr tab with
//! one pane in the workspace of its project, left unlabelled so herdr numbers it like
//! its own tabs; a control stream mirrors the pane's screen into the session owner's
//! terminal, and input, resize and mouse go back through it. Projects, git, files,
//! exec, settings and activity stay Atur Terminal's.

mod adopt;
mod api;
mod base64;
mod control;
#[cfg(test)]
mod fake;
mod panes;
mod scrollback;
mod store;
mod target;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use muxy_protocol::{
    AgentProvider, AgentState, ErrorCode, ExitReason, MouseEvent, SessionId, SessionInfo, Size,
};
use muxy_terminal::pty::ExitStatus;
use muxy_terminal::{CellSize, Row};
use serde_json::{Value, json};

use api::Api;
use control::{Control, Launch};
use panes::{Lookup, PaneState, Panes};
use store::Store;
use target::Target;

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

#[derive(Debug)]
pub(crate) struct Herdr {
    api: Arc<Api>,
    config: HerdrConfig,
    store: Store,
    panes: Panes,
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
            panes: Panes::default(),
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

    /// Creates the session's tab, and its project's workspace if needed. The tab
    /// gets no label of its own, so herdr numbers it like any tab the user opens;
    /// the session is found again by its pane id and the `ATUR_TERM_SESSION_ID`
    /// of its shell, see `adopt`.
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
        let project = request.info.project;
        let existing = self
            .store
            .workspace(project)
            .filter(|workspace| self.workspace_exists(workspace));
        let reply = if let Some(workspace) = existing {
            self.api
                .call(
                    "tab.create",
                    &json!({"workspace_id": workspace, "cwd": cwd, "env": env, "focus": false}),
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
            reply
        };
        let terminal = reply["root_pane"]["terminal_id"]
            .as_str()
            .ok_or_else(|| unavailable("a new pane without a terminal id"))?
            .to_owned();
        let pane = reply["root_pane"]["pane_id"].as_str().map(str::to_owned);
        self.store
            .set_session(request.info.id, project, &terminal, pane.as_deref());
        Ok(HerdrBacking::new(
            Arc::clone(self),
            terminal,
            pane,
            request.size,
        ))
    }

    fn workspace_exists(&self, workspace: &str) -> bool {
        self.api
            .call("workspace.get", &json!({"workspace_id": workspace}))
            .is_ok()
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
    fetched: Option<Instant>,
}

/// How long a foreground-process answer is reused. The owner asks on every tick.
const PROCESS_TTL: Duration = Duration::from_millis(250);

type SharedControl = Arc<Mutex<Option<Control>>>;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A herdr terminal as a session's process. The output stream is opened only while
/// someone watches the session; everything else goes through herdr's socket API.
pub(crate) struct HerdrBacking {
    herdr: Arc<Herdr>,
    target: Arc<Target>,
    size: Size,
    cell: CellSize,
    control: SharedControl,
    process: Mutex<ProcessInfo>,
}

impl std::fmt::Debug for HerdrBacking {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HerdrBacking")
            .field("terminal", &self.target.terminal())
            .finish_non_exhaustive()
    }
}

impl HerdrBacking {
    fn new(herdr: Arc<Herdr>, terminal: String, pane: Option<String>, size: Size) -> Self {
        Self {
            herdr,
            target: Arc::new(Target::new(terminal, pane)),
            size,
            cell: CellSize::default(),
            control: Arc::new(Mutex::new(None)),
            process: Mutex::new(ProcessInfo {
                shell: None,
                foreground: None,
                fetched: None,
            }),
        }
    }

    pub(crate) fn size(&self) -> Size {
        self.size
    }

    fn control(&self) -> Option<Control> {
        lock(&self.control).clone()
    }

    fn state(&self) -> Option<PaneState> {
        self.herdr.panes.state(&self.herdr.api, &self.target)
    }

    fn pane(&self) -> Option<String> {
        self.state().map(|pane| pane.pane)
    }

    fn open(&self, sink: Sender<OwnerEvent>) -> Result<(), ServerError> {
        let mut slot = lock(&self.control);
        if slot.is_some() {
            return Ok(());
        }
        let control = Control::start(
            self.herdr.launch(),
            Arc::clone(&self.herdr.api),
            Arc::clone(&self.target),
            self.size,
            sink,
        )
        .map_err(ServerError::spawn_failed)?;
        if self.cell != CellSize::default() {
            let _ = control.resize(self.size, self.cell);
        }
        *slot = Some(control);
        Ok(())
    }

    fn close(&self) {
        if let Some(control) = lock(&self.control).take() {
            control.detach();
        }
    }

    fn refresh(&self, process: &mut ProcessInfo) {
        // The foreground group of the shell's terminal is in /proc: no request needed.
        #[cfg(target_os = "linux")]
        if let Some(shell) = process.shell
            && let Some(group) = foreground_group(shell)
        {
            process.foreground = Some(group);
            process.fetched = Some(Instant::now());
            return;
        }
        let Some(pane) = self.pane() else {
            return;
        };
        let Ok(info) = self
            .herdr
            .api
            .call("pane.process_info", &json!({"pane_id": pane}))
        else {
            return;
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
        let mut process = lock(&self.process);
        if process.fetched.is_none_or(|at| at.elapsed() > PROCESS_TTL) {
            self.refresh(&mut process);
            process.fetched.get_or_insert_with(Instant::now);
        }
        (process.shell, process.foreground)
    }
}

/// The terminal's foreground process group, the `tpgid` of the shell's `stat`.
#[cfg(target_os = "linux")]
fn foreground_group(shell: u32) -> Option<u32> {
    let stat = std::fs::read(format!("/proc/{shell}/stat")).ok()?;
    let rest = &stat[stat.iter().rposition(|byte| *byte == b')')? + 1..];
    let group: i64 = std::str::from_utf8(rest)
        .ok()?
        .split_ascii_whitespace()
        .nth(5)?
        .parse()
        .ok()?;
    u32::try_from(group).ok().filter(|group| *group > 0)
}

/// Writes to the output stream while one is open and to herdr's API otherwise.
struct InputWriter {
    herdr: Arc<Herdr>,
    target: Arc<Target>,
    control: SharedControl,
}

impl Write for InputWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(control) = lock(&self.control).clone()
            && control.input(bytes).is_ok()
        {
            return Ok(bytes.len());
        }
        let pane = self
            .herdr
            .panes
            .state(&self.herdr.api, &self.target)
            .map(|pane| pane.pane)
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?;
        self.herdr
            .api
            .call(
                "pane.send_text",
                &json!({"pane_id": pane, "text": String::from_utf8_lossy(bytes)}),
            )
            .map_err(io::Error::other)?;
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

/// The provider herdr names by its agent label.
fn provider(label: &str) -> Option<AgentProvider> {
    Some(match label {
        "claude" => AgentProvider::Claude,
        "codex" => AgentProvider::Codex,
        "opencode" => AgentProvider::OpenCode,
        "cursor" => AgentProvider::Cursor,
        "github_copilot" | "github-copilot" | "copilot" => AgentProvider::Copilot,
        "droid" => AgentProvider::Droid,
        "pi" => AgentProvider::Pi,
        "grok" => AgentProvider::Grok,
        "kiro" => AgentProvider::Kiro,
        "antigravity" => AgentProvider::Antigravity,
        "omp" => AgentProvider::OhMyPi,
        _ => return None,
    })
}

fn agent_state(status: &str) -> AgentState {
    match status {
        "working" => AgentState::Working,
        "blocked" => AgentState::Blocked,
        "idle" | "done" => AgentState::Idle,
        _ => AgentState::Unknown,
    }
}

impl Backing for HerdrBacking {
    fn on_demand(&self) -> bool {
        true
    }

    fn start_reader(&self, sink: Sender<OwnerEvent>) -> Result<(), ServerError> {
        self.open(sink)
    }

    fn set_streaming(&mut self, on: bool, sink: &Sender<OwnerEvent>) -> Result<(), Fault> {
        if on {
            self.open(sink.clone())?;
        } else {
            self.close();
        }
        Ok(())
    }

    fn take_writer(&mut self) -> Result<Box<dyn Write + Send>, ServerError> {
        Ok(Box::new(InputWriter {
            herdr: Arc::clone(&self.herdr),
            target: Arc::clone(&self.target),
            control: Arc::clone(&self.control),
        }))
    }

    fn resize(&mut self, size: Size) -> Result<(), Fault> {
        self.size = size;
        if let Some(control) = self.control() {
            control.resize(size, self.cell)?;
        }
        Ok(())
    }

    fn set_cell_size(&mut self, cell: CellSize) -> Result<(), Fault> {
        self.cell = cell;
        if let Some(control) = self.control() {
            control.resize(self.size, cell)?;
        }
        Ok(())
    }

    fn try_wait(&mut self) -> Option<ExitStatus> {
        let gone = match self.control() {
            Some(control) => control
                .shared()
                .gone
                .load(std::sync::atomic::Ordering::Acquire),
            None => matches!(
                self.herdr.panes.lookup(&self.herdr.api, &self.target),
                Lookup::Missing
            ),
        };
        gone.then_some(GONE)
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
            self.close();
            return Ok(GONE);
        }
        let _ = self.kill();
        let status = self.wait();
        self.close();
        status
    }

    fn child_pid(&self) -> u32 {
        self.process().0.unwrap_or(0)
    }

    fn foreground_pid(&self) -> Option<u32> {
        self.process().1
    }

    fn mouse(&mut self, event: &MouseEvent) -> Option<Result<(), Fault>> {
        Some(match self.control() {
            Some(control) => control.mouse(event).map_err(Into::into),
            None => Ok(()),
        })
    }

    fn brackets_paste(&self) -> bool {
        true
    }

    fn persists_content(&self) -> bool {
        true
    }

    fn history_rows(&self) -> Option<usize> {
        Some(self.state().map_or(0, |pane| pane.history_rows))
    }

    fn history(&self, range: std::ops::Range<usize>, cols: u16) -> Result<Vec<Row>, Fault> {
        let pane = self.state().ok_or("herdr no longer lists the terminal")?;
        scrollback::read(&self.herdr.api, &pane, cols, range)
    }

    fn screen(&self) -> Option<(Size, Vec<u8>)> {
        let pane = self.state()?;
        let size = self.herdr.pane_size(&pane.pane)?;
        let reply = self
            .herdr
            .api
            .call(
                "pane.read",
                &json!({
                    "pane_id": pane.pane,
                    "source": "visible",
                    "format": "ansi",
                    "strip_ansi": false,
                }),
            )
            .ok()?;
        Some((size, reply["read"]["text"].as_str()?.as_bytes().to_vec()))
    }

    fn title(&self) -> Option<String> {
        self.state()?.title
    }

    fn detects_agents(&self) -> bool {
        true
    }

    fn agent(&self) -> Option<(AgentProvider, AgentState)> {
        let pane = self.state()?;
        let provider = provider(pane.agent.as_deref()?)?;
        Some((provider, agent_state(pane.agent_status.as_deref()?)))
    }
}

impl Drop for HerdrBacking {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_agent_labels_and_states_map_to_atur_ones() {
        assert_eq!(provider("omp"), Some(AgentProvider::OhMyPi));
        assert_eq!(provider("github_copilot"), Some(AgentProvider::Copilot));
        assert_eq!(provider("amp"), None);
        assert_eq!(agent_state("done"), AgentState::Idle);
        assert_eq!(agent_state("working"), AgentState::Working);
        assert_eq!(agent_state("blocked"), AgentState::Blocked);
        assert_eq!(agent_state("unknown"), AgentState::Unknown);
    }

    #[test]
    fn a_reported_working_cycle_completes_when_the_agent_goes_idle() {
        let mut detector = crate::detection::Detector::default();
        let provider = Some(AgentProvider::Claude);
        assert!(!detector.adopt(provider, AgentState::Idle));
        assert!(!detector.adopt(provider, AgentState::Working));
        assert!(!detector.adopt(provider, AgentState::Blocked));
        assert!(detector.adopt(provider, AgentState::Idle));
        assert!(!detector.adopt(provider, AgentState::Idle));
    }
}
