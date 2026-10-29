use std::net::{SocketAddr, TcpListener};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use atur_mobile::{
    Connection, ConnectionEvent, ConnectionListener, Direct, DirectListener, GitChangeKind,
    GitDiffKind, Key, Line, MobileError, Modifiers, MouseButton, ScrollDirection, ServerCredential,
    direct_pair, direct_parse_link,
};
use muxy_client::Client;
use muxy_protocol::transport::Listener;
use muxy_protocol::transport::tls::TlsListener;
use muxy_protocol::{
    ListenerStatus, OperationId, ProjectDescriptor, ProjectId, ProjectIntent, ProjectMutation,
    RemoteAccessSettings, ServerPath,
};
use muxy_server::connection::{serve, serve_remote};
use muxy_server::{Registry, ServerEvent, ServerSettings, UdpOffers};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const TIMEOUT: Duration = Duration::from_secs(10);

/// An address the test server reports for itself that no phone can reach.
const SERVER_ADDRESS: &str = "192.168.43.7";

type Subscribers = Arc<Mutex<Vec<Sender<ServerEvent>>>>;

/// An in-process server whose network listener is a real TLS listener on loopback.
struct Server {
    registry: Arc<Registry>,
    subscribers: Subscribers,
}

/// Forwards server events such as session ends to every connection, as the executable does.
fn subscribe(subscribers: &Subscribers) -> Receiver<ServerEvent> {
    let (sender, events) = mpsc::channel();
    subscribers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(sender);
    events
}

impl Server {
    /// Serves UDP offers on `ATUR_TERM_UDP_PORTS`, like the executable.
    fn new() -> Self {
        Self::build(true)
    }

    /// Answers `OfferUdp` as a server that turned UDP off does.
    fn without_udp() -> Self {
        Self::build(false)
    }

    fn build(udp: bool) -> Self {
        let (sender, events) = mpsc::channel::<ServerEvent>();
        let subscribers = Subscribers::default();
        let sinks = Arc::clone(&subscribers);
        thread::spawn(move || {
            for event in events {
                sinks
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .retain(|sink| sink.send(event.clone()).is_ok());
            }
        });
        let slot = Arc::new(OnceLock::<Weak<Registry>>::new());
        let owner = Arc::clone(&slot);
        let remote_subscribers = Arc::clone(&subscribers);
        let running = Mutex::new(None::<Arc<TlsListener>>);
        let udp_subscribers = Arc::clone(&subscribers);
        let mut registry = Registry::new(
            ServerSettings {
                default_shell: Some(PathBuf::from("/bin/sh")),
                ..ServerSettings::default()
            },
            sender,
        )
        // What a paired phone is told about the computer's addresses: a new network's
        // address, and the loopback one this test server is really reached at.
        .with_address_source(|| vec![SERVER_ADDRESS.to_owned(), "127.0.0.1".to_owned()]);
        let offers = udp
            .then(|| {
                UdpOffers::from_environment(move |stream, registry, admission| {
                    let events = subscribe(&udp_subscribers);
                    thread::spawn(move || {
                        let _ = serve_remote(stream, registry, events, admission);
                    });
                    Ok(())
                })
            })
            .flatten();
        if let Some(offers) = offers {
            registry = registry.with_udp_offers(offers);
        }
        let registry = Arc::new(registry.with_remote_listener(move |listening| {
            let mut running = running.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(previous) = running.take() {
                previous.close();
            }
            let Some((port, identity)) = listening else {
                return ListenerStatus::Disabled;
            };
            match TlsListener::bind(SocketAddr::from(([127, 0, 0, 1], port)), &identity) {
                Ok(listener) => {
                    let listener = Arc::new(listener);
                    *running = Some(Arc::clone(&listener));
                    let registry = owner.get().cloned().unwrap_or_default();
                    let subscribers = Arc::clone(&remote_subscribers);
                    thread::spawn(move || accept(&listener, &registry, &subscribers));
                    ListenerStatus::Listening
                }
                Err(error) => ListenerStatus::Failed(error.to_string()),
            }
        }));
        let _ = slot.set(Arc::downgrade(&registry));
        Self {
            registry,
            subscribers,
        }
    }

    fn local(&self) -> TestResult<Client> {
        let (client, server) = UnixStream::pair()?;
        let registry = Arc::clone(&self.registry);
        let events = subscribe(&self.subscribers);
        thread::spawn(move || {
            let _ = serve(Box::new(server), registry, events);
        });
        Ok(Client::from_stream(Box::new(client))?)
    }
}

fn accept(listener: &TlsListener, registry: &Weak<Registry>, subscribers: &Subscribers) {
    while let Ok(stream) = listener.accept() {
        let Some(registry) = registry.upgrade() else {
            return;
        };
        let Some(admission) = registry.admit_remote() else {
            continue;
        };
        let events = subscribe(subscribers);
        thread::spawn(move || {
            let _ = serve_remote(stream, registry, events, admission);
        });
    }
}

struct Recorder(Mutex<Sender<ConnectionEvent>>);

impl ConnectionListener for Recorder {
    fn on_event(&self, event: ConnectionEvent) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(event);
    }
}

fn listener() -> (Arc<Recorder>, Receiver<ConnectionEvent>) {
    let (sender, events) = mpsc::channel();
    (Arc::new(Recorder(Mutex::new(sender))), events)
}

fn wait_for(
    events: &Receiver<ConnectionEvent>,
    mut done: impl FnMut(&ConnectionEvent) -> bool,
) -> TestResult {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let event = events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        if done(&event) {
            return Ok(());
        }
    }
}

/// Waits until `done` holds, checking now and after each event.
fn wait_until(events: &Receiver<ConnectionEvent>, mut done: impl FnMut() -> bool) -> TestResult {
    if done() {
        return Ok(());
    }
    wait_for(events, |_| done())
}

fn metadata_changed(event: &ConnectionEvent, session: u64) -> bool {
    matches!(event, ConnectionEvent::MetadataChanged { session_id } if *session_id == session)
}

/// A temporary folder, removed when dropped.
struct Folder(PathBuf);

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A new folder that the computer registers as a project; returns it with the project's id.
fn project_folder(local: &Client) -> TestResult<(Folder, String)> {
    let folder = Folder(std::env::temp_dir().join(format!("atur-sdk-{}", OperationId::new())));
    std::fs::create_dir(&folder.0)?;
    let project = ProjectId::new();
    local.mutate_project(ProjectIntent {
        operation: OperationId::new(),
        mutation: ProjectMutation::Create(ProjectDescriptor {
            id: project,
            home: false,
            directory: ServerPath(folder.0.as_os_str().as_bytes().to_vec()),
            name: "Phone".into(),
            icon: None,
            logo: None,
            color: "#ffffff".into(),
            kind: None,
            parent_id: None,
        }),
    })?;
    Ok((folder, project.to_string()))
}

fn init_repository(folder: &Path) -> TestResult {
    for args in [
        &["init", "-b", "main"][..],
        &["config", "user.name", "Test"],
        &["config", "user.email", "test@example.invalid"],
        &["config", "commit.gpgsign", "false"],
    ] {
        run_git(folder, args)?;
    }
    Ok(())
}

fn run_git(folder: &Path, args: &[&str]) -> TestResult {
    let output = Command::new("git")
        .args(args)
        .current_dir(folder)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into())
    }
}

