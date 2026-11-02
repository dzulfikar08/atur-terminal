//! Finds, starts and stops `atur-term-screen`, the program that shares this
//! Mac's screen, and is the one place that knows its command line.
//!
//! A [`HelperApp`] is the program's small application bundle, started through
//! Launch Services so that macOS treats it as an application of its own, or a
//! plain executable that is spawned directly, for tests and development. A
//! program started through Launch Services inherits nothing from the caller,
//! so everything crosses as arguments and through the status file it rewrites
//! (see [`crate::status`]).
//!
//! The caller owns a private folder (see [`private_folder`]); the launcher
//! keeps the status file, the socket and, for a plain executable, a log in it.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};

use crate::status::{State, Status};

/// Names a plain executable to run instead of the bundled program. With it
/// set, nothing else is searched.
pub const HELPER_VARIABLE: &str = "ATUR_TERM_SCREEN_HELPER";

/// Name of the program inside the bundle.
const EXECUTABLE: &str = "atur-term-screen";
/// The apps that carry the program, the stable one first.
const APP_NAMES: [&str; 2] = ["Atur Terminal.app", "Atur Terminal Beta.app"];
const OPEN: &str = "/usr/bin/open";
/// What the program is called to the user when it was not started as a bundle.
const DEFAULT_NAME: &str = "Atur Terminal Screen";

const STATUS: &str = "status";
const SOCKET: &str = "s";
const LOG: &str = "log";
/// The longest socket path macOS accepts is 103 bytes; stay below it.
const MAX_SOCKET_PATH: usize = 100;

/// First launches of a downloaded app can be slow.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(25);
const EXIT_POLL: Duration = Duration::from_millis(20);
/// How long a program gets to end after it was asked to, before it is forced.
const STOP_GRACE: Duration = Duration::from_secs(2);

const NOT_FOUND: &str = "Atur Terminal's screen sharing program was not found on this Mac. \
                         Install the Atur Terminal app in the Applications folder to share this Mac's screen.";
const NOT_IN_TIME: &str = "The screen sharing program did not start in time. Check that someone \
                           is logged in on this Mac's screen, then try again.";
const STOPPED_AT_START: &str = "The screen sharing program stopped before it was ready.";

/// Nothing that can run the screen sharing program was found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FindError;

impl fmt::Display for FindError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(NOT_FOUND)
    }
}

impl std::error::Error for FindError {}

/// Why the program did not become ready. Both carry a sentence for the user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartError {
    /// Screen Recording is not allowed for the program.
    NeedsPermission(String),
    /// It could not start, or did not in time.
    Failed(String),
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsPermission(message) | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StartError {}

/// How the program treats the modifier keys a viewer sends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Keys {
    /// Control acts as Command, Alt as Option, Super and Meta as Control, so
    /// that Ctrl+C and Ctrl+V from a viewer copy and paste on the Mac.
    Command,
    /// Control acts as Control, Alt as Option, Super and Meta as Command.
    Literal,
}

impl Keys {
    /// `command` or `literal`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "command" => Some(Self::Command),
            "literal" => Some(Self::Literal),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Literal => "literal",
        }
    }
}

/// What the program is asked to do when it serves a screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServeOptions {
    /// Frames per second ceiling; the program's own default when `None`.
    pub fps: Option<u32>,
    /// Longest edge of the shared picture in pixels; the program's default when `None`.
    pub max_edge: Option<u32>,
    pub keys: Option<Keys>,
    /// The name the screen is announced under.
    pub name: Option<String>,
    /// Show the screen but ignore mouse, keyboard and clipboard.
    pub view_only: bool,
    /// A moving test pattern of this size instead of the screen: no capture,
    /// no permission. For tests.
    pub synthetic: Option<(u32, u32)>,
    /// How long the program may take to become ready.
    pub ready_timeout: Duration,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            fps: None,
            max_edge: None,
            keys: None,
            name: None,
            view_only: false,
            synthetic: None,
            ready_timeout: READY_TIMEOUT,
        }
    }
}

