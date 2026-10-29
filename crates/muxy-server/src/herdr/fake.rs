//! A stand-in for herdr's socket API in tests: the few requests the backend makes
//! to create and adopt tabs, over a unix socket, one request per connection.
// Tests that read a shell's environment from `/proc` run on Linux only.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

/// A process whose environment a test controls, standing for a pane's shell.
pub(super) struct Shell(Child);

impl Shell {
    pub(super) fn spawn(environment: &[(&str, &str)]) -> Self {
        let mut command = Command::new("sleep");
        command
            .arg("600")
            .env_remove("ATUR_TERM_SESSION_ID")
            .env_remove("MUXY_SESSION_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, value) in environment {
            command.env(name, value);
        }
        Self(command.spawn().expect("sleep"))
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Pane {
    pane_id: String,
    terminal_id: String,
    /// What the terminal id is made of besides the handoff generation.
    serial: u32,
    tab_id: String,
    workspace_id: String,
    shell: Option<u32>,
}

struct Tab {
    tab_id: String,
    workspace_id: String,
    label: String,
}

#[derive(Default)]
struct State {
    panes: Vec<Pane>,
    tabs: Vec<Tab>,
    workspaces: Vec<String>,
    calls: Vec<(String, Value)>,
    generation: u32,
    serial: u32,
}

impl State {
    fn terminal_id(&self, serial: u32) -> String {
        format!("term_{}_{serial}", self.generation)
    }

    /// A tab with one pane, labelled like herdr does when it is given none.
    fn add_tab(&mut self, workspace: &str, label: Option<&str>, shell: Option<u32>) -> usize {
        if !self.workspaces.iter().any(|known| known == workspace) {
            self.workspaces.push(workspace.to_owned());
        }
        self.serial += 1;
        let serial = self.serial;
        let number = self
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace)
            .count()
            + 1;
        let tab_id = format!("{workspace}:t{serial}");
        self.tabs.push(Tab {
            tab_id: tab_id.clone(),
            workspace_id: workspace.to_owned(),
            label: label.map_or_else(|| number.to_string(), str::to_owned),
        });
        self.panes.push(Pane {
            pane_id: format!("{workspace}:p{serial}"),
            terminal_id: self.terminal_id(serial),
            serial,
            tab_id,
            workspace_id: workspace.to_owned(),
            shell,
        });
        self.panes.len() - 1
    }

    fn created(&self, pane: usize) -> Value {
        let pane = &self.panes[pane];
        let tab = self.tabs.iter().find(|tab| tab.tab_id == pane.tab_id);
        json!({
            "workspace": {"workspace_id": pane.workspace_id},
            "tab": {"tab_id": pane.tab_id, "label": tab.map(|tab| tab.label.as_str())},
            "root_pane": {"pane_id": pane.pane_id, "terminal_id": pane.terminal_id},
        })
    }

    fn handle(&mut self, method: &str, params: &Value) -> Result<Value, &'static str> {
        self.calls.push((method.to_owned(), params.clone()));
        match method {
            "ping" => Ok(json!({"type": "pong"})),
            "pane.list" => Ok(json!({"panes": self.panes.iter().map(|pane| json!({
                "pane_id": pane.pane_id,
                "terminal_id": pane.terminal_id,
                "tab_id": pane.tab_id,
                "workspace_id": pane.workspace_id,
            })).collect::<Vec<_>>()})),
            "tab.list" => Ok(json!({"tabs": self.tabs.iter().map(|tab| json!({
                "tab_id": tab.tab_id,
                "workspace_id": tab.workspace_id,
                "label": tab.label,
            })).collect::<Vec<_>>()})),
            "pane.process_info" => {
                let pane = self
                    .panes
                    .iter()
                    .find(|pane| Some(pane.pane_id.as_str()) == params["pane_id"].as_str())
                    .ok_or("pane_not_found")?;
                Ok(json!({"process_info": {"shell_pid": pane.shell}}))
            }
            "workspace.get" => {
                let wanted = params["workspace_id"].as_str().ok_or("invalid_params")?;
                self.workspaces
                    .iter()
                    .any(|workspace| workspace == wanted)
                    .then(|| json!({"workspace": {"workspace_id": wanted}}))
                    .ok_or("workspace_not_found")
            }
            "workspace.create" => {
                let workspace = format!("w{}", self.workspaces.len() + 1);
                let pane = self.add_tab(&workspace, None, None);
                Ok(self.created(pane))
            }
            "tab.create" => {
                let workspace = params["workspace_id"]
                    .as_str()
                    .ok_or("invalid_params")?
                    .to_owned();
                let pane = self.add_tab(&workspace, params["label"].as_str(), None);
                Ok(self.created(pane))
            }
            _ => Err("unsupported"),
        }
    }
}

pub(super) struct Fake {
    directory: PathBuf,
    socket: PathBuf,
    state: Arc<Mutex<State>>,
}

fn serve(stream: UnixStream, state: &Mutex<State>) {
    let mut line = String::new();
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    if BufReader::new(clone).read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let Ok(request) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    let method = request["method"].as_str().unwrap_or_default();
    let reply = match state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .handle(method, &request["params"])
    {
        Ok(result) => json!({"id": request["id"], "result": result}),
        Err(code) => json!({"id": request["id"], "error": {"code": code, "message": code}}),
    };
    let mut stream = stream;
    let _ = writeln!(stream, "{reply}");
}

impl Fake {
    pub(super) fn start() -> Self {
        let directory =
            std::env::temp_dir().join(format!("atur-herdr-fake-{}", getrandom::u64().unwrap_or(1)));
        std::fs::create_dir_all(&directory).expect("directory");
        let socket = directory.join("herdr.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Self {
            directory,
            socket,
            state,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(super) fn socket(&self) -> &Path {
        &self.socket
    }

    /// Adds a tab with one pane in `workspace`; pane ids count up from `p1`.
    pub(super) fn add_tab(&self, workspace: &str, label: Option<&str>, shell: Option<&Shell>) {
        self.state()
            .add_tab(workspace, label, shell.map(Shell::pid));
    }

    /// herdr's live handoff: terminals get new ids, panes and processes stay.
    pub(super) fn hand_off(&self) {
        let mut state = self.state();
        state.generation += 1;
        let generation = state.generation;
        for pane in &mut state.panes {
            pane.terminal_id = format!("term_{generation}_{}", pane.serial);
        }
    }

    /// Two panes trade their processes, as when herdr numbers its panes anew in
    /// another order after a restart: the saved ids of one now lead to the other's.
    pub(super) fn swap_processes(&self, first: &str, second: &str) {
        let mut state = self.state();
        let position = |state: &State, id: &str| {
            state
                .panes
                .iter()
                .position(|pane| pane.pane_id == id)
                .expect("pane")
        };
        let (a, b) = (position(&state, first), position(&state, second));
        let shell = state.panes[a].shell;
        let other = std::mem::replace(&mut state.panes[b].shell, shell);
        state.panes[a].shell = other;
    }

    /// Every pane and tab gone, and the numbering started over, as after a restart.
    pub(super) fn clear(&self) {
        let mut state = self.state();
        state.panes.clear();
        state.tabs.clear();
        state.serial = 0;
    }

    pub(super) fn labels(&self) -> Vec<String> {
        self.state()
            .tabs
            .iter()
            .map(|tab| tab.label.clone())
            .collect()
    }

    pub(super) fn calls(&self, method: &str) -> Vec<Value> {
        self.state()
            .calls
            .iter()
            .filter(|(called, _)| called == method)
            .map(|(_, params)| params.clone())
            .collect()
    }
}