/// Pairs through the SDK and returns the saved credential plus the local admin client.
/// Turns on mobile access at a free port. Tests running alongside can take
/// the port before the server listens on it, so a taken port is replaced.
fn listen(local: &Client) -> TestResult<u16> {
    let mut status = ListenerStatus::Disabled;
    for _ in 0..8 {
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        status = local
            .write_remote_access(RemoteAccessSettings {
                enabled: true,
                port,
            })?
            .status;
        if status == ListenerStatus::Listening {
            return Ok(port);
        }
    }
    Err(format!("mobile access never listened: {status:?}").into())
}

/// Turns on mobile access at `port`, which a network shaper was told about.
fn listen_at(local: &Client, port: u16) -> TestResult {
    let status = local
        .write_remote_access(RemoteAccessSettings {
            enabled: true,
            port,
        })?
        .status;
    if status == ListenerStatus::Listening {
        Ok(())
    } else {
        Err(format!("mobile access never listened on {port}: {status:?}").into())
    }
}

fn paired() -> TestResult<(Server, Client, ServerCredential)> {
    let server = Server::new();
    let local = server.local()?;
    let port = listen(&local)?;
    let mut offer = local.start_pairing()?;
    offer.invite.hosts = vec!["127.0.0.1".into()];
    let link = offer.invite.to_link();
    let preview = atur_mobile::parse_pairing_link(link.clone())?;
    assert_eq!(preview.port, port);
    let credential = atur_mobile::pair(link, "  Test phone\n".into())?;
    assert_eq!(credential.fingerprint.len(), 32);
    Ok((server, local, credential))
}

#[test]
fn a_phone_pairs_opens_a_shell_and_types() -> TestResult {
    let (_server, local, credential) = paired()?;
    assert_eq!(local.read_remote_access()?.devices[0].name, "Test phone");
    let (recorder, events) = listener();
    let connection = Connection::connect(credential, recorder)?;
    let projects = connection.projects()?;
    let home = projects
        .iter()
        .find(|project| project.is_home)
        .ok_or("no Home project")?;
    let session = connection.create_session(home.id.clone(), 40, 6)?;
    let terminal = connection.attach(session.id, 40, 6)?;
    assert_eq!(terminal.screen().columns, 40);
    terminal.send_input(b"printf '\\nmobile-%d\\n' $((40+2))".to_vec())?;
    terminal.send_key(Key::Enter, Modifiers::default())?;
    let text = || -> Vec<String> {
        terminal
            .screen()
            .lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect()
    };
    wait_for(&events, |event| {
        matches!(event, ConnectionEvent::ScreenChanged { session_id } if *session_id == session.id)
            && text().iter().any(|line| line.trim() == "mobile-42")
    })
    .map_err(|error| format!("{error}; screen: {:?}", text()))?;
    let sessions = connection.sessions(home.id.clone())?;
    assert!(
        sessions
            .iter()
            .any(|listed| listed.id == session.id && listed.attached)
    );
    terminal.detach()?;
    connection.end_session(session.id)?;
    wait_for(
        &events,
        |event| matches!(event, ConnectionEvent::SessionEnded { session_id } if *session_id == session.id),
    )?;
    Ok(())
}

#[test]
fn the_phone_tracks_desktop_resizes_without_resizing_the_session() -> TestResult {
    let (_server, local, credential) = paired()?;
    let (recorder, events) = listener();
    let connection = Connection::connect(credential, recorder)?;
    let home = connection
        .projects()?
        .into_iter()
        .find(|project| project.is_home)
        .ok_or("no Home project")?;
    let session = connection.create_session(home.id, 40, 6)?;
    let id = muxy_protocol::SessionId::new(session.id).ok_or("invalid session")?;
    let desktop = local.attach(id, muxy_protocol::Size { cols: 40, rows: 6 })?;
    let terminal = connection.attach(session.id, 20, 3)?;
    assert_eq!((terminal.screen().columns, terminal.screen().rows), (40, 6));
    let result = (|| -> TestResult {
        for (columns, rows) in [(70, 10), (20, 3), (55, 3), (55, 8)] {
            local.resize(
                desktop.channel,
                muxy_protocol::Size {
                    cols: columns,
                    rows,
                },
            )?;
            wait_until(&events, || {
                let screen = terminal.screen();
                (screen.columns, screen.rows) == (columns, rows)
                    && screen.lines.len() == usize::from(rows)
            })?;
        }
        Ok(())
    })();
    connection.end_session(session.id)?;
    result
}

fn texts(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .collect()
}

#[test]
fn scrollback_reads_every_line_while_new_output_arrives() -> TestResult {
    let (_server, _local, credential) = paired()?;
    let (recorder, events) = listener();
    let connection = Connection::connect(credential, recorder)?;
    let home = connection
        .projects()?
        .into_iter()
        .find(|project| project.is_home)
        .ok_or("no Home project")?;
    let session = connection.create_session(home.id, 30, 5)?;
    let terminal = connection.attach(session.id, 30, 5)?;
    let on_screen = |text: &str| {
        texts(&terminal.screen().lines)
            .iter()
            .any(|line| line == text)
    };
    terminal.send_input(
        b"printf '\\n'; i=1; while [ $i -le 120 ]; do echo line-$i; i=$((i+1)); done\r".to_vec(),
    )?;
    wait_for(&events, |_| on_screen("line-120"))?;
    let scrollback = terminal.scrollback(50)?;
    terminal.send_input(b"printf '\\nafter-%d\\n' $((1+1))\r".to_vec())?;
    wait_for(&events, |_| on_screen("after-2"))?;
    let mut lines = texts(&scrollback.lines());
    loop {
        let older = scrollback.load_older(50)?;
        if older.is_empty() {
            break;
        }
        lines.splice(0..0, texts(&older));
    }
    let numbered: Vec<_> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("line-")?.parse::<u32>().ok())
        .collect();
    assert_eq!(numbered, (1..=120).collect::<Vec<_>>());
    assert!(!lines.iter().any(|line| line == "after-2"));
    connection.end_session(session.id)?;
    Ok(())
}

#[test]
fn terminals_attached_at_the_same_time_all_receive_their_screens() -> TestResult {
    let (_server, _local, credential) = paired()?;
    let (recorder, events) = listener();
    let connection = Connection::connect(credential, recorder)?;
    let home = connection
        .projects()?
        .into_iter()
        .find(|project| project.is_home)
        .ok_or("no Home project")?;
    let sessions: Vec<_> = (0..4)
        .map(|_| connection.create_session(home.id.clone(), 30, 4))
        .collect::<Result<_, _>>()?;
    let terminals: Vec<_> = thread::scope(|scope| {
        let attaching: Vec<_> = sessions
            .iter()
            .map(|session| scope.spawn(|| connection.attach(session.id, 30, 4)))
            .collect();
        attaching
            .into_iter()
            .map(|attach| attach.join().map_err(|_| "attach panicked"))
            .collect::<Result<Vec<_>, _>>()
    })?
    .into_iter()
    .collect::<Result<_, _>>()?;
    for (index, terminal) in terminals.iter().enumerate() {
        terminal.send_input(format!("printf '\\nready-%d\\n' {index}\r").into_bytes())?;
    }
    let mut ready = vec![false; terminals.len()];
    let deadline = Instant::now() + TIMEOUT;
    while ready.contains(&false) {
        events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        for (index, terminal) in terminals.iter().enumerate() {
            ready[index] |= terminal.screen().lines.iter().any(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
                    .trim()
                    == format!("ready-{index}")
            });
        }
    }
    for session in sessions {
        connection.end_session(session.id)?;
    }
    Ok(())
}