impl ServeOptions {
    fn arguments(&self, socket: &Path, status: &Path) -> Vec<OsString> {
        let mode = if self.synthetic.is_some() {
            "synthetic"
        } else {
            "serve"
        };
        let mut arguments: Vec<OsString> = vec![mode.into()];
        arguments.push("--socket".into());
        arguments.push(socket.into());
        arguments.push("--status".into());
        arguments.push(status.into());
        arguments.push("--owner".into());
        arguments.push(std::process::id().to_string().into());
        if let Some((width, height)) = self.synthetic {
            arguments.push("--size".into());
            arguments.push(format!("{width}x{height}").into());
            return arguments;
        }
        if let Some(fps) = self.fps {
            arguments.push("--fps".into());
            arguments.push(fps.to_string().into());
        }
        if let Some(edge) = self.max_edge {
            arguments.push("--max-edge".into());
            arguments.push(edge.to_string().into());
        }
        if let Some(keys) = self.keys {
            arguments.push("--keys".into());
            arguments.push(keys.as_str().into());
        }
        if let Some(name) = &self.name {
            arguments.push("--name".into());
            arguments.push(name.into());
        }
        if self.view_only {
            arguments.push("--view-only".into());
        }
        arguments
    }
}

/// Which permissions the program has been given.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions {
    pub screen_recording: bool,
    /// Needed for mouse and keyboard; without it the screen is only shown.
    pub accessibility: bool,
}

impl Permissions {
    #[must_use]
    pub fn all_allowed(self) -> bool {
        self.screen_recording && self.accessibility
    }
}

/// What to tell the user when Screen Recording is not allowed on `host`.
/// `app_name` is [`HelperApp::display_name`].
#[must_use]
pub fn permission_message(host: &str, app_name: &str) -> String {
    format!(
        "Screen recording permission needed on {host}: open System Settings → Privacy & \
         Security → Screen & System Audio Recording and turn on {app_name}. A prompt was shown \
         on that Mac; once you have allowed it, choose Retry connection."
    )
}

/// The program that shares this Mac's screen, and how it is started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelperApp {
    target: Target,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Target {
    /// An application bundle, opened through Launch Services.
    Bundle { app: PathBuf, opener: PathBuf },
    /// A plain executable, spawned directly.
    Executable(PathBuf),
}

impl HelperApp {
    /// Finds the program: the executable named by [`HELPER_VARIABLE`], else the
    /// bundle inside the app this executable belongs to, else the one inside
    /// an installed app.
    ///
    /// # Errors
    /// [`FindError`] when there is none.
    pub fn find() -> Result<Self, FindError> {
        let executable = std::env::current_exe().and_then(fs::canonicalize).ok();
        Self::locate(
            std::env::var_os(HELPER_VARIABLE),
            executable.as_deref(),
            std::env::home_dir().as_deref(),
            Path::new("/Applications"),
        )
    }

    /// A plain executable that is spawned directly; for tests and development.
    pub fn from_executable(program: impl Into<PathBuf>) -> Self {
        Self {
            target: Target::Executable(program.into()),
        }
    }

    fn locate(
        variable: Option<OsString>,
        executable: Option<&Path>,
        home: Option<&Path>,
        system: &Path,
    ) -> Result<Self, FindError> {
        if let Some(program) = variable.filter(|program| !program.is_empty()) {
            return Ok(Self::from_executable(program));
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        candidates.extend(executable.and_then(helper_in_bundle));
        let folders: Vec<PathBuf> = std::iter::once(system.to_path_buf())
            .chain(home.map(|home| home.join("Applications")))
            .collect();
        for name in APP_NAMES {
            for folder in &folders {
                candidates.extend(helper_in_app(&folder.join(name)));
            }
        }
        candidates
            .into_iter()
            .find(|app| app.join("Contents/MacOS").join(EXECUTABLE).is_file())
            .map(|app| Self {
                target: Target::Bundle {
                    app,
                    opener: PathBuf::from(OPEN),
                },
            })
            .ok_or(FindError)
    }

    /// What the program is called in System Settings.
    #[must_use]
    pub fn display_name(&self) -> &str {
        match &self.target {
            Target::Bundle { app, .. } => app
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or(DEFAULT_NAME),
            Target::Executable(_) => DEFAULT_NAME,
        }
    }

    /// Starts the program serving a screen and waits until it is ready.
    ///
    /// `folder` is a private folder the caller created; the status file and
    /// the socket are made in it. On an error nothing is left running; the
    /// folder is the caller's to remove.
    ///
    /// # Errors
    /// [`StartError::NeedsPermission`] when Screen Recording is not allowed,
    /// [`StartError::Failed`] for anything else.
    pub fn start(&self, folder: &Path, options: &ServeOptions) -> Result<Running, StartError> {
        let socket = folder.join(SOCKET);
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(StartError::Failed(format!(
                "The screen sharing program cannot listen at {}: the folder name is too long.",
                socket.display()
            )));
        }
        let status_path = folder.join(STATUS);
        clear(folder);
        let mut launch = self.launch(folder, &options.arguments(&socket, &status_path))?;
        let status = match launch.wait(&status_path, options.ready_timeout) {
            Ok(status) => status,
            Err(error) => {
                launch.end(true);
                return Err(error);
            }
        };
        match status.state {
            State::Ready => {
                launch.release_opener();
                Ok(Running {
                    pid: launch.pid.unwrap_or(status.pid),
                    launch,
                    folder: folder.to_path_buf(),
                    socket,
                    status_path,
                })
            }
            State::NeedsPermission => {
                launch.end(false);
                Err(StartError::NeedsPermission(status.message))
            }
            State::Starting | State::Failed | State::Ended => {
                launch.end(false);
                Err(StartError::Failed(message_or(status, STOPPED_AT_START)))
            }
        }
    }

