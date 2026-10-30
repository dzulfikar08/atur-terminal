//! Finding the user's Chromium-based browser and its remote debugging port:
//! attaching to a running one, or starting it with the port open. The port
//! is only ever used on 127.0.0.1.

use std::io::{self, BufRead, IsTerminal, Write};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use muxy_app_core::browser::BrowserEngine;
use serde_json::Value;

use super::ws;

/// How long a browser has to open its debugging port after it starts.
const START_TIME: Duration = Duration::from_secs(30);
/// How long a browser has to quit when asked to.
const QUIT_TIME: Duration = Duration::from_secs(30);
const PROBE_TIME: Duration = Duration::from_secs(2);

/// Where the browser is and how to reach it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Browser {
    pub(super) port: u16,
    /// The path of its browser-level debugging socket.
    pub(super) socket_path: String,
}

/// What decides which browser is driven and where it keeps its profile.
#[derive(Debug)]
pub(in crate::browser) struct Config {
    pub(super) engine: BrowserEngine,
    /// The browser's executable, if not to be found.
    pub(super) path: Option<PathBuf>,
    /// The profile folder, if not the browser's normal one.
    pub(super) user_data_dir: Option<PathBuf>,
    /// More flags for a browser this starts.
    pub(super) extra_args: Vec<String>,
    /// A loopback debugging port to use as is.
    pub(super) endpoint: Option<u16>,
    /// Restart a running browser without a debugging port, without asking.
    pub(super) restart: bool,
}

impl Config {
    /// The configuration for `engine` from the environment and the `path`
    /// setting; the environment wins.
    pub(in crate::browser) fn new(
        engine: BrowserEngine,
        path: &str,
        restart: bool,
    ) -> Result<Self, String> {
        let var = |name: &str| muxy_core::env::var(name).filter(|value| !value.trim().is_empty());
        let path = var("ATUR_TERM_BROWSER_PATH")
            .or_else(|| Some(path.to_owned()).filter(|path| !path.trim().is_empty()))
            .map(PathBuf::from);
        let endpoint = var("ATUR_TERM_BROWSER_ENDPOINT")
            .map(|text| parse_endpoint(&text))
            .transpose()?;
        Ok(Self {
            engine,
            path,
            user_data_dir: var("ATUR_TERM_BROWSER_USER_DATA_DIR").map(PathBuf::from),
            extra_args: var("ATUR_TERM_BROWSER_ARGS")
                .map(|args| args.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            endpoint,
            restart,
        })
    }

    fn name(&self) -> &'static str {
        self.engine.name()
    }

    fn data_dir(&self) -> Option<PathBuf> {
        self.user_data_dir
            .clone()
            .or_else(|| default_data_dir(self.engine, std::env::home_dir()?.as_path()))
    }
}

/// A loopback port from `host:port`, `http://host:port` or a bare port.
/// Anything not on this computer is refused: the debugging port gives
/// whoever reaches it full control of the browser.
pub(super) fn parse_endpoint(text: &str) -> Result<u16, String> {
    let text = text.trim();
    let text = text
        .strip_prefix("http://")
        .or_else(|| text.strip_prefix("ws://"))
        .unwrap_or(text);
    let authority = text.split('/').next().unwrap_or_default();
    let (host, port) = authority
        .rsplit_once(':')
        .map_or(("127.0.0.1", authority), |(host, port)| (host, port));
    if !is_loopback_host(host) {
        return Err(format!(
            "the browser endpoint {text} is not on this computer; only 127.0.0.1, localhost and ::1 are allowed"
        ));
    }
    port.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| format!("invalid browser endpoint {text}; use 127.0.0.1:PORT"))
}

pub(super) fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.trim_matches(['[', ']']),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// The profile folder a browser uses unless told otherwise.
pub(super) fn default_data_dir(engine: BrowserEngine, home: &Path) -> Option<PathBuf> {
    let relative = match (engine, cfg!(target_os = "macos")) {
        (BrowserEngine::Helium, false) => ".config/net.imput.helium",
        (BrowserEngine::Helium, true) => "Library/Application Support/net.imput.helium",
        (BrowserEngine::Chromium, false) => ".config/chromium",
        (BrowserEngine::Chromium, true) => "Library/Application Support/Chromium",
        (BrowserEngine::Chrome, false) => ".config/google-chrome",
        (BrowserEngine::Chrome, true) => "Library/Application Support/Google/Chrome",
        (BrowserEngine::Builtin, _) => return None,
    };
    Some(home.join(relative))
}