#[test]
fn taps_and_scrolling_reach_a_program_that_tracks_the_mouse() -> TestResult {
    let (_server, _local, credential) = paired()?;
    let (recorder, events) = listener();
    let connection = Connection::connect(credential, recorder)?;
    let home = connection
        .projects()?
        .into_iter()
        .find(|project| project.is_home)
        .ok_or("no Home project")?;
    let session = connection.create_session(home.id, 40, 6)?;
    let terminal = connection.attach(session.id, 40, 6)?;
    assert!(!terminal.screen().mouse_tracking);
    // Button tracking with SGR reports, printed as soon as each one arrives.
    terminal.send_input(
        b"PS1='mouse-ready> '; stty -icanon -echo; printf '\\033[?1000h\\033[?1006h'; cat -v\r"
            .to_vec(),
    )?;
    wait_for(&events, |event| {
        metadata_changed(event, session.id) && terminal.screen().mouse_tracking
    })
    .map_err(|error| {
        format!(
            "enable mouse reporting: {error}; {:?}",
            texts(&terminal.screen().lines)
        )
    })?;
    terminal.click(MouseButton::Left, 2, 4, Modifiers::default())?;
    terminal.scroll(ScrollDirection::Up, 2, 4)?;
    wait_until(&events, || {
        texts(&terminal.screen().lines)
            .iter()
            .any(|line| line.contains("^[[<0;5;3M^[[<0;5;3m^[[<64;5;3M"))
    })
    .map_err(|error| {
        format!(
            "read mouse reports: {error}; {:?}",
            texts(&terminal.screen().lines)
        )
    })?;

    // Attaching again shows the mode the program already turned on.
    terminal.detach()?;
    let terminal = connection.attach(session.id, 40, 6)?;
    wait_until(&events, || terminal.screen().mouse_tracking)?;
    let control = Modifiers {
        control: true,
        ..Modifiers::default()
    };
    terminal.send_key(Key::Character { text: "c".into() }, control)?;
    wait_until(&events, || {
        texts(&terminal.screen().lines)
            .iter()
            .any(|line| line.trim() == "mouse-ready>")
    })?;
    terminal.send_input(b"printf '\\033[?1000l'\r".to_vec())?;
    wait_for(&events, |event| {
        metadata_changed(event, session.id) && !terminal.screen().mouse_tracking
    })
    .map_err(|error| {
        format!(
            "disable mouse reporting: {error}; {:?}",
            texts(&terminal.screen().lines)
        )
    })?;
    connection.end_session(session.id)?;
    Ok(())
}

#[test]
fn a_phone_reads_and_changes_a_repository() -> TestResult {
    let (_server, local, credential) = paired()?;
    let (folder, project) = project_folder(&local)?;
    init_repository(&folder.0)?;
    let (recorder, events) = listener();
    let connection = Connection::connect(credential, recorder)?;
    let git = connection.git(project.clone())?;
    let files = connection.files(project.clone())?;
    git.watch()?;
    files.write_text("notes.txt".into(), "one\n".into())?;
    wait_for(&events, |event| {
        *event
            == ConnectionEvent::GitChanged {
                project_id: project.clone(),
            }
    })?;
    let changes = git.changes()?;
    assert_eq!(
        (
            changes[0].path.as_str(),
            changes[0].staged,
            changes[0].unstaged
        ),
        ("notes.txt", None, Some(GitChangeKind::Untracked))
    );
    git.stage(vec!["notes.txt".into()])?;
    assert_eq!(git.changes()?[0].staged, Some(GitChangeKind::Added));
    let hash = git.commit("Add notes".into(), false)?;
    files.write_text("notes.txt".into(), "one\ntwo\n".into())?;
    let diff = git.diff("notes.txt".into(), false, None)?;
    assert_eq!((diff.additions, diff.deletions, diff.binary), (1, 0, false));
    assert!(diff.rows.iter().any(|row| {
        row.kind == GitDiffKind::Addition && row.new_text.as_deref() == Some("two")
    }));
    files.write_bytes("logo.png".into(), vec![0x89, b'P', 0, 0xff])?;
    git.stage(Vec::new())?;
    let status = git.status(false)?;
    let logo = status
        .files
        .iter()
        .find(|file| file.file.path == "logo.png")
        .ok_or("the image is missing from the status")?;
    assert_eq!(logo.file.staged, Some(GitChangeKind::Added));
    assert!(logo.staged_lines.binary);
    assert!(git.diff("logo.png".into(), true, None)?.binary);
    let commits = git.log(10, 0)?;
    assert_eq!(
        (commits[0].hash.as_str(), commits[0].subject.as_str()),
        (hash.as_str(), "Add notes")
    );
    git.create_branch("feature".into())?;
    let summary = git.summary()?.ok_or("expected a repository")?;
    assert_eq!(summary.branch.as_deref(), Some("feature"));
    assert_eq!((summary.staged, summary.unstaged), (2, 0));
    assert!(matches!(
        git.checkout_commit("not a hash".into()),
        Err(MobileError::Server { .. })
    ));
    Ok(())
}

#[test]
fn a_phone_adds_and_removes_worktrees_where_the_desktop_would() -> TestResult {
    let (_server, local, credential) = paired()?;
    let (folder, project) = project_folder(&local)?;
    init_repository(&folder.0)?;
    run_git(&folder.0, &["commit", "--allow-empty", "-m", "initial"])?;
    // Worktrees go next to the project's folder, named `<project>-<branch>`.
    let beside = |branch: &str| Folder(folder.0.with_file_name(format!("Phone-{branch}")));
    let (first, second, taken) = (
        format!("first-{}", OperationId::new()),
        format!("second-{}", OperationId::new()),
        format!("taken-{}", OperationId::new()),
    );
    let (first_folder, second_folder, taken_folder) =
        (beside(&first), beside(&second), beside(&taken));
    let connection = Connection::connect(credential, listener().0)?;
    let worktree = connection.git(project.clone())?.create_worktree(
        first.clone(),
        Some("HEAD".into()),
        None,
    )?;
    assert_eq!(
        (worktree.name.as_str(), worktree.is_worktree),
        (first.as_str(), true)
    );
    assert_eq!(worktree.parent_id.as_deref(), Some(project.as_str()));
    assert_eq!(
        Path::new(&worktree.directory),
        first_folder.0.canonicalize()?
    );
    // From a worktree project, worktrees are listed and added in its parent.
    let git = connection.git(worktree.id.clone())?;
    let sibling = git.create_worktree(second.clone(), Some("HEAD".into()), None)?;
    assert_eq!(sibling.parent_id.as_deref(), Some(project.as_str()));
    assert_eq!(
        Path::new(&sibling.directory),
        second_folder.0.canonicalize()?
    );
    let registered: Vec<_> = git
        .worktrees()?
        .into_iter()
        .filter_map(|entry| entry.registered)
        .collect();
    assert!(registered.contains(&worktree.id) && registered.contains(&sibling.id));
    std::fs::create_dir(&taken_folder.0)?;
    assert!(matches!(
        git.create_worktree(taken, Some("HEAD".into()), None),
        Err(MobileError::Server { reason }) if reason == "Worktree directory already exists"
    ));
    assert!(taken_folder.0.exists());
    for removed in [worktree, sibling] {
        let git = connection.git(removed.id.clone())?;
        let removal = git.inspect_worktree_removal()?;
        assert!(!removal.dirty);
        git.remove_worktree(removal)?;
        assert!(!Path::new(&removed.directory).exists());
    }
    assert!(
        connection
            .projects()?
            .iter()
            .all(|listed| listed.parent_id.is_none())
    );
    Ok(())
}