    /// Asks the program which permissions it has, without any prompt.
    ///
    /// # Errors
    /// [`StartError::Failed`] when the program could not be run.
    pub fn check_permissions(&self, folder: &Path) -> Result<Permissions, StartError> {
        self.permissions("check", folder)
    }

    /// Makes the system show its permission prompts for the program, and
    /// reports what is allowed at that moment.
    ///
    /// # Errors
    /// [`StartError::Failed`] when the program could not be run.
    pub fn request_permissions(&self, folder: &Path) -> Result<Permissions, StartError> {
        self.permissions("request", folder)
    }

    fn permissions(&self, mode: &str, folder: &Path) -> Result<Permissions, StartError> {
        let status_path = folder.join(STATUS);
        clear(folder);
        let arguments = [
            OsString::from(mode),
            OsString::from("--status"),
            status_path.clone().into_os_string(),
        ];
        let mut launch = self.launch(folder, &arguments)?;
        let outcome = launch.wait(&status_path, CHECK_TIMEOUT);
        launch.end(outcome.is_err());
        let status = outcome?;
        match status.state {
            State::Ready | State::NeedsPermission => Ok(Permissions {
                screen_recording: status
                    .screen_recording
                    .unwrap_or(status.state == State::Ready),
                accessibility: status.accessibility.unwrap_or(false),
            }),
            State::Starting | State::Failed | State::Ended => {
                Err(StartError::Failed(message_or(status, STOPPED_AT_START)))
            }
        }
    }

    fn launch(&self, folder: &Path, arguments: &[OsString]) -> Result<Launch, StartError> {
        match &self.target {
            Target::Bundle { app, opener } => {
                let opener = Command::new(opener)
                    .args(["-n", "-g", "-j", "-a"])
                    .arg(app)
                    .arg("--args")
                    .args(arguments)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|error| {
                        StartError::Failed(format!(
                            "The screen sharing program could not be started on this Mac: {error}."
                        ))
                    })?;
                Ok(Launch {
                    helper: None,
                    opener: Some(opener),
                    log: None,
                    pid: None,
                })
            }
            Target::Executable(program) => {
                let log = folder.join(LOG);
                let helper = spawn_direct(program, arguments, &log)?;
                Ok(Launch {
                    pid: Some(helper.id()),
                    helper: Some(helper),
                    opener: None,
                    log: Some(log),
                })
            }
        }
    }
}

/// The helper bundle inside the app bundle that holds `executable`, which is
/// `<name>.app/Contents/MacOS/<program>`: `<name>.app/Contents/Helpers/<name> Screen.app`.
#[must_use]
pub fn helper_in_bundle(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    if macos.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    helper_in_app(contents.parent()?)
}

fn helper_in_app(app: &Path) -> Option<PathBuf> {
    let name = app.file_name()?.to_str()?.strip_suffix(".app")?;
    Some(
        app.join("Contents/Helpers")
            .join(format!("{name} Screen.app")),
    )
}