/// The executable names a browser's main process may have, lower case.
fn process_names(engine: BrowserEngine) -> &'static [&'static str] {
    match engine {
        BrowserEngine::Helium => &["helium", "helium-browser"],
        BrowserEngine::Chromium => &["chromium", "chromium-browser"],
        BrowserEngine::Chrome => &[
            "chrome",
            "google-chrome",
            "google-chrome-stable",
            "google chrome",
        ],
        BrowserEngine::Builtin => &[],
    }
}

/// The commands to try, in order, when looking for the browser.
fn command_names(engine: BrowserEngine) -> &'static [&'static str] {
    match engine {
        BrowserEngine::Helium => &["helium-browser", "helium"],
        BrowserEngine::Chromium => &["chromium", "chromium-browser"],
        BrowserEngine::Chrome => &["google-chrome-stable", "google-chrome", "chrome"],
        BrowserEngine::Builtin => &[],
    }
}

/// Where the browser's app or package puts it, besides the search path.
fn fixed_paths(engine: BrowserEngine) -> &'static [&'static str] {
    match engine {
        BrowserEngine::Helium => &[
            "/Applications/Helium.app/Contents/MacOS/Helium",
            "/opt/helium-browser-bin/helium-browser",
            "/opt/helium-browser-bin/helium",
        ],
        BrowserEngine::Chromium => &["/Applications/Chromium.app/Contents/MacOS/Chromium"],
        BrowserEngine::Chrome => &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/opt/google/chrome/chrome",
        ],
        BrowserEngine::Builtin => &[],
    }
}

/// `command` in the folders of `search_path`, if it is an executable file.
fn find_in(search_path: &std::ffi::OsStr, command: &str) -> Option<PathBuf> {
    std::env::split_paths(search_path)
        .map(|folder| folder.join(command))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn find_binary(config: &Config) -> Result<PathBuf, String> {
    if let Some(path) = &config.path {
        if path.components().count() > 1 {
            return if is_executable(path) {
                Ok(path.clone())
            } else {
                Err(format!("{} is not an executable file", path.display()))
            };
        }
        let search = std::env::var_os("PATH").unwrap_or_default();
        return find_in(&search, &path.to_string_lossy())
            .ok_or_else(|| format!("could not find {} on the search path", path.display()));
    }
    let search = std::env::var_os("PATH").unwrap_or_default();
    command_names(config.engine)
        .iter()
        .find_map(|name| find_in(&search, name))
        .or_else(|| {
            fixed_paths(config.engine)
                .iter()
                .map(PathBuf::from)
                .find(|path| is_executable(path))
        })
        .ok_or_else(|| {
            format!(
                "could not find {}; set ATUR_TERM_BROWSER_PATH or Settings → Browser → Browser path",
                config.name()
            )
        })
}

/// A process from `ps`.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Process {
    pub(super) pid: u32,
    pub(super) command: String,
}

/// The processes listed by `ps -axo pid=,args=`.
pub(super) fn parse_processes(output: &str) -> Vec<Process> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, command) = line.split_once(char::is_whitespace)?;
            Some(Process {
                pid: pid.parse().ok()?,
                command: command.trim().to_owned(),
            })
        })
        .collect()
}

fn processes() -> Vec<Process> {
    Command::new("ps")
        .args(["-axo", "pid=,args="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|output| parse_processes(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default()
}

/// The executable part of a command line: what comes before the first flag.
fn executable(command: &str) -> &str {
    command.split(" --").next().unwrap_or(command)
}

/// The value of `--name=value` in a command line.
pub(super) fn flag_value<'a>(command: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!(" --{name}=");
    let start = command.find(&marker)? + marker.len();
    let rest = &command[start..];
    Some(rest.split(" --").next().unwrap_or(rest).trim())
}

/// The first word of the value of `--name=value`: for flags that never hold
/// spaces, so a page address after the flags is not taken for part of it.
fn flag_word<'a>(command: &'a str, name: &str) -> Option<&'a str> {
    flag_value(command, name)?.split_whitespace().next()
}

/// Whether the folder in a `--user-data-dir` value, which may be followed by
/// a page address, is `dir`.
fn is_folder(value: &str, dir: &Path) -> bool {
    if Path::new(value) == dir {
        return true;
    }
    let dir = dir.to_string_lossy();
    value
        .strip_prefix(dir.as_ref())
        .is_some_and(|rest| rest.starts_with(' '))
}