#[test]
fn a_phone_browses_and_edits_project_files() -> TestResult {
    let (_server, local, credential) = paired()?;
    let (_folder, project) = project_folder(&local)?;
    let (recorder, events) = listener();
    let connection = Connection::connect(credential, recorder)?;
    let files = connection.files(project.clone())?;
    files.watch()?;
    assert_eq!(files.create_directory("docs".into())?, "docs");
    assert_eq!(files.create_directory("docs".into())?, "docs 2");
    assert_eq!(
        files.write_text("docs/a.md".into(), "hello".into())?,
        "docs/a.md"
    );
    wait_for(&events, |event| {
        matches!(event, ConnectionEvent::FilesChanged { project_id, paths }
            if *project_id == project
                && (paths.is_empty() || paths.iter().any(|path| path.starts_with("docs"))))
    })?;
    let entries = files.list("docs".into())?;
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["docs/a.md"]
    );
    assert_eq!(files.read_text("docs/a.md".into())?, "hello");
    let info = files.stat("docs/a.md".into())?;
    assert!(!info.is_directory && info.size == 5);
    let image = vec![0x89, b'P', b'N', b'G', 0, 0xff];
    assert_eq!(
        files.write_bytes("docs/logo.png".into(), image.clone())?,
        "docs/logo.png"
    );
    assert_eq!(files.read_bytes("docs/logo.png".into())?, image);
    assert!(matches!(
        files.read_text("docs/logo.png".into()),
        Err(MobileError::Server { .. })
    ));
    assert_eq!(
        files.rename("docs/a.md".into(), "b.md".into())?,
        "docs/b.md"
    );
    assert_eq!(
        files.move_files(vec!["docs/b.md".into()], String::new())?,
        ["b.md"]
    );
    // An empty delete moves nothing to the computer's Trash.
    files.delete_files(Vec::new())?;
    assert!(matches!(
        files.read_bytes("../outside".into()),
        Err(MobileError::Server { .. })
    ));
    files.unwatch()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The facade the React Native module drives

struct Collector(Mutex<Sender<Value>>);

impl DirectListener for Collector {
    fn on_message(&self, json: String) {
        if let Ok(value) = serde_json::from_str(&json) {
            let _ = self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(value);
        }
    }
}

/// A paired phone's credential as the module stores it, and a connected facade.
fn direct_phone() -> TestResult<(Server, Client, Arc<Direct>, Receiver<Value>)> {
    let (server, local, direct, events, _) = connect_phone(Server::new(), None, None)?;
    Ok((server, local, direct, events))
}

/// Pairs a phone with `server` (over `port` when given) and returns the credential as the
/// module stores it, asking for `transport` when given.
fn pair_phone(
    server: Server,
    transport: Option<&str>,
    port: Option<u16>,
) -> TestResult<(Server, Client, Value)> {
    let local = server.local()?;
    match port {
        Some(port) => listen_at(&local, port)?,
        None => {
            listen(&local)?;
        }
    }
    let mut offer = local.start_pairing()?;
    offer.invite.hosts = vec!["127.0.0.1".into()];
    let link = offer.invite.to_link();
    let target = ok(&direct_parse_link(link.clone()))?;
    assert_eq!(target["hosts"][0], "127.0.0.1");
    let mut credential = ok(&direct_pair(link, "Test phone".into()))?;
    assert_eq!(credential["fingerprint"].as_str().map(str::len), Some(64));
    if let Some(transport) = transport {
        credential["transport"] = json!(transport);
    }
    Ok((server, local, credential))
}

type Phone = (Server, Client, Arc<Direct>, Receiver<Value>, Value);

/// Pairs a phone with `server` (over `port` when given) and connects its facade, asking for
/// `transport` when given. Also returns the connect answer.
fn connect_phone(server: Server, transport: Option<&str>, port: Option<u16>) -> TestResult<Phone> {
    let (server, local, credential) = pair_phone(server, transport, port)?;
    let (sender, events) = mpsc::channel();
    let direct = Direct::new(Arc::new(Collector(Mutex::new(sender))));
    let info = ok(&direct.connect(credential.to_string()))?;
    assert!(info["serverId"].as_str().is_some_and(|id| !id.is_empty()));
    Ok((server, local, direct, events, info))
}

fn ok(answer: &str) -> TestResult<Value> {
    let value: Value = serde_json::from_str(answer)?;
    match value.get("ok") {
        Some(ok) => Ok(ok.clone()),
        None => Err(format!("error answer: {answer}").into()),
    }
}

fn call(direct: &Direct, method: &str, args: &Value) -> TestResult<Value> {
    ok(&direct.call(method.into(), args.to_string()))
}

fn fire(direct: &Direct, method: &str, args: &Value) -> TestResult<()> {
    ok(&direct.fire(method.into(), args.to_string())).map(drop)
}

fn wait_json(events: &Receiver<Value>, mut done: impl FnMut(&Value) -> bool) -> TestResult<Value> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let event = events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        if done(&event) {
            return Ok(event);
        }
    }
}

/// Keeps the rows the app would draw from `snapshot` and `diff` events.
#[derive(Default)]
struct Rows(Vec<String>);

impl Rows {
    fn apply(&mut self, event: &Value) {
        match event["t"].as_str() {
            Some("snapshot") => {
                self.0 = event["ansi"]
                    .as_str()
                    .unwrap_or_default()
                    .split("\r\n")
                    .map(plain)
                    .collect();
            }
            Some("diff") => {
                for row in event["rows"].as_array().into_iter().flatten() {
                    let (Some(index), Some(text)) = (row[0].as_u64(), row[1].as_str()) else {
                        continue;
                    };
                    let index = usize::try_from(index).unwrap_or(usize::MAX);
                    if self.0.len() <= index {
                        self.0.resize(index + 1, String::new());
                    }
                    self.0[index] = plain(text);
                }
            }
            _ => {}
        }
    }

    fn has(&self, needle: &str) -> bool {
        self.0.iter().any(|row| row.contains(needle))
    }

    /// How many times `marker` is on the screen.
    fn count(&self, marker: char) -> usize {
        self.0.iter().map(|row| row.matches(marker).count()).sum()
    }
}

/// Text without escape sequences.
fn plain(ansi: &str) -> String {
    let mut out = String::new();
    let mut chars = ansi.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn home_session(direct: &Direct, cols: u16, rows: u16) -> TestResult<String> {
    let projects = call(direct, "projects", &Value::Null)?;
    let home = projects
        .as_array()
        .and_then(|all| all.iter().find(|project| project["isHome"] == true))
        .ok_or("no Home project")?;
    let created = call(
        direct,
        "createSession",
        &json!({ "projectId": home["id"], "cols": cols, "rows": rows }),
    )?;
    Ok(created["id"].as_str().ok_or("no session id")?.to_owned())
}

#[test]
fn the_facade_pairs_attaches_types_and_streams_rows() -> TestResult {
    let (_server, _local, direct, events) = direct_phone()?;
    let session = home_session(&direct, 40, 6)?;
    let attached = call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 40, "rows": 6 }),
    )?;
    assert_eq!(attached["cols"], 40);

    let mut rows = Rows::default();
    let hello = wait_json(&events, |event| event["t"] == "hello")?;
    assert_eq!(
        (hello["cols"].clone(), hello["rows"].clone()),
        (json!(40), json!(6))
    );
    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "printf '\\ndirect-%d\\n' $((40+2))\r" }),
    )?;
    wait_json(&events, |event| {
        rows.apply(event);
        rows.has("direct-42")
    })
    .map_err(|error| format!("{error}; rows: {:?}", rows.0))?;

    let listed = call(&direct, "sessions", &Value::Null)?;
    assert!(listed.as_array().is_some_and(|all| {
        all.iter()
            .any(|s| s["id"] == session.as_str() && s["attached"] == true)
    }));
    let history = call(
        &direct,
        "history",
        &json!({ "session": session, "lines": 100 }),
    )?;
    assert_eq!(history["visible"], 6);
    assert!(history["rows"].as_array().is_some_and(|all| all.len() >= 6));
    let ping = call(&direct, "ping", &Value::Null)?;
    assert!(ping.as_f64().is_some_and(|ms| ms >= 0.0));
    let stats = call(&direct, "echoStats", &json!({ "session": session }))?;
    assert_eq!(stats["mode"], "auto");

    // Arrow keys reach the program as the keys they are: Ctrl-C ends the running command.
    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "sleep 30\r" }),
    )?;
    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "\u{3}" }),
    )?;
    fire(
        &direct,
        "paste",
        &json!({ "session": session, "text": "echo pasted-ok", "submit": true }),
    )?;
    wait_json(&events, |event| {
        rows.apply(event);
        rows.has("pasted-ok")
    })
    .map_err(|error| format!("{error}; rows: {:?}", rows.0))?;

    call(&direct, "detach", &json!({ "session": session }))?;
    call(&direct, "endSession", &json!({ "session": session }))?;
    direct.disconnect();
    wait_json(&events, |event| event["t"] == "disconnected")?;
    let after = direct.call("projects".into(), "{}".into());
    assert!(after.contains("Disconnected"), "{after}");
    Ok(())
}