/// Spawns the program in a process group of its own, its error output in `log`.
fn spawn_direct(program: &Path, arguments: &[OsString], log: &Path) -> Result<Child, StartError> {
    let failed = |error: io::Error| {
        StartError::Failed(format!(
            "The screen sharing program could not be started: {error}."
        ))
    };
    let log = File::create(log).map_err(failed)?;
    let mut attempts = 0;
    loop {
        let spawned = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.try_clone().map_err(failed)?)
            .process_group(0)
            .spawn();
        match spawned {
            // A program that was just written is briefly busy when another
            // thread forks while the file is still open for writing.
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy && attempts < 50 => {
                attempts += 1;
                thread::sleep(Duration::from_millis(10));
            }
            result => return result.map_err(failed),
        }
    }
}

/// Removes what an earlier run left in the folder.
fn clear(folder: &Path) {
    for name in [STATUS, "status.tmp", SOCKET, LOG] {
        let _ = fs::remove_file(folder.join(name));
    }
}

fn message_or(status: Status, fallback: &str) -> String {
    if status.message.is_empty() {
        fallback.to_owned()
    } else {
        status.message
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn last_line(text: &str) -> Option<&str> {
    text.lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty())
}

fn process_id(pid: u32) -> Option<Pid> {
    Pid::from_raw(i32::try_from(pid).ok()?)
}

fn send(pid: u32, signal: Signal) {
    if let Some(pid) = process_id(pid) {
        let _ = rustix::process::kill_process(pid, signal);
    }
}

fn alive(pid: u32) -> bool {
    process_id(pid).is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

/// Waits for a child in the background so that it does not stay a zombie.
fn reap(mut child: Child) {
    let _ = thread::Builder::new()
        .name("atur-screen-reap".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// A started program that has not yet reported that it is ready.
#[derive(Debug)]
struct Launch {
    /// The program itself, when it was spawned directly.
    helper: Option<Child>,
    /// `open`, when Launch Services was asked to start the program.
    opener: Option<Child>,
    /// Where a directly spawned program writes its error output.
    log: Option<PathBuf>,
    /// Process number of the program, once known.
    pid: Option<u32>,
}

impl Launch {
    /// Waits for the status file to say more than `starting`.
    fn wait(&mut self, status_path: &Path, timeout: Duration) -> Result<Status, StartError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = Status::read(status_path) {
                if self.helper.is_none() && status.pid != 0 {
                    self.pid = Some(status.pid);
                }
                if status.state != State::Starting {
                    return Ok(status);
                }
            }
            if let Some(failure) = self.failed_early() {
                // It may have written its status and then ended.
                return match Status::read(status_path).filter(|s| s.state != State::Starting) {
                    Some(status) => Ok(status),
                    None => Err(failure),
                };
            }
            if Instant::now() >= deadline {
                return Err(StartError::Failed(NOT_IN_TIME.to_owned()));
            }
            thread::sleep(POLL);
        }
    }

    /// Whether the program ended, or `open` refused it, before reporting anything.
    fn failed_early(&mut self) -> Option<StartError> {
        if let Some(helper) = &mut self.helper
            && let Ok(Some(exit)) = helper.try_wait()
        {
            let log = self
                .log
                .as_deref()
                .and_then(|path| fs::read_to_string(path).ok())
                .unwrap_or_default();
            return Some(StartError::Failed(match last_line(&log) {
                Some(line) => format!("{STOPPED_AT_START} {line}"),
                None => format!("The screen sharing program stopped before it was ready ({exit})."),
            }));
        }
        if let Some(opener) = &mut self.opener
            && let Ok(Some(exit)) = opener.try_wait()
            && !exit.success()
        {
            return Some(refused(opener, exit));
        }
        None
    }

    /// Whether the program is gone.
    fn exited(&mut self) -> bool {
        if let Some(helper) = &mut self.helper {
            return !matches!(helper.try_wait(), Ok(None));
        }
        self.pid.is_none_or(|pid| !alive(pid))
    }

    fn wait_exit(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if self.exited() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(EXIT_POLL);
        }
    }

    /// Ends the program: asks it to stop first when `ask` is set, waits a
    /// moment for it to go, then forces it.
    fn end(&mut self, ask: bool) {
        if !self.exited() {
            let pid = self.helper.as_ref().map(Child::id).or(self.pid);
            if ask && let Some(pid) = pid {
                send(pid, Signal::TERM);
            }
            if !self.wait_exit(STOP_GRACE) {
                if let Some(pid) = pid {
                    log::warn!(
                        "the screen sharing program (process {pid}) did not stop; forcing it"
                    );
                    send(pid, Signal::KILL);
                }
                self.wait_exit(STOP_GRACE);
            }
        }
        self.release_opener();
    }

    fn release_opener(&mut self) {
        if let Some(opener) = self.opener.take() {
            reap(opener);
        }
    }
}

/// The error for an `open` that exited unsuccessfully, with its first line of complaint.
fn refused(opener: &mut Child, exit: ExitStatus) -> StartError {
    let mut text = String::new();
    if let Some(stderr) = &mut opener.stderr {
        let _ = stderr.by_ref().take(4096).read_to_string(&mut text);
    }
    let cause = first_line(&text).map_or_else(
        || format!("open exited with {exit}"),
        |line| line.trim_end_matches('.').to_owned(),
    );
    StartError::Failed(format!(
        "This Mac did not start the screen sharing program: {cause}. Someone needs to be logged \
         in on the Mac's own screen for it to start."
    ))
}

/// A program that is ready and serving a screen.
#[derive(Debug)]
pub struct Running {
    launch: Launch,
    folder: PathBuf,
    socket: PathBuf,
    status_path: PathBuf,
    pid: u32,
}

impl Running {
    /// Process number of the program.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The Unix socket the program accepts viewers on.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The folder this run keeps its files in.
    #[must_use]
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// The status file now; `None` when it cannot be read.
    #[must_use]
    pub fn status(&self) -> Option<Status> {
        Status::read(&self.status_path)
    }

    /// Whether the program is still there and still says it is serving.
    pub fn is_running(&mut self) -> bool {
        if self.launch.exited() {
            return false;
        }
        self.status()
            .is_none_or(|status| status.state == State::Ready)
    }

    /// Ends the program, waiting up to two seconds for it to go before forcing
    /// it, and removes the folder.
    pub fn stop(mut self) {
        let finished = self.status().is_some_and(|status| {
            matches!(
                status.state,
                State::Ended | State::Failed | State::NeedsPermission
            )
        });
        self.launch.end(!finished);
        let _ = fs::remove_dir_all(&self.folder);
    }
}

/// Why the program in `folder` ended, in its own words, when it said.
#[must_use]
pub fn ended_reason(folder: &Path) -> Option<String> {
    let status = Status::read(&folder.join(STATUS))?;
    match status.state {
        State::Ended | State::Failed | State::NeedsPermission => {
            Some(status.message).filter(|message| !message.is_empty())
        }
        State::Starting | State::Ready => None,
    }
}

/// Creates the private folder `name` (one path component) for a run: mode
/// 0700 and owned by this user, under `$TMPDIR` or `/tmp`. What an earlier run
/// left there is removed first. The socket inside it must fit the path
/// limit of Unix sockets, so a long `$TMPDIR` is passed over for `/tmp`.
///
/// # Errors
/// When it cannot be created, or something of another user is in the way.
pub fn private_folder(name: &str) -> io::Result<PathBuf> {
    let fallback = PathBuf::from("/tmp");
    let mut base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|base| base.is_absolute())
        .unwrap_or_else(|| fallback.clone());
    if base.join(name).join(SOCKET).as_os_str().len() > MAX_SOCKET_PATH {
        base = fallback;
    }
    let folder = base.join(name);
    let uid = rustix::process::getuid().as_raw();
    match fs::symlink_metadata(&folder) {
        Ok(metadata) if metadata.is_dir() => {
            if metadata.uid() != uid {
                return Err(io::Error::other(format!(
                    "{} belongs to another user",
                    folder.display()
                )));
            }
            fs::remove_dir_all(&folder)?;
        }
        Ok(_) => fs::remove_file(&folder)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::DirBuilder::new().mode(0o700).create(&folder)?;
    let metadata = fs::symlink_metadata(&folder)?;
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(io::Error::other(format!(
            "{} is not a private folder of this user",
            folder.display()
        )));
    }
    if metadata.mode() & 0o777 != 0o700 {
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
    }
    Ok(folder)
}

#[cfg(test)]
mod tests;