/// The main process of the browser using `data_dir`, not one of its helpers.
pub(super) fn main_process<'a>(
    processes: &'a [Process],
    names: &[&str],
    data_dir: Option<&Path>,
    default_dir: Option<&Path>,
) -> Option<&'a Process> {
    processes.iter().find(|process| {
        let command = &process.command;
        let program = Path::new(executable(command))
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !names.contains(&program.as_str()) || command.contains(" --type=") {
            return false;
        }
        let Some(wanted) = data_dir else {
            return true;
        };
        match flag_value(command, "user-data-dir") {
            Some(value) => is_folder(value, wanted),
            None => default_dir == Some(wanted),
        }
    })
}

/// How a running browser's command line configures remote debugging.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Debugging {
    /// Not turned on.
    Off,
    /// On at this port; zero means the browser picked one and wrote it to
    /// its `DevToolsActivePort` file.
    Port(u16),
    /// On, but bound to an address beyond this computer.
    Exposed,
}

pub(super) fn debugging(command: &str) -> Debugging {
    if let Some(address) = flag_word(command, "remote-debugging-address")
        && !is_loopback_host(address)
    {
        return Debugging::Exposed;
    }
    match flag_word(command, "remote-debugging-port").map(str::parse::<u16>) {
        Some(Ok(port)) => Debugging::Port(port),
        _ => Debugging::Off,
    }
}

/// The port a browser that picked its own wrote to `data_dir`.
fn announced_port(data_dir: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(data_dir.join("DevToolsActivePort")).ok()?;
    text.lines()
        .next()?
        .trim()
        .parse()
        .ok()
        .filter(|port| *port != 0)
}

/// The browser answering on loopback `port`, if one does.
pub(super) fn probe(port: u16) -> Option<Browser> {
    let body = ws::http_get(port, "/json/version", PROBE_TIME).ok()?;
    let version: Value = serde_json::from_str(&body).ok()?;
    let socket_path = socket_path(version.get("webSocketDebuggerUrl")?.as_str()?)?;
    Some(Browser { port, socket_path })
}

/// The path of a debugging socket address, if it is on this computer.
pub(super) fn socket_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("ws://")?;
    let (authority, path) = rest.split_once('/')?;
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    is_loopback_host(host).then(|| format!("/{path}"))
}

/// Attaches to the browser, starting it if it isn't running.
pub(super) fn attach(config: &Config) -> Result<Browser, String> {
    if let Some(port) = config.endpoint {
        return probe(port).ok_or_else(|| {
            format!("nothing answers as a browser on 127.0.0.1:{port} (ATUR_TERM_BROWSER_ENDPOINT)")
        });
    }
    let data_dir = config.data_dir();
    let default_dir = default_data_dir(config.engine, &std::env::home_dir().unwrap_or_default());
    let mut names = process_names(config.engine).to_vec();
    let custom = config
        .path
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_lowercase());
    if let Some(custom) = custom.as_deref() {
        names.push(custom);
    }
    let running = processes();
    let Some(process) = main_process(
        &running,
        &names,
        data_dir.as_deref(),
        default_dir.as_deref(),
    ) else {
        return start(config, &[]);
    };
    match debugging(&process.command) {
        Debugging::Exposed => Err(format!(
            "{} has its debugging port open beyond this computer; atur-term only uses loopback. \
             Restart it with --remote-debugging-address=127.0.0.1",
            config.name()
        )),
        Debugging::Port(port) => {
            let port = if port == 0 {
                data_dir.as_deref().and_then(announced_port).unwrap_or(0)
            } else {
                port
            };
            probe(port).ok_or_else(|| {
                format!(
                    "{} has remote debugging on, but nothing answers on 127.0.0.1:{port}",
                    config.name()
                )
            })
        }
        Debugging::Off => restart_without_debugging(config, process),
    }
}

/// Handles a running browser that can't be attached to: restarts it when the
/// user allows, else says how.
fn restart_without_debugging(config: &Config, process: &Process) -> Result<Browser, String> {
    if config.restart || confirm_restart(config.name()) {
        let _ = writeln!(
            io::stderr(),
            "atur-term: restarting {} with remote debugging; your tabs are restored",
            config.name()
        );
        quit(&process.command, process.pid)?;
        return start(config, &["--restore-last-session"]);
    }
    Err(restart_advice(config.name(), command_names(config.engine)))
}