#[test]
fn the_facade_moves_to_udp_and_works_the_same() -> TestResult {
    let (_server, _local, direct, events, info) = connect_phone(Server::new(), None, None)?;
    assert_eq!(info["transport"], "udp", "{info}");
    assert_eq!(info["udpNote"], "");
    let session = home_session(&direct, 40, 6)?;
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 40, "rows": 6 }),
    )?;
    let mut rows = Rows::default();
    wait_json(&events, |event| event["t"] == "hello")?;
    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "printf '\\nudp-%d\\n' $((40+2))\r" }),
    )?;
    wait_json(&events, |event| {
        rows.apply(event);
        rows.has("udp-42")
    })
    .map_err(|error| format!("{error}; rows: {:?}", rows.0))?;
    let stats = call(&direct, "echoStats", &json!({ "session": session }))?;
    assert_eq!(stats["transport"], "udp");
    assert_eq!(stats["mode"], "auto");
    assert!(call(&direct, "ping", &Value::Null)?.is_f64());
    let history = call(
        &direct,
        "history",
        &json!({ "session": session, "lines": 100 }),
    )?;
    assert_eq!(history["visible"], 6);
    call(&direct, "detach", &json!({ "session": session }))?;
    call(&direct, "endSession", &json!({ "session": session }))?;
    direct.disconnect();
    wait_json(&events, |event| event["t"] == "disconnected")?;
    Ok(())
}

#[test]
fn the_facade_stays_on_tls_when_asked_or_when_udp_is_off() -> TestResult {
    let (_server, _local, direct, _events, info) = connect_phone(Server::new(), Some("tls"), None)?;
    assert_eq!(info["transport"], "tls", "{info}");
    assert!(
        info["udpNote"]
            .as_str()
            .is_some_and(|note| !note.is_empty()),
        "{info}"
    );
    let session = home_session(&direct, 40, 6)?;
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 40, "rows": 6 }),
    )?;
    let stats = call(&direct, "echoStats", &json!({ "session": session }))?;
    assert_eq!(stats["transport"], "tls");
    direct.disconnect();

    let (_server, _local, direct, _events, info) =
        connect_phone(Server::without_udp(), None, None)?;
    assert_eq!(info["transport"], "tls", "{info}");
    assert!(
        info["udpNote"]
            .as_str()
            .is_some_and(|note| !note.is_empty()),
        "{info}"
    );
    direct.disconnect();
    Ok(())
}

#[test]
fn sessions_carry_the_terminals_title_and_attaching_still_counts() -> TestResult {
    let (_server, _local, direct, events, _) = connect_phone(Server::new(), Some("tls"), None)?;
    let session = home_session(&direct, 40, 6)?;
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 40, "rows": 6 }),
    )?;
    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "printf '\\033]0;phone-title\\007'\r" }),
    )?;
    wait_json(&events, |event| {
        event["t"] == "title" && event["title"] == "phone-title"
    })?;
    let entry = |listed: &Value| -> Option<Value> {
        listed
            .as_array()?
            .iter()
            .find(|entry| entry["id"] == session.as_str())
            .cloned()
    };
    let attached = entry(&call(&direct, "sessions", &Value::Null)?).ok_or("session not listed")?;
    assert_eq!(attached["title"], "phone-title");
    assert_eq!(attached["attached"], true);

    // Detached, the title arrives as a pushed session event instead of from the terminal.
    call(&direct, "detach", &json!({ "session": session }))?;
    let detached = entry(&call(&direct, "sessions", &Value::Null)?).ok_or("session not listed")?;
    assert_eq!(detached["title"], "phone-title");
    assert_eq!(detached["attached"], false);

    // Listing asked the server about this session and let go again; attaching must still count.
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 40, "rows": 6 }),
    )?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let again = entry(&call(&direct, "sessions", &Value::Null)?).ok_or("session not listed")?;
        if again["attached"] == true {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the attached session never counted: {again}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    call(&direct, "endSession", &json!({ "session": session }))?;
    direct.disconnect();
    Ok(())
}

#[test]
fn the_connect_answer_names_the_route_and_the_addresses_to_keep() -> TestResult {
    let server = Server::new();
    let local = server.local()?;
    listen(&local)?;
    let mut offer = local.start_pairing()?;
    offer.invite.hosts = vec!["127.0.0.1".into()];
    let mut credential = ok(&direct_pair(offer.invite.to_link(), "Test phone".into()))?;
    // The first stored address refuses at once: nothing listens on 127.0.0.3.
    credential["hosts"] = json!(["127.0.0.3", "127.0.0.1"]);
    credential["transport"] = json!("tls");
    let (sender, _events) = mpsc::channel();
    let direct = Direct::new(Arc::new(Collector(Mutex::new(sender))));
    let started = Instant::now();
    let info = ok(&direct.connect(credential.to_string()))?;
    // The refused address did not cost a timeout, and the second one answered.
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(info["route"]["host"], "127.0.0.1", "{info}");
    assert_eq!(info["route"]["kind"], "local", "{info}");
    assert!(
        info["route"]["ms"]
            .as_f64()
            .is_some_and(|ms| ms > 0.0 && ms < 3000.0),
        "{info}"
    );
    // What the server reported first, then the stored address it did not list.
    assert_eq!(
        info["addresses"],
        json!([SERVER_ADDRESS, "127.0.0.1", "127.0.0.3"]),
        "{info}"
    );
    let session = home_session(&direct, 40, 6)?;
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 40, "rows": 6 }),
    )?;
    let stats = call(&direct, "echoStats", &json!({ "session": session }))?;
    assert_eq!(stats["route"]["host"], "127.0.0.1", "{stats}");
    assert_eq!(stats["route"]["kind"], "local", "{stats}");
    direct.disconnect();
    Ok(())
}

#[test]
fn the_facade_reports_errors_as_codes() -> TestResult {
    let (sender, _events) = mpsc::channel();
    let direct = Direct::new(Arc::new(Collector(Mutex::new(sender))));
    let answer: Value = serde_json::from_str(&direct.call("projects".into(), "{}".into()))?;
    assert_eq!(answer["error"]["code"], "Disconnected");
    let answer: Value = serde_json::from_str(&direct_parse_link("not a link".into()))?;
    assert_eq!(answer["error"]["code"], "InvalidLink");
    let answer: Value = serde_json::from_str(&direct.connect("{}".into()))?;
    assert_eq!(answer["error"]["code"], "InvalidCredential");
    Ok(())
}

