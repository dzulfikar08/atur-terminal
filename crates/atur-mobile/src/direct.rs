//! The facade the Atur React Native module drives.
//!
//! The app's JavaScript layer already draws terminals from ANSI rows (`snapshot` and `diff`
//! frames), so this layer speaks that vocabulary: it renders each attached screen to rows, sends
//! only the rows that changed, and runs the local echo predictor over them. Everything crossing
//! the boundary is a JSON string, which keeps the Kotlin and Swift glue to a few lines each:
//!
//! * requests: [`Direct::call`] (blocking) and [`Direct::fire`] (queued, returns at once, keeps
//!   order; typing, paste and resize);
//! * answers: `{"ok": value}` or `{"error": {"code", "reason"}}`;
//! * pushed events: [`DirectListener::on_message`], one JSON object each, `t` names the kind.
//!
//! Session ids are decimal strings, because a 64-bit id does not fit a JavaScript number.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use muxy_client::PredictMode;
use muxy_protocol::{HistoryCursor, SessionId};
use serde_json::{Value, json};

use crate::connection::{Connection, ConnectionEvent, ConnectionListener, TransportMode};
use crate::input::{self, Segment};
use crate::keys::{self, Key, Modifiers};
use crate::records::{AgentState, ServerCredential, SessionStatus};
use crate::route::RouteInfo;
use crate::terminal::Terminal;
use crate::terminals::Content;
use crate::{MobileError, ansi};

/// Receives pushed events on a core thread; hand them to the JavaScript thread without blocking.
#[uniffi::export(with_foreign)]
pub trait DirectListener: Send + Sync {
    fn on_message(&self, json: String);
}

/// Longest wait of the echo timer while a guess waits for a verdict nobody has scheduled.
const ECHO_POLL: Duration = Duration::from_millis(250);
/// Most rows of scrollback one `history` request returns.
const MAX_HISTORY_ROWS: usize = 20_000;
/// The server serves up to this many history rows per page.
const HISTORY_PAGE: u16 = 500;

/// What a request can fail with: a core error the app can act on, or a malformed request.
enum Fail {
    Core(MobileError),
    Bad(String),
}

impl From<MobileError> for Fail {
    fn from(error: MobileError) -> Self {
        Self::Core(error)
    }
}

impl From<muxy_client::ClientError> for Fail {
    fn from(error: muxy_client::ClientError) -> Self {
        Self::Core(error.into())
    }
}

type Answer = Result<Value, Fail>;

fn bad(what: &str) -> Fail {
    Fail::Bad(format!("missing or invalid `{what}`"))
}