pub(super) fn restart_advice(name: &str, commands: &[&str]) -> String {
    let command = commands.first().copied().unwrap_or("the browser");
    format!(
        "{name} is running without remote debugging, so agents can't attach to it, and a second \
         copy can't share your profile. Choose one:\n\
         \x20 - Let atur-term restart it with debugging on (tabs are restored): \
         atur-term browser --restart-browser open <url>\n\
         \x20 - Restart it yourself once: {command} --remote-debugging-port=9222 \
         --remote-debugging-address=127.0.0.1\n\
         \x20 - Make it permanent: put those two flags in the browser's flags file or desktop entry \
         (see docs/features/browser.md)\n\
         atur-term never restarts {name} on its own."
    )
}

/// Asks at a terminal whether to restart the browser.
fn confirm_restart(name: &str) -> bool {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return false;
    }
    let _ = write!(
        io::stderr(),
        "{name} is running without remote debugging. Restart it with debugging on? \
         Your tabs are restored. [y/N] "
    );
    let _ = io::stderr().flush();
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer).is_ok()
        && matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Asks the browser to quit, as closing its session would, and waits for it.
fn quit(command: &str, pid: u32) -> Result<(), String> {
    let pid_text = pid.to_string();
    let asked = if cfg!(target_os = "macos") {
        let app = Path::new(executable(command))
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Command::new("osascript")
            .args(["-e", &format!("tell application \"{app}\" to quit")])
            .status()
    } else {
        Command::new("kill").args(["-TERM", &pid_text]).status()
    };
    if !asked.is_ok_and(|status| status.success()) {
        return Err("could not ask the browser to quit".into());
    }
    let deadline = Instant::now() + QUIT_TIME;
    while alive(&pid_text) {
        if Instant::now() >= deadline {
            return Err(
                "the browser did not quit; close it yourself and run the command again".into(),
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// A loopback port nothing listens on.
fn free_port() -> io::Result<u16> {
    Ok(TcpListener::bind(ws::loopback(0))?.local_addr()?.port())
}

/// The command line that starts the browser with remote debugging on
/// `port`, on loopback only.
pub(super) fn launch_args(config: &Config, port: u16, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        format!("--remote-debugging-port={port}"),
        "--remote-debugging-address=127.0.0.1".to_owned(),
    ];
    if let Some(dir) = &config.user_data_dir {
        args.push(format!("--user-data-dir={}", dir.display()));
    }
    args.extend(config.extra_args.iter().cloned());
    args.extend(extra.iter().map(|flag| (*flag).to_owned()));
    args
}

/// Starts the browser on the user's profile with remote debugging, apart
/// from this process, and waits for its port.
fn start(config: &Config, extra: &[&str]) -> Result<Browser, String> {
    let binary = find_binary(config)?;
    let port = free_port().map_err(|error| format!("no free port: {error}"))?;
    let mut child: Child = Command::new(&binary)
        .args(launch_args(config, port, extra))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", binary.display()))?;
    let deadline = Instant::now() + START_TIME;
    loop {
        if let Some(browser) = probe(port) {
            return Ok(browser);
        }
        if let Ok(Some(status)) = child.try_wait() {
            // A browser that hands off to a running one exits at once.
            return probe(port).ok_or_else(|| {
                format!(
                    "{} exited ({status}) before opening its debugging port. If it is already \
                     running, it was started without remote debugging; see \
                     docs/features/browser.md",
                    config.name()
                )
            });
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{} did not open its debugging port. Chrome 136 and later ignore it for the \
                 default profile folder; set ATUR_TERM_BROWSER_USER_DATA_DIR to another folder",
                config.name()
            ));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "\
  913617 /opt/helium-browser-bin/helium
  913693 /opt/helium-browser-bin/helium_crashpad_handler --monitor-self --database=/home/u/.config/net.imput.helium/Crash Reports
  913700 /opt/helium-browser-bin/helium --type=renderer --user-data-dir=/home/u/.config/net.imput.helium
 2000100 /usr/bin/chromium --user-data-dir=/tmp/profile --remote-debugging-port=0 --remote-debugging-address=127.0.0.1
 3000000 /Applications/Helium.app/Contents/MacOS/Helium --remote-debugging-port=9222
";

    #[test]
    fn processes_are_read_from_ps() {
        let all = parse_processes(PS);
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].pid, 913_617);
        assert_eq!(all[0].command, "/opt/helium-browser-bin/helium");
    }

    #[test]
    fn only_the_main_process_of_the_right_profile_counts() {
        let all = parse_processes(PS);
        let home = Path::new("/home/u/.config/net.imput.helium");
        let helium = main_process(&all, &["helium", "helium-browser"], Some(home), Some(home));
        assert_eq!(helium.map(|process| process.pid), Some(913_617));
        let other = Path::new("/tmp/profile");
        let chromium = main_process(&all, &["chromium"], Some(other), Some(home));
        assert_eq!(chromium.map(|process| process.pid), Some(2_000_100));
        assert!(main_process(&all, &["chromium"], Some(home), Some(home)).is_none());
        let mac = main_process(&all, &["helium"], None, None);
        assert_eq!(mac.map(|process| process.pid), Some(913_617));
        assert!(main_process(&all[1..3], &["helium"], None, None).is_none());
    }

    #[test]
    fn the_debugging_flags_are_read_from_a_command_line() {
        assert_eq!(debugging("/opt/helium-browser-bin/helium"), Debugging::Off);
        assert_eq!(
            debugging("/usr/bin/chromium --remote-debugging-port=9222"),
            Debugging::Port(9222)
        );
        assert_eq!(
            debugging(
                "/usr/bin/chromium --remote-debugging-port=0 --remote-debugging-address=127.0.0.1 --x"
            ),
            Debugging::Port(0)
        );
        assert_eq!(
            debugging(
                "/usr/bin/chromium --remote-debugging-port=9222 --remote-debugging-address=0.0.0.0"
            ),
            Debugging::Exposed
        );
        assert_eq!(
            flag_value(
                "/a/b --user-data-dir=/home/u/My Profile --x=1",
                "user-data-dir"
            ),
            Some("/home/u/My Profile")
        );
    }

    #[test]
    fn endpoints_must_be_on_this_computer() -> Result<(), String> {
        assert_eq!(parse_endpoint("127.0.0.1:9222")?, 9222);
        assert_eq!(parse_endpoint("http://localhost:9333/json")?, 9333);
        assert_eq!(parse_endpoint("[::1]:9444")?, 9444);
        assert_eq!(parse_endpoint("9555")?, 9555);
        for refused in [
            "0.0.0.0:9222",
            "192.168.1.5:9222",
            "http://example.com:9222",
            "127.0.0.1:0",
            "127.0.0.1:x",
        ] {
            assert!(parse_endpoint(refused).is_err(), "{refused}");
        }
        Ok(())
    }

    #[test]
    fn socket_addresses_off_this_computer_are_refused() {
        assert_eq!(
            socket_path("ws://127.0.0.1:9222/devtools/browser/abc"),
            Some("/devtools/browser/abc".to_owned())
        );
        assert_eq!(
            socket_path("ws://localhost:9222/devtools/browser/abc"),
            Some("/devtools/browser/abc".to_owned())
        );
        assert_eq!(socket_path("ws://10.0.0.2:9222/devtools/browser/abc"), None);
        assert_eq!(socket_path("wss://127.0.0.1:9222/x"), None);
    }

    #[test]
    fn the_browser_starts_on_loopback_with_the_users_flags() {
        let config = Config {
            engine: BrowserEngine::Helium,
            path: None,
            user_data_dir: Some(PathBuf::from("/tmp/p")),
            extra_args: vec!["--headless=new".into()],
            endpoint: None,
            restart: false,
        };
        assert_eq!(
            launch_args(&config, 4242, &["--restore-last-session"]),
            [
                "--remote-debugging-port=4242",
                "--remote-debugging-address=127.0.0.1",
                "--user-data-dir=/tmp/p",
                "--headless=new",
                "--restore-last-session",
            ]
        );
        let normal = Config {
            user_data_dir: None,
            extra_args: Vec::new(),
            ..config
        };
        assert!(
            !launch_args(&normal, 1, &[])
                .iter()
                .any(|flag| flag.starts_with("--user-data-dir"))
        );
    }

    #[test]
    fn each_engine_has_its_own_profile_folder() {
        let home = Path::new("/home/u");
        assert_eq!(default_data_dir(BrowserEngine::Builtin, home), None);
        let helium = default_data_dir(BrowserEngine::Helium, home);
        assert!(helium.is_some_and(|path| path.ends_with("net.imput.helium")));
    }

    #[test]
    fn advice_names_the_choices_and_the_promise() {
        let advice = restart_advice("Helium", &["helium-browser"]);
        assert!(advice.contains("--restart-browser"));
        assert!(advice.contains("helium-browser --remote-debugging-port=9222"));
        assert!(advice.contains("never restarts Helium on its own"));
    }
}