/// Reports attach time and input-to-screen latency through the facade over loopback TLS:
/// `cargo test -p atur-mobile --test sdk bench -- --ignored --nocapture`.
#[test]
#[ignore = "prints measurements"]
#[allow(
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn bench_facade_attach_and_echo() -> TestResult {
    // `ATUR_BENCH_TRANSPORT=tls` keeps the phone on TLS; otherwise it moves to UDP, as the app does.
    let tls_only = std::env::var("ATUR_BENCH_TRANSPORT").is_ok_and(|transport| transport == "tls");
    let (_server, _local, direct, events, info) =
        connect_phone(Server::new(), tls_only.then_some("tls"), None)?;
    eprintln!("bench transport={}", info["transport"]);
    let session = home_session(&direct, 100, 10)?;
    let started = Instant::now();
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 100, "rows": 10 }),
    )?;
    let mut rows = Rows::default();
    wait_json(&events, |event| {
        rows.apply(event);
        event["t"] == "snapshot"
    })?;
    eprintln!(
        "bench attach_ms={:.2}",
        started.elapsed().as_secs_f64() * 1000.0
    );

    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "cat >/dev/null\r" }),
    )?;
    thread::sleep(Duration::from_millis(1200));
    for mode in ["never", "always"] {
        call(&direct, "echoMode", &json!({ "mode": mode }))?;
        let mut samples = Vec::new();
        let mut typed = String::new();
        // A different letter per round, so the earlier round's line does not match.
        let letter = if mode == "never" { 'q' } else { 'w' };
        for _ in 0..40 {
            typed.push(letter);
            let sent = Instant::now();
            fire(
                &direct,
                "input",
                &json!({ "session": session, "data": letter.to_string() }),
            )?;
            wait_json(&events, |event| {
                rows.apply(event);
                rows.has(&typed)
            })?;
            samples.push(sent.elapsed().as_secs_f64() * 1000.0);
            thread::sleep(Duration::from_millis(100));
        }
        // Start the next round on a fresh line.
        fire(
            &direct,
            "input",
            &json!({ "session": session, "data": "\u{4}" }),
        )?;
        fire(
            &direct,
            "input",
            &json!({ "session": session, "data": "cat >/dev/null\r" }),
        )?;
        thread::sleep(Duration::from_millis(1200));
        rows = Rows::default();
        call(
            &direct,
            "attach",
            &json!({ "session": session, "cols": 100, "rows": 10 }),
        )?;
        wait_json(&events, |event| {
            rows.apply(event);
            event["t"] == "snapshot"
        })?;
        let stats = call(&direct, "echoStats", &json!({ "session": session }))?;
        eprintln!(
            "bench stats mode={mode} {stats} first={:?}",
            &samples[..5.min(samples.len())]
        );
        samples.sort_by(f64::total_cmp);
        let at = |fraction: f64| {
            samples[((samples.len() as f64 * fraction) as usize).min(samples.len() - 1)]
        };
        eprintln!(
            "bench echo mode={mode} n={} p50_ms={:.2} p90_ms={:.2} max_ms={:.2}",
            samples.len(),
            at(0.5),
            at(0.9),
            samples[samples.len() - 1]
        );
    }
    let ping = call(&direct, "ping", &Value::Null)?;
    eprintln!("bench ping_ms={:.2}", ping.as_f64().unwrap_or_default());
    direct.disconnect();
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The same measurement against a server that is already running (the executable, not the
// in-process one): `ATUR_BENCH_LINK_FILE` names a file holding the pairing link it printed.

fn millis(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// Count, median, 90th percentile and maximum of `samples`, in milliseconds.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn describe(samples: &[f64]) -> String {
    if samples.is_empty() {
        return "n=0".into();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at =
        |fraction: f64| sorted[((sorted.len() as f64 * fraction) as usize).min(sorted.len() - 1)];
    format!(
        "n={} p50_ms={:.1} p90_ms={:.1} max_ms={:.1}",
        sorted.len(),
        at(0.5),
        at(0.9),
        sorted[sorted.len() - 1]
    )
}

/// Keys typed per variant: `ATUR_BENCH_KEYS`, 30 by default.
fn bench_keys() -> usize {
    std::env::var("ATUR_BENCH_KEYS")
        .ok()
        .and_then(|keys| keys.parse().ok())
        .unwrap_or(30)
}

fn unhex(text: &str) -> TestResult<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|at| -> TestResult<u8> { Ok(u8::from_str_radix(&text[at..at + 2], 16)?) })
        .collect()
}

/// The stored credential the facade uses, as the core's own record.
fn core_credential(value: &Value) -> TestResult<ServerCredential> {
    let text = |name: &str| {
        value[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("the credential has no {name}"))
    };
    Ok(ServerCredential {
        server_id: text("serverId")?,
        server_name: text("serverName").unwrap_or_default(),
        hosts: value["hosts"]
            .as_array()
            .ok_or("the credential has no hosts")?
            .iter()
            .filter_map(|host| host.as_str().map(str::to_owned))
            .collect(),
        port: value["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .ok_or("the credential has no port")?,
        fingerprint: unhex(&text("fingerprint")?)?,
        device_id: text("deviceId")?,
        token: unhex(&text("token")?)?,
    })
}

/// Connects with the core's own API (no facade, no address learning, no UDP offer, no title
/// references), types one key at a time and times the screen change.
#[allow(clippy::print_stderr)]
fn bench_core(label: &str, credential: &Value) -> TestResult {
    let (recorder, events) = listener();
    let started = Instant::now();
    let connection = Connection::connect(core_credential(credential)?, recorder)?;
    let connect_ms = millis(started);
    let projects = connection.projects()?;
    let home = projects
        .iter()
        .find(|project| project.is_home)
        .ok_or("no Home project")?;
    let session = connection.create_session(home.id.clone(), 80, 24)?;
    let started = Instant::now();
    let terminal = connection.attach(session.id, 80, 24)?;
    let attach_ms = millis(started);
    terminal.send_input(b"cat >/dev/null\r".to_vec())?;
    thread::sleep(Duration::from_millis(1200));
    let text = || -> Vec<String> {
        terminal
            .screen()
            .lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect()
    };
    while events.try_recv().is_ok() {}
    let mut samples = Vec::new();
    let mut typed = String::new();
    for _ in 0..bench_keys() {
        typed.push('q');
        let sent = Instant::now();
        terminal.send_input(b"q".to_vec())?;
        wait_for(&events, |event| {
            matches!(event, ConnectionEvent::ScreenChanged { .. })
                && text().iter().any(|line| line.contains(&typed))
        })?;
        samples.push(millis(sent));
        thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "bench {label} core connect_ms={connect_ms:.1} attach_ms={attach_ms:.1} echo {}",
        describe(&samples)
    );
    eprintln!(
        "bench {label} core samples={:?}",
        samples
            .iter()
            .map(|ms| (ms * 10.0).round() / 10.0)
            .collect::<Vec<_>>()
    );
    connection.end_session(session.id)?;
    connection.disconnect();
    Ok(())
}

/// One connection through the facade, made as `variant` says: `tls` or `auto` (UDP when the server
/// offers it), then `+sessions` to list the sessions before attaching (which makes the server
/// reference them) and `+history` to read scrollback after attaching, as the app's pane does.
#[allow(clippy::print_stderr)]
fn bench_external_facade(label: &str, variant: &str, credential: &Value) -> TestResult {
    let mut parts = variant.split('+');
    let transport = parts.next().unwrap_or("tls");
    let flags: Vec<&str> = parts.collect();
    let mut credential = credential.clone();
    credential["transport"] = json!(if transport == "tls" { "tls" } else { "auto" });
    let (sender, events) = mpsc::channel();
    let direct = Direct::new(Arc::new(Collector(Mutex::new(sender))));
    let started = Instant::now();
    let info = ok(&direct.connect(credential.to_string()))?;
    let connect_ms = millis(started);
    let session = home_session(&direct, 80, 24)?;
    let mut extra = String::new();
    if flags.contains(&"sessions") {
        let started = Instant::now();
        call(&direct, "sessions", &Value::Null)?;
        extra = format!("{extra} sessions_ms={:.1}", millis(started));
    }
    let started = Instant::now();
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 80, "rows": 24 }),
    )?;
    let mut rows = Rows::default();
    wait_json(&events, |event| {
        rows.apply(event);
        event["t"] == "snapshot"
    })?;
    let attach_ms = millis(started);
    if flags.contains(&"history") {
        let started = Instant::now();
        call(
            &direct,
            "history",
            &json!({ "session": session, "lines": 1000 }),
        )?;
        extra = format!("{extra} history_ms={:.1}", millis(started));
    }
    call(&direct, "echoMode", &json!({ "mode": "never" }))?;
    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "cat >/dev/null\r" }),
    )?;
    thread::sleep(Duration::from_millis(1200));
    while let Ok(event) = events.try_recv() {
        rows.apply(&event);
    }
    let mut samples = Vec::new();
    let mut typed = String::new();
    for _ in 0..bench_keys() {
        typed.push('q');
        let sent = Instant::now();
        fire(
            &direct,
            "input",
            &json!({ "session": session, "data": "q" }),
        )?;
        wait_json(&events, |event| {
            rows.apply(event);
            rows.has(&typed)
        })?;
        samples.push(millis(sent));
        thread::sleep(Duration::from_millis(100));
    }
    let mut pings = Vec::new();
    for _ in 0..10 {
        pings.push(
            call(&direct, "ping", &Value::Null)?
                .as_f64()
                .unwrap_or_default(),
        );
    }
    let stats = call(&direct, "echoStats", &json!({ "session": session }))?;
    eprintln!(
        "bench {label} {variant} transport={} connect_ms={connect_ms:.1} attach_ms={attach_ms:.1}{extra} echo {} ping {} rtt_ms={}",
        stats["transport"],
        describe(&samples),
        describe(&pings),
        stats["rttMs"]
    );
    eprintln!(
        "bench {label} {variant} samples={:?} note={:?}",
        samples
            .iter()
            .map(|ms| (ms * 10.0).round() / 10.0)
            .collect::<Vec<_>>(),
        info["udpNote"]
    );
    call(&direct, "endSession", &json!({ "session": session }))?;
    direct.disconnect();
    Ok(())
}