fn envelope(answer: Answer) -> String {
    let value = match answer {
        Ok(value) => json!({ "ok": value }),
        Err(Fail::Bad(reason)) => json!({ "error": { "code": "Internal", "reason": reason } }),
        Err(Fail::Core(error)) => {
            let (code, reason) = match &error {
                MobileError::InvalidLink => ("InvalidLink", String::new()),
                MobileError::InvalidCredential => ("InvalidCredential", String::new()),
                MobileError::Unreachable { reason } => ("Unreachable", reason.clone()),
                MobileError::IdentityMismatch => ("IdentityMismatch", String::new()),
                MobileError::Unauthorized => ("Unauthorized", String::new()),
                MobileError::IncompatibleVersion => ("IncompatibleVersion", String::new()),
                MobileError::Unsupported => ("Unsupported", String::new()),
                MobileError::Timeout => ("Timeout", String::new()),
                MobileError::Disconnected => ("Disconnected", String::new()),
                MobileError::Server { reason } => ("Server", reason.clone()),
            };
            json!({ "error": { "code": code, "reason": reason } })
        }
    };
    value.to_string()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Pairing and credentials

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

fn credential_json(credential: &ServerCredential) -> Value {
    json!({
        "serverId": credential.server_id,
        "serverName": credential.server_name,
        "hosts": credential.hosts,
        "port": credential.port,
        "fingerprint": hex(&credential.fingerprint),
        "deviceId": credential.device_id,
        "token": hex(&credential.token),
    })
}

fn credential_from_json(text: &str) -> Result<ServerCredential, MobileError> {
    let value: Value = serde_json::from_str(text).map_err(|_| MobileError::InvalidCredential)?;
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    let hosts = value
        .get("hosts")
        .and_then(Value::as_array)
        .ok_or(MobileError::InvalidCredential)?
        .iter()
        .map(|host| host.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or(MobileError::InvalidCredential)?;
    let port = value
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or(MobileError::InvalidCredential)?;
    let bytes = |name: &str| {
        field(name)
            .and_then(|text| unhex(&text))
            .ok_or(MobileError::InvalidCredential)
    };
    Ok(ServerCredential {
        server_id: field("serverId").ok_or(MobileError::InvalidCredential)?,
        server_name: field("serverName").unwrap_or_default(),
        hosts,
        port,
        fingerprint: bytes("fingerprint")?,
        device_id: field("deviceId").ok_or(MobileError::InvalidCredential)?,
        token: bytes("token")?,
    })
}

/// The optional `"transport"` of a connect request: `auto` (the default) or `tls`.
fn transport_mode(text: &str) -> Result<TransportMode, Fail> {
    let value: Value = serde_json::from_str(text).map_err(|_| MobileError::InvalidCredential)?;
    match value.get("transport").and_then(Value::as_str) {
        None | Some("auto") => Ok(TransportMode::Auto),
        Some("tls") => Ok(TransportMode::Tls),
        Some(_) => Err(bad("transport")),
    }
}

/// Checks a scanned or pasted link without touching the network.
#[uniffi::export]
pub fn direct_parse_link(link: String) -> String {
    envelope(
        crate::parse_pairing_link(link)
            .map(|target| json!({ "hosts": target.hosts, "port": target.port }))
            .map_err(Fail::from),
    )
}

/// Pairs with the computer that showed the link; blocks on the network. The answer holds the
/// credential to keep in the Keychain or Keystore.
#[uniffi::export]
pub fn direct_pair(link: String, device_name: String) -> String {
    envelope(
        crate::pair(link, device_name)
            .map(|credential| credential_json(&credential))
            .map_err(Fail::from),
    )
}

// ---------------------------------------------------------------------------------------------
// Shared state

/// Wakes the echo timer: a flag plus a condition variable, so waiting costs nothing.
#[derive(Default)]
struct Wake {
    pending: Mutex<bool>,
    signal: Condvar,
}

impl Wake {
    fn notify(&self) {
        *lock(&self.pending) = true;
        self.signal.notify_all();
    }

    fn wait(&self, timeout: Option<Duration>) {
        let mut pending = lock(&self.pending);
        if !*pending {
            pending = match timeout {
                Some(timeout) => {
                    self.signal
                        .wait_timeout(pending, timeout)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .signal
                    .wait(pending)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
        *pending = false;
    }
}

/// What the app was last told about one screen, to send only what changed.
struct Shown {
    cols: u16,
    rows: u16,
    lines: Vec<String>,
}

struct Attached {
    session: u64,
    terminal: Arc<Terminal>,
    shown: Mutex<Option<Shown>>,
}

struct Shared {
    listener: Arc<dyn DirectListener>,
    attached: Mutex<HashMap<u64, Arc<Attached>>>,
    closed: AtomicBool,
    mode: Mutex<PredictMode>,
    wake: Wake,
}

impl Shared {
    fn emit(&self, value: &Value) {
        self.listener.on_message(value.to_string());
    }

    fn attached(&self, session: u64) -> Option<Arc<Attached>> {
        lock(&self.attached).get(&session).cloned()
    }

    fn all_attached(&self) -> Vec<Arc<Attached>> {
        lock(&self.attached).values().cloned().collect()
    }

    /// Sends the app what changed on a screen since it was last told. With `announce`, starts the
    /// stream: `hello`, then a full `snapshot`.
    fn refresh(&self, attached: &Attached, announce: bool) {
        let mut shown = lock(&attached.shown);
        if shown.is_none() && !announce {
            return;
        }
        let (cols, rows, lines) = render(&lock_content(&attached.terminal), Instant::now());
        let session = attached.session.to_string();
        let resized = shown
            .as_ref()
            .is_none_or(|old| old.cols != cols || old.rows != rows);
        if announce {
            self.emit(&json!({ "t": "hello", "session": session, "cols": cols, "rows": rows }));
        } else if resized {
            self.emit(&json!({ "t": "geometry", "session": session, "cols": cols, "rows": rows }));
        }
        if resized || announce {
            self.emit(&json!({ "t": "snapshot", "session": session, "ansi": lines.join("\r\n") }));
        } else if let Some(old) = shown.as_ref() {
            let changed: Vec<Value> = lines
                .iter()
                .enumerate()
                .filter(|(index, line)| old.lines.get(*index) != Some(line))
                .map(|(index, line)| json!([index, line]))
                .collect();
            if changed.is_empty() {
                return;
            }
            self.emit(&json!({ "t": "diff", "session": session, "rows": changed }));
        }
        *shown = Some(Shown { cols, rows, lines });
    }

    /// Lets the predictor expire what the server did not echo; returns how long the timer may
    /// sleep, `None` for as long as it likes.
    fn tick(&self) -> Option<Duration> {
        let now = Instant::now();
        let mut sleep: Option<Duration> = None;
        for attached in self.all_attached() {
            let (changed, pending, deadline) = {
                let mut content = lock_content(&attached.terminal);
                let content = &mut *content;
                let changed = content.predict.tick(&content.grid, now);
                (
                    changed,
                    content.predict.pending(),
                    content.predict.next_deadline(now),
                )
            };
            if changed || pending {
                self.refresh(&attached, false);
            }
            let wait = match (deadline, pending) {
                (Some(at), _) => Some(at.saturating_duration_since(now).min(ECHO_POLL)),
                (None, true) => Some(ECHO_POLL),
                (None, false) => None,
            };
            sleep = match (sleep, wait) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        sleep
    }

    fn timer(self: &Arc<Self>) {
        while !self.closed.load(Ordering::Acquire) {
            let sleep = self.tick();
            self.wake.wait(sleep);
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake.notify();
    }
}

fn lock_content(terminal: &Terminal) -> MutexGuard<'_, Content> {
    terminal.view.lock()
}

/// The screen as ANSI rows with the predictor's guesses drawn in.
fn render(content: &Content, now: Instant) -> (u16, u16, Vec<String>) {
    let cols = content.grid.size.cols;
    let guessed: Vec<u16> = content
        .predict
        .cells(now)
        .iter()
        .map(|cell| cell.row)
        .collect();
    let lines = content
        .grid
        .rows
        .iter()
        .enumerate()
        .map(|(index, runs)| {
            let row = u16::try_from(index).unwrap_or(u16::MAX);
            let overlaid = guessed
                .contains(&row)
                .then(|| content.predict.overlay_runs(row, runs, cols, now))
                .flatten();
            ansi::row(overlaid.as_deref().unwrap_or(runs.as_slice()))
        })
        .collect::<Vec<_>>();
    (cols, content.grid.size.rows, lines)
}

/// Turns the connection's notifications into the app's events.
struct Router {
    shared: Arc<Shared>,
}

impl ConnectionListener for Router {
    fn on_event(&self, event: ConnectionEvent) {
        let shared = &self.shared;
        match event {
            ConnectionEvent::ScreenChanged { session_id } => {
                if let Some(attached) = shared.attached(session_id) {
                    shared.refresh(&attached, false);
                    if lock_content(&attached.terminal).predict.pending() {
                        shared.wake.notify();
                    }
                }
            }
            ConnectionEvent::MetadataChanged { session_id } => {
                if let Some(attached) = shared.attached(session_id) {
                    let content = lock_content(&attached.terminal);
                    let directory = String::from_utf8_lossy(&content.directory.0).into_owned();
                    shared.emit(&json!({
                        "t": "title",
                        "session": session_id.to_string(),
                        "title": content.title,
                        "directory": directory,
                    }));
                }
            }
            ConnectionEvent::SessionEnded { session_id } => {
                lock(&shared.attached).remove(&session_id);
                shared.emit(&json!({ "t": "ended", "session": session_id.to_string() }));
            }
            ConnectionEvent::CatalogChanged => shared.emit(&json!({ "t": "catalog" })),
            ConnectionEvent::SessionsChanged => shared.emit(&json!({ "t": "sessions" })),
            ConnectionEvent::ActivityChanged => shared.emit(&json!({ "t": "activity" })),
            ConnectionEvent::GitChanged { project_id } => {
                shared.emit(&json!({ "t": "git", "projectId": project_id }));
            }
            ConnectionEvent::FilesChanged { project_id, paths } => {
                shared.emit(&json!({ "t": "files", "projectId": project_id, "paths": paths }));
            }
            ConnectionEvent::ServerRestarting => shared.emit(&json!({ "t": "restarting" })),
            ConnectionEvent::Disconnected => {
                shared.close();
                shared.emit(&json!({ "t": "disconnected" }));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The object the module holds

enum Job {
    Input {
        session: u64,
        data: String,
    },
    Paste {
        session: u64,
        text: String,
        submit: bool,
    },
    Resize {
        session: u64,
        cols: u16,
        rows: u16,
    },
}

struct Live {
    connection: Arc<Connection>,
    shared: Arc<Shared>,
    jobs: Sender<Job>,
}

/// One connection to one computer, as the React Native module uses it.
#[derive(uniffi::Object)]
pub struct Direct {
    listener: Arc<dyn DirectListener>,
    live: Mutex<Option<Arc<Live>>>,
}

impl std::fmt::Debug for Direct {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Direct").finish_non_exhaustive()
    }
}

#[uniffi::export]
impl Direct {
    #[uniffi::constructor]
    pub fn new(listener: Arc<dyn DirectListener>) -> Arc<Self> {
        Arc::new(Self {
            listener,
            live: Mutex::new(None),
        })
    }

    /// Connects with a stored credential (the JSON `direct_pair` returned, with an optional
    /// `"transport": "auto" | "tls"`); blocks on the network. The answer says which transport
    /// carries the connection and, when it is not UDP, why.
    pub fn connect(&self, credential_json: String) -> String {
        envelope(self.start(&credential_json))
    }

    /// A request that may take a moment; blocks until the server answers.
    pub fn call(&self, method: String, args_json: String) -> String {
        envelope(self.dispatch(&method, &args_json))
    }

    /// A request that must keep its order and never wait: typing, paste and resize are queued
    /// and carried out by one thread, in the order they were fired.
    pub fn fire(&self, method: String, args_json: String) -> String {
        envelope(self.enqueue(&method, &args_json))
    }

    /// Closes the connection. Safe to call twice, or when never connected.
    pub fn disconnect(&self) {
        if let Some(live) = lock(&self.live).take() {
            live.shared.close();
            live.connection.disconnect();
        }
    }
}

impl Direct {
    fn live(&self) -> Result<Arc<Live>, Fail> {
        lock(&self.live)
            .clone()
            .ok_or(Fail::Core(MobileError::Disconnected))
    }

    fn start(&self, credential_json: &str) -> Answer {
        self.disconnect();
        let credential = credential_from_json(credential_json)?;
        let mode = transport_mode(credential_json)?;
        let name = credential.server_name.clone();
        let shared = Arc::new(Shared {
            listener: Arc::clone(&self.listener),
            attached: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            mode: Mutex::new(PredictMode::default()),
            wake: Wake::default(),
        });
        let router = Arc::new(Router {
            shared: Arc::clone(&shared),
        });
        let opened = Connection::connect_direct(credential, mode, router)?;
        let connection = opened.connection;
        let server_id = connection.server_id()?;
        let version = connection.server_version();
        let transport = connection.transport_name();

        let (jobs, queue) = mpsc::channel();
        let worker = Worker {
            shared: Arc::clone(&shared),
            connection: Arc::clone(&connection),
        };
        spawn("atur-direct-input", move || {
            for job in queue {
                worker.run(job);
            }
        })?;
        let timer = Arc::clone(&shared);
        spawn("atur-direct-echo", move || timer.timer())?;

        *lock(&self.live) = Some(Arc::new(Live {
            connection,
            shared,
            jobs,
        }));
        Ok(json!({
            "serverId": server_id,
            "serverName": name,
            "serverVersion": version,
            "transport": transport,
            "udpNote": opened.udp_note,
            "route": opened.route.to_json(),
            "addresses": opened.addresses,
        }))
    }

    fn enqueue(&self, method: &str, args_json: &str) -> Answer {
        let live = self.live()?;
        let args = parse_args(args_json)?;
        let session = || session_arg(&args);
        let job = match method {
            "input" => Job::Input {
                session: session()?,
                data: str_arg(&args, "data")?,
            },
            "paste" => Job::Paste {
                session: session()?,
                text: str_arg(&args, "text")?,
                submit: args.get("submit").and_then(Value::as_bool).unwrap_or(false),
            },
            "resize" => Job::Resize {
                session: session()?,
                cols: u16_arg(&args, "cols")?,
                rows: u16_arg(&args, "rows")?,
            },
            other => return Err(Fail::Bad(format!("`{other}` cannot be fired"))),
        };
        live.jobs
            .send(job)
            .map_err(|_| Fail::Core(MobileError::Disconnected))?;
        Ok(Value::Null)
    }

    fn dispatch(&self, method: &str, args_json: &str) -> Answer {
        if matches!(method, "input" | "paste" | "resize") {
            return self.enqueue(method, args_json);
        }
        let live = self.live()?;
        let args = parse_args(args_json)?;
        let connection = &live.connection;
        let shared = &live.shared;
        match method {
            "projects" => Ok(Value::Array(
                connection
                    .projects()?
                    .iter()
                    .map(|project| {
                        json!({
                            "id": project.id,
                            "name": project.name,
                            "directory": project.directory,
                            "isHome": project.is_home,
                            "isWorktree": project.is_worktree,
                        })
                    })
                    .collect(),
            )),
            "sessions" => sessions(connection),
            "createSession" => {
                let project = str_arg(&args, "projectId")?;
                let created = connection.create_session(
                    project.clone(),
                    u16_arg(&args, "cols")?,
                    u16_arg(&args, "rows")?,
                )?;
                Ok(json!({
                    "id": created.id.to_string(),
                    "projectId": project,
                    "projectName": "",
                    "directory": created.directory,
                    "title": "",
                    "status": "live",
                    "attached": false,
                    "agent": Value::Null,
                }))
            }
            "endSession" => {
                connection.end_session(session_arg(&args)?)?;
                Ok(Value::Null)
            }
            "attach" => attach(
                &live,
                session_arg(&args)?,
                u16_arg(&args, "cols")?,
                u16_arg(&args, "rows")?,
            ),
            "detach" => {
                let removed = lock(&shared.attached).remove(&session_arg(&args)?);
                if let Some(attached) = removed {
                    attached.terminal.detach()?;
                }
                Ok(Value::Null)
            }
            "history" => {
                let attached = attached_arg(shared, &args)?;
                let lines = args
                    .get("lines")
                    .and_then(Value::as_u64)
                    .and_then(|lines| usize::try_from(lines).ok())
                    .unwrap_or(400)
                    .clamp(1, MAX_HISTORY_ROWS);
                let (rows, visible) = history(&attached.terminal, lines)?;
                Ok(json!({ "rows": rows, "visible": visible }))
            }
            "activity" => activity(connection),
            "ackActivity" => {
                let ids = args
                    .get("eventIds")
                    .and_then(Value::as_array)
                    .ok_or_else(|| bad("eventIds"))?
                    .iter()
                    .map(|id| id.as_str().and_then(|id| id.parse::<u64>().ok()))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| bad("eventIds"))?;
                connection.acknowledge_activity(ids)?;
                Ok(Value::Null)
            }
            other => Self::tools(&live, other, &args),
        }
    }

    /// The requests that read or tune the connection rather than manage sessions.
    fn tools(live: &Live, method: &str, args: &Value) -> Answer {
        let connection = &live.connection;
        let shared = &live.shared;
        match method {
            "echoMode" => {
                let mode: PredictMode = str_arg(args, "mode")?.parse().map_err(|_| bad("mode"))?;
                *lock(&shared.mode) = mode;
                for attached in shared.all_attached() {
                    lock_content(&attached.terminal).predict.set_mode(mode);
                    shared.refresh(&attached, false);
                }
                Ok(Value::Null)
            }
            "echoStats" => {
                let attached = attached_arg(shared, args)?;
                let rtt = connection.client().rtt();
                let content = lock_content(&attached.terminal);
                let stats = content.predict.stats();
                Ok(json!({
                    "mode": content.predict.mode().as_str(),
                    "rttMs": rtt.map(|rtt| rtt.as_secs_f64() * 1000.0),
                    "transport": connection.transport_name(),
                    "route": connection.route().map(RouteInfo::to_json),
                    "keys": stats.keys,
                    "visibleConfirmed": stats.visible_confirmed,
                    "hiddenConfirmed": stats.hidden_confirmed,
                    "visibleWrong": stats.visible_wrong,
                    "hiddenWrong": stats.hidden_wrong,
                }))
            }
            "ping" => {
                let start = Instant::now();
                connection.client().ping()?;
                Ok(json!(start.elapsed().as_secs_f64() * 1000.0))
            }
            "filesList" => {
                let entries = connection
                    .files(str_arg(args, "projectId")?)?
                    .list(str_arg(args, "path").unwrap_or_default())?;
                Ok(Value::Array(
                    entries
                        .iter()
                        .map(|entry| {
                            json!({
                                "name": entry.name,
                                "path": entry.path,
                                "isDirectory": entry.is_directory,
                            })
                        })
                        .collect(),
                ))
            }
            "gitSummary" => {
                let summary = connection.git(str_arg(args, "projectId")?)?.summary()?;
                Ok(summary.map_or(Value::Null, |summary| {
                    json!({
                        "branch": summary.branch,
                        "ahead": summary.ahead,
                        "behind": summary.behind,
                        "staged": summary.staged,
                        "unstaged": summary.unstaged,
                        "untracked": summary.untracked,
                        "conflicted": summary.conflicted,
                    })
                }))
            }
            "pasteImage" => paste_image(live, args),
            other => Err(Fail::Bad(format!("unknown method `{other}`"))),
        }
    }
}

const PASTE_LIMIT: usize = 5 * 1024 * 1024;
const PASTE_DIRECTORY: [&str; 3] = [".cache", "atur", "pastes"];

fn server_error(reason: &str) -> Fail {
    Fail::Core(MobileError::Server {
        reason: reason.into(),
    })
}

/// Standard base64, padded or not; ASCII whitespace is ignored.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let (mut buffer, mut bits) = (0_u32, 0_u32);
    for byte in text.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

/// Writes a pasted image into the computer's Home project at `.cache/atur/pastes/<name>`, where
/// the agent can read it, and answers with its absolute path.
fn paste_image(live: &Live, args: &Value) -> Answer {
    attached_arg(&live.shared, args)?;
    let name = str_arg(args, "name")?;
    if name.is_empty()
        || name.contains(['/', '\\'])
        || name.contains("..")
        || name.chars().any(char::is_control)
    {
        return Err(bad("name"));
    }
    let bytes = base64_decode(&str_arg(args, "base64")?).ok_or_else(|| bad("base64"))?;
    if bytes.len() > PASTE_LIMIT {
        return Err(server_error("Image is larger than 5 MiB."));
    }
    let home = live
        .connection
        .projects()?
        .into_iter()
        .find(|project| project.is_home)
        .ok_or_else(|| server_error("The computer has no Home project."))?;
    let files = live.connection.files(home.id)?;
    let mut folder = String::new();
    for part in PASTE_DIRECTORY {
        if !folder.is_empty() {
            folder.push('/');
        }
        folder.push_str(part);
        if files.stat(folder.clone()).is_err() {
            files.create_directory(folder.clone())?;
        }
    }
    files.write_bytes(format!("{folder}/{name}"), bytes)?;
    let base = home.directory.trim_end_matches('/');
    Ok(json!({ "path": format!("{base}/{folder}/{name}") }))
}

fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> Result<(), Fail> {
    thread::Builder::new()
        .name(name.into())
        .spawn(work)
        .map(drop)
        .map_err(|error| {
            Fail::Core(MobileError::Unreachable {
                reason: error.to_string(),
            })
        })
}

// ---------------------------------------------------------------------------------------------
// Arguments

fn parse_args(text: &str) -> Result<Value, Fail> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(text).map_err(|_| Fail::Bad("arguments are not JSON".into()))
}

fn str_arg(args: &Value, name: &str) -> Result<String, Fail> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| bad(name))
}

fn u16_arg(args: &Value, name: &str) -> Result<u16, Fail> {
    args.get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| bad(name))
}

fn session_arg(args: &Value) -> Result<u64, Fail> {
    str_arg(args, "session")?
        .parse()
        .map_err(|_| bad("session"))
}

fn attached_arg(shared: &Shared, args: &Value) -> Result<Arc<Attached>, Fail> {
    shared.attached(session_arg(args)?).ok_or_else(|| {
        Fail::Core(MobileError::Server {
            reason: "That terminal is not attached.".into(),
        })
    })
}

// ---------------------------------------------------------------------------------------------
// Requests

fn state_name(state: AgentState) -> &'static str {
    match state {
        AgentState::Unknown => "unknown",
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
    }
}

/// Live sessions of every project, each with the agent working in it and the terminal's title.
fn sessions(connection: &Connection) -> Answer {
    let agents: HashMap<u64, (String, AgentState)> = connection
        .activity()?
        .agents
        .into_iter()
        .map(|agent| (agent.session_id, (agent.provider, agent.state)))
        .collect();
    let mut listed = Vec::new();
    for project in connection.projects()? {
        for session in connection.sessions(project.id.clone())? {
            let status = match session.status {
                SessionStatus::Live => "live",
                SessionStatus::Starting => "starting",
                SessionStatus::Ended | SessionStatus::Unavailable => continue,
            };
            listed.push(((project.id.clone(), project.name.clone()), session, status));
        }
    }
    let live: Vec<SessionId> = listed
        .iter()
        .filter(|(_, _, status)| *status == "live")
        .filter_map(|(_, session, _)| SessionId::new(session.id))
        .collect();
    let titles = connection.session_titles(&live);
    Ok(Value::Array(
        listed
            .into_iter()
            .map(|((project_id, project_name), session, status)| {
                let agent = agents.get(&session.id).map_or(
                    Value::Null,
                    |(provider, state)| json!({ "provider": provider, "state": state_name(*state) }),
                );
                let title = SessionId::new(session.id)
                    .and_then(|id| titles.get(&id).cloned())
                    .unwrap_or_default();
                json!({
                    "id": session.id.to_string(),
                    "projectId": project_id,
                    "projectName": project_name,
                    "directory": session.directory,
                    "title": title,
                    "status": status,
                    "attached": session.attached,
                    "agent": agent,
                })
            })
            .collect(),
    ))
}

fn activity(connection: &Connection) -> Answer {
    let activity = connection.activity()?;
    Ok(json!({
        "agents": activity.agents.iter().map(|agent| json!({
            "session": agent.session_id.to_string(),
            "projectId": agent.project_id,
            "provider": agent.provider,
            "state": state_name(agent.state),
        })).collect::<Vec<_>>(),
        "events": activity.events.iter().map(|event| json!({
            "id": event.id.to_string(),
            "session": event.session_id.to_string(),
            "projectId": event.project_id,
            "provider": event.provider,
            "timestamp": event.timestamp,
            "kind": match event.kind {
                crate::records::ActivityKind::Attention => "attention",
                crate::records::ActivityKind::Completed => "completed",
            },
        })).collect::<Vec<_>>(),
    }))
}

fn attach(live: &Live, session: u64, cols: u16, rows: u16) -> Answer {
    let shared = &live.shared;
    let attached = if let Some(existing) = shared.attached(session) {
        existing
    } else {
        let terminal = live.connection.attach(session, cols, rows)?;
        lock_content(&terminal)
            .predict
            .set_mode(*lock(&shared.mode));
        let attached = Arc::new(Attached {
            session,
            terminal,
            shown: Mutex::new(None),
        });
        lock(&shared.attached).insert(session, Arc::clone(&attached));
        attached
    };
    // Starts (or restarts) the frame stream for the app.
    shared.refresh(&attached, true);
    let content = lock_content(&attached.terminal);
    Ok(json!({
        "session": session.to_string(),
        "cols": content.grid.size.cols,
        "rows": content.grid.size.rows,
        "title": content.title,
        "directory": String::from_utf8_lossy(&content.directory.0),
    }))
}

/// How often a read starts over when the computer rewrites its scrollback under it.
const HISTORY_ATTEMPTS: usize = 3;

/// The newest `lines` rows including scrollback, oldest first, ending with the visible screen.
///
/// A page cursor goes stale when the computer clears or rewrites its scrollback between two pages
/// (a full-screen redraw of an agent does that); the read then starts over from the newest page,
/// and only the last attempt settles for the rows it has, which are a short read, not a wrong one.
fn history(terminal: &Terminal, lines: usize) -> Result<(Vec<String>, usize), Fail> {
    let mut attempt = 1;
    loop {
        if let Some(read) = history_once(terminal, lines, attempt >= HISTORY_ATTEMPTS)? {
            return Ok(read);
        }
        attempt += 1;
    }
}

/// One read; `None` when a cursor went stale and `settle` is off.
fn history_once(
    terminal: &Terminal,
    lines: usize,
    settle: bool,
) -> Result<Option<(Vec<String>, usize)>, Fail> {
    let client = &terminal.client;
    let channel = terminal.view.channel;
    let page_rows = u16::try_from(lines.min(usize::from(HISTORY_PAGE))).unwrap_or(HISTORY_PAGE);
    let first = client.history_page(channel, HistoryCursor(0), page_rows)?;
    let screen: Vec<String> = match &first.screen {
        Some(screen) => screen.rows.iter().map(|row| ansi::row(&row.runs)).collect(),
        None => lock_content(terminal)
            .grid
            .rows
            .iter()
            .map(|runs| ansi::row(runs))
            .collect(),
    };
    let visible = screen.len();
    let mut rows: Vec<String> = first.rows.iter().map(|row| ansi::row(&row.runs)).collect();
    let mut next = first.next;
    while rows.len() + visible < lines {
        let Some(before) = next else { break };
        match client.history_page(channel, before, HISTORY_PAGE) {
            Ok(page) => {
                let mut older: Vec<String> =
                    page.rows.iter().map(|row| ansi::row(&row.runs)).collect();
                older.append(&mut rows);
                rows = older;
                next = page.next;
            }
            Err(muxy_client::ClientError::Server(error))
                if error.code == muxy_protocol::ErrorCode::StaleHistoryCursor =>
            {
                if !settle {
                    return Ok(None);
                }
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    rows.extend(screen);
    if rows.len() > lines.max(visible) {
        rows.drain(..rows.len() - lines.max(visible));
    }
    Ok(Some((rows, visible)))
}

// ---------------------------------------------------------------------------------------------
// The input thread

struct Worker {
    shared: Arc<Shared>,
    connection: Arc<Connection>,
}

impl Worker {
    fn run(&self, job: Job) {
        match job {
            Job::Input { session, data } => {
                if let Some(attached) = self.shared.attached(session) {
                    for segment in input::segments(&data) {
                        self.send(&attached, &segment);
                    }
                    self.settle(&attached);
                }
            }
            Job::Paste {
                session,
                text,
                submit,
            } => {
                if let Some(attached) = self.shared.attached(session) {
                    {
                        let mut content = lock_content(&attached.terminal);
                        content.predict.interrupt(Instant::now());
                    }
                    let _ = attached.terminal.paste(text);
                    if submit {
                        let _ = attached.terminal.send_key(Key::Enter, Modifiers::default());
                    }
                }
            }
            Job::Resize {
                session,
                cols,
                rows,
            } => {
                if let Some(attached) = self.shared.attached(session) {
                    lock_content(&attached.terminal).predict.reset();
                    let _ = attached.terminal.resize(cols, rows);
                }
            }
        }
    }

    /// Guesses what the program will print for these keys, then sends them.
    fn send(&self, attached: &Attached, segment: &Segment) {
        let bytes = match segment {
            Segment::Text(text) => text.clone().into_bytes(),
            Segment::Key(key, modifiers) => {
                let modes = lock_content(&attached.terminal).grid.modes;
                match keys::encode(key, *modifiers, modes) {
                    Some(bytes) => bytes,
                    None => return,
                }
            }
        };
        {
            let mut content = lock_content(&attached.terminal);
            let content = &mut *content;
            content.predict.set_rtt(self.connection.client().rtt());
            content.predict.keys(&bytes, &content.grid, Instant::now());
        }
        let _ = attached.terminal.send_input(bytes);
    }

    /// Shows the guesses at once, and asks the server for the answer that settles them.
    fn settle(&self, attached: &Attached) {
        self.shared.refresh(attached, false);
        let wanted = {
            let content = lock_content(&attached.terminal);
            content.predict.wants_barrier()
        };
        if !wanted {
            return;
        }
        self.shared.wake.notify();
        let stamp = Instant::now();
        let terminal = Arc::clone(&attached.terminal);
        let shared = Arc::clone(&self.shared);
        self.connection
            .client()
            .ping_async()
            .on_complete(move |reply| {
                if reply.is_ok() {
                    lock_content(&terminal)
                        .predict
                        .barrier(stamp, Instant::now());
                    shared.wake.notify();
                }
            });
    }
}