/// Echo, ping and attach time against the running server `ATUR_BENCH_LINK_FILE` names, once per
/// variant in `ATUR_BENCH_VARIANTS` (`core`, then facade variants such as `tls`, `auto`,
/// `tls+sessions`); `ATUR_BENCH_LABEL` tags the output.
/// `cargo test -p atur-mobile --test sdk bench_facade_external -- --ignored --nocapture`.
#[test]
#[ignore = "prints measurements; needs a running server and ATUR_BENCH_LINK_FILE"]
#[allow(clippy::print_stderr)]
fn bench_facade_external() -> TestResult {
    let label = std::env::var("ATUR_BENCH_LABEL").unwrap_or_else(|_| "external".into());
    let link = std::fs::read_to_string(
        std::env::var_os("ATUR_BENCH_LINK_FILE").ok_or("ATUR_BENCH_LINK_FILE is not set")?,
    )?;
    let credential = ok(&direct_pair(link.trim().to_owned(), "Echo bench".into()))?;
    let variants = std::env::var("ATUR_BENCH_VARIANTS")
        .unwrap_or_else(|_| "core,tls,tls+sessions,auto,auto+sessions".into());
    for variant in variants.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        let outcome = if variant == "core" {
            bench_core(&label, &credential)
        } else {
            bench_external_facade(&label, variant, &credential)
        };
        if let Err(error) = outcome {
            eprintln!("bench {label} {variant} failed: {error}");
        }
    }
    Ok(())
}

/// Runs `ATUR_BENCH_NETEM <step>` when the variable names the shaping script
/// (`scripts/lossy-netem.sh`); returns whether it ran.
fn netem(step: &str) -> TestResult<bool> {
    let Some(script) = std::env::var_os("ATUR_BENCH_NETEM") else {
        return Ok(false);
    };
    let status = Command::new(script).arg(step).status()?;
    if status.success() {
        Ok(true)
    } else {
        Err(format!("the shaping script failed at `{step}`: {status}").into())
    }
}

/// Waits for an event like `wait_json`, for as long as a badly shaped link needs.
fn wait_json_within(
    events: &Receiver<Value>,
    limit: Duration,
    mut done: impl FnMut(&Value) -> bool,
) -> TestResult<Value> {
    let deadline = Instant::now() + limit;
    loop {
        let event = events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        if done(&event) {
            return Ok(event);
        }
    }
}

/// The character the outage probes of connection number `generation` type. Each connection
/// has its own, so a probe typed on an earlier one cannot be mistaken for it.
fn probe_marker(generation: usize) -> char {
    char::from(b'z' - u8::try_from(generation % 8).unwrap_or(0))
}

/// Connects again with the stored credential, attaches `session` and waits for its screen,
/// retrying until `deadline`. Returns the connect answer.
#[allow(clippy::print_stderr)]
fn reconnect(
    direct: &Direct,
    events: &Receiver<Value>,
    credential: &Value,
    session: &str,
    rows: &mut Rows,
    deadline: Instant,
) -> TestResult<Value> {
    loop {
        if Instant::now() >= deadline {
            return Err("the phone did not reconnect in time".into());
        }
        // Whatever the old connection left behind is not about this one.
        while events.try_recv().is_ok() {}
        let attempt = ok(&direct.connect(credential.to_string())).and_then(|info| {
            call(
                direct,
                "attach",
                &json!({ "session": session, "cols": 100, "rows": 10 }),
            )?;
            *rows = Rows::default();
            wait_json_within(events, Duration::from_secs(30), |event| {
                rows.apply(event);
                event["t"] == "snapshot"
            })?;
            call(direct, "echoMode", &json!({ "mode": "never" }))?;
            Ok(info)
        });
        match attempt {
            Ok(info) => return Ok(info),
            Err(error) => {
                eprintln!("reconnect attempt failed: {error}");
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// How an outage ended for the phone.
struct Recovery {
    /// From the end of the outage to the first probe back on the screen.
    elapsed: Duration,
    /// Whether the connection died and a new one carried the probe.
    reconnected: bool,
}

/// Lives through an outage that has just begun and lasts 10 s: types a marker every 500 ms and
/// drains the events, never waiting on a request that could time out. When the link comes back
/// (`outage-off`) the first marker typed since the outage began to appear on the screen ends the
/// wait; if the connection died on the way (a `disconnected` event), a new one is made after the
/// outage and the first marker its own probes show ends it. Fails only after two minutes.
#[allow(clippy::print_stderr)]
fn recover_from_outage(
    transport: &str,
    direct: &Direct,
    events: &Receiver<Value>,
    credential: &Value,
    session: &str,
    rows: &mut Rows,
) -> TestResult<Recovery> {
    const OUTAGE: Duration = Duration::from_secs(10);
    const PROBE_EVERY: Duration = Duration::from_millis(500);
    const LIMIT: Duration = Duration::from_secs(120);
    let began = Instant::now();
    let outage_ends = began + OUTAGE;
    let mut restored: Option<Instant> = None;
    let mut generation = 0;
    let mut seen = rows.count(probe_marker(generation));
    let mut dead = false;
    let mut reconnected = false;
    let mut next_probe = began;
    loop {
        if restored.is_none() && Instant::now() >= outage_ends {
            netem("outage-off")?;
            restored = Some(Instant::now());
        }
        if let Some(at) = restored {
            if at.elapsed() > LIMIT {
                return Err(
                    format!("no probe came back within {LIMIT:?}; rows: {:?}", rows.0).into(),
                );
            }
            if dead {
                let info = reconnect(direct, events, credential, session, rows, at + LIMIT)?;
                eprintln!(
                    "lossy {transport} reconnected transport={} note={:?}",
                    info["transport"], info["udpNote"]
                );
                generation += 1;
                seen = rows.count(probe_marker(generation));
                dead = false;
                reconnected = true;
                next_probe = Instant::now();
            }
        }
        if !dead && Instant::now() >= next_probe {
            let data = probe_marker(generation).to_string();
            if fire(
                direct,
                "input",
                &json!({ "session": session, "data": data }),
            )
            .is_err()
            {
                dead = true;
            }
            next_probe = Instant::now() + PROBE_EVERY;
        }
        let mut wake = if dead { outage_ends } else { next_probe };
        if restored.is_none() {
            wake = wake.min(outage_ends);
        }
        match events.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(event) => {
                if event["t"] == "disconnected" {
                    eprintln!(
                        "lossy {transport} disconnected {} ms after the outage began",
                        began.elapsed().as_millis()
                    );
                    dead = true;
                    continue;
                }
                rows.apply(&event);
                if rows.count(probe_marker(generation)) > seen {
                    return Ok(Recovery {
                        elapsed: restored.map_or(Duration::ZERO, |at| at.elapsed()),
                        reconnected,
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error @ mpsc::RecvTimeoutError::Disconnected) => return Err(error.into()),
        }
    }
}

/// Echo latency and recovery from a 10 s outage through the facade, over the transport named by
/// `ATUR_BENCH_TRANSPORT` (`tls`, or `udp`, the default). Nothing here shapes the network: with
/// `ATUR_BENCH_NETEM` naming `scripts/lossy-netem.sh` it asks that script to (after connecting,
/// so pairing is not shaped), and `ATUR_BENCH_TLS_PORT` fixes the TLS port the script filters on.
/// The in-process server offers UDP on `ATUR_TERM_UDP_PORTS`, as the executable does.
/// `cargo test -p atur-mobile --test sdk bench_facade_lossy -- --ignored --nocapture`.
#[test]
#[ignore = "prints measurements; the lossy workflow job shapes the network"]
#[allow(
    clippy::too_many_lines,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn bench_facade_lossy() -> TestResult {
    const KEYS: usize = 30;
    const LIMIT: Duration = Duration::from_secs(120);
    let transport = std::env::var("ATUR_BENCH_TRANSPORT").unwrap_or_else(|_| "udp".into());
    let port = std::env::var("ATUR_BENCH_TLS_PORT")
        .ok()
        .map(|port| port.parse::<u16>())
        .transpose()?;
    let asked = if transport == "tls" { "tls" } else { "auto" };
    let (_server, _local, credential) = pair_phone(Server::new(), Some(asked), port)?;
    let (sender, events) = mpsc::channel();
    let direct = Direct::new(Arc::new(Collector(Mutex::new(sender))));
    let info = ok(&direct.connect(credential.to_string()))?;
    eprintln!(
        "lossy {transport} connected transport={} note={:?}",
        info["transport"], info["udpNote"]
    );
    if info["transport"] != transport.as_str() {
        return Err(format!(
            "wanted {transport}, got {}; UDP note: {}",
            info["transport"], info["udpNote"]
        )
        .into());
    }
    let session = home_session(&direct, 100, 10)?;
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 100, "rows": 10 }),
    )?;
    let mut rows = Rows::default();
    wait_json(&events, |event| {
        rows.apply(event);
        event["t"] == "snapshot"
    })?;
    call(&direct, "echoMode", &json!({ "mode": "never" }))?;
    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "cat >/dev/null\r" }),
    )?;
    thread::sleep(Duration::from_millis(1200));
    let shaped = netem("on")?;

    let mut samples = Vec::new();
    let mut typed = String::new();
    for _ in 0..KEYS {
        typed.push('q');
        let sent = Instant::now();
        fire(
            &direct,
            "input",
            &json!({ "session": session, "data": "q" }),
        )?;
        wait_json_within(&events, LIMIT, |event| {
            rows.apply(event);
            rows.has(&typed)
        })
        .map_err(|error| format!("{error}; rows: {:?}", rows.0))?;
        samples.push(sent.elapsed().as_secs_f64() * 1000.0);
        thread::sleep(Duration::from_millis(250));
    }
    samples.sort_by(f64::total_cmp);
    let at = |fraction: f64| samples[((samples.len() as f64 * fraction) as usize).min(KEYS - 1)];
    eprintln!(
        "lossy {transport} echo n={KEYS} p50_ms={:.0} p90_ms={:.0} max_ms={:.0}",
        at(0.5),
        at(0.9),
        samples[KEYS - 1]
    );

    if netem("outage-on")? {
        let outcome = recover_from_outage(
            &transport,
            &direct,
            &events,
            &credential,
            &session,
            &mut rows,
        );
        if outcome.is_err() {
            let _ = netem("outage-off");
        }
        let recovery = outcome?;
        eprintln!(
            "lossy {transport} recovery_ms={:.0} reconnected={}",
            recovery.elapsed.as_secs_f64() * 1000.0,
            recovery.reconnected
        );
    } else {
        eprintln!("lossy {transport} recovery_ms=skipped (no ATUR_BENCH_NETEM)");
    }
    if shaped {
        netem("off")?;
    }
    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "\u{3}" }),
    )?;
    direct.disconnect();
    Ok(())
}

#[test]
fn the_facade_pastes_images_into_the_home_project() -> TestResult {
    let (_server, _local, direct, _events) = direct_phone()?;
    let session = home_session(&direct, 40, 6)?;
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 40, "rows": 6 }),
    )?;
    let png: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0xFF, 0x00, 0x7F,
    ];
    let name = format!("paste-test-{}.png", std::process::id());
    let base64 = "iVBORw0KGgr/AH8=";
    let pasted = call(
        &direct,
        "pasteImage",
        &json!({ "session": session, "name": name, "base64": base64 }),
    )?;
    let path = PathBuf::from(pasted["path"].as_str().ok_or("no path")?);
    let written = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    assert!(path.ends_with(format!(".cache/atur/pastes/{name}")));
    assert_eq!(written?, png);

    for bad in ["", "a/b.png", "a\\b.png", "..", "x..png", "a\nb.png"] {
        let answer = direct.call(
            "pasteImage".into(),
            json!({ "session": session, "name": bad, "base64": base64 }).to_string(),
        );
        assert!(ok(&answer).is_err(), "accepted name {bad:?}");
    }
    let huge = "A".repeat(4 * (5 * 1024 * 1024 / 3 + 2));
    let answer = direct.call(
        "pasteImage".into(),
        json!({ "session": session, "name": "huge.png", "base64": huge }).to_string(),
    );
    assert!(answer.contains("larger than 5 MiB"), "{answer}");
    Ok(())
}
