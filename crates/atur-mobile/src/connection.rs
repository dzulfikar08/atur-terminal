use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use muxy_client::{Client, ClientError, ClientEvent, RemoteEndpoint, Transport, UdpSettings};
use muxy_protocol::{
    DeviceCredential, ErrorCode, OperationId, ProjectDescriptor, ProjectId, ProjectSession,
    SessionId, Size,
};

use crate::MobileError;
use crate::channel::BridgeChannel;
use crate::files::ProjectFiles;
use crate::git::GitRepository;
use crate::records::{Activity, Project, ServerCredential, Session, SessionStatus, text};
use crate::route::{self, RouteInfo};
use crate::terminal::Terminal;
use crate::terminals::{Delivery, Terminals};

/// Receives connection events on an SDK thread; hop to the main thread before touching UI.
#[uniffi::export(with_foreign)]
pub trait ConnectionListener: Send + Sync {
    fn on_event(&self, event: ConnectionEvent);
}

#[derive(Clone, Debug, Eq, PartialEq, uniffi::Enum)]
pub enum ConnectionEvent {
    /// The terminal's screen changed; read it again.
    ScreenChanged {
        session_id: u64,
    },
    /// The terminal's title, directory, history size, or mouse modes changed.
    MetadataChanged {
        session_id: u64,
    },
    SessionEnded {
        session_id: u64,
    },
    /// Projects changed; read them again.
    CatalogChanged,
    /// Sessions were created, ended, or attached; read them again.
    SessionsChanged,
    /// Agent activity changed; read it again.
    ActivityChanged,
    /// The repository that `GitRepository::watch` watches changed.
    GitChanged {
        project_id: String,
    },
    /// Files changed in a project that `ProjectFiles::watch` watches. `paths`
    /// are relative to the project's folder; empty means anything may have changed.
    FilesChanged {
        project_id: String,
        paths: Vec<String>,
    },
    /// The server is restarting for an update; connect again shortly.
    ServerRestarting,
    /// The connection closed; connect again, then attach again.
    Disconnected,
}

/// Which transport `Connection::connect_direct` may use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransportMode {
    /// TLS first, then UDP when the server offers it and answers.
    Auto,
    /// TLS only.
    Tls,
}

/// A connection made by `Connection::connect_direct`, and why UDP is not carrying it, if so.
pub(crate) struct Opened {
    pub(crate) connection: Arc<Connection>,
    /// Empty when UDP carries the connection.
    pub(crate) udp_note: String,
    /// The address that won the race, and how long it took to sign in.
    pub(crate) route: RouteInfo,
    /// The addresses to keep in the credential: what the server reported, then the stored ones.
    pub(crate) addresses: Vec<String>,
}

/// How long `sessions` waits for the server to push the terminals' titles.
const TITLE_WAIT: Duration = Duration::from_secs(2);

/// The terminal titles the server pushed, newest per session. The server pushes
/// them only for sessions this connection references.
#[derive(Default)]
struct Titles {
    state: Mutex<TitleState>,
    changed: Condvar,
}

#[derive(Default)]
struct TitleState {
    titles: HashMap<SessionId, String>,
    /// Sessions whose title arrived since `begin`.
    fresh: HashSet<SessionId>,
}

impl Titles {
    fn lock(&self) -> std::sync::MutexGuard<'_, TitleState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn begin(&self) {
        self.lock().fresh.clear();
    }

    fn record(&self, session: SessionId, title: String) {
        let mut state = self.lock();
        state.titles.insert(session, title);
        state.fresh.insert(session);
        drop(state);
        self.changed.notify_all();
    }

    /// Waits until every session in `wanted` has a title newer than `begin`, or `timeout`.
    fn wait(&self, wanted: &[SessionId], timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        while !wanted.iter().all(|session| state.fresh.contains(session)) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// A live connection to one server, paired or reached over SSH.
#[derive(uniffi::Object)]
pub struct Connection {
    client: Client,
    /// The address a paired connection was made through; none over a bridge.
    route: Option<RouteInfo>,
    terminals: Arc<Terminals>,
    titles: Arc<Titles>,
    /// Frames are held only for channels newer than every attached one, which
    /// is true only while attaches run one at a time.
    attaching: Mutex<()>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.client.disconnect();
    }
}

#[uniffi::export]
impl Connection {
    /// Connects with a saved pairing, trying each of the server's addresses.
    #[uniffi::constructor]
    pub fn connect(
        credential: ServerCredential,
        listener: Arc<dyn ConnectionListener>,
    ) -> Result<Arc<Self>, MobileError> {
        let (endpoint, device) = remote(credential)?;
        let (client, route) = Client::connect_remote_at(&endpoint, device)?;
        Self::start(client, Some(RouteInfo::new(&route)), listener)
    }

    /// Connects through an SSH exec channel that the app opened to run
    /// `bridge_command()`. `host` names the computer in errors. The server
    /// sees a local client, which older servers keep calling a CLI.
    #[uniffi::constructor]
    pub fn connect_channel(
        channel: Arc<BridgeChannel>,
        host: String,
        listener: Arc<dyn ConnectionListener>,
    ) -> Result<Arc<Self>, MobileError> {
        let client = channel.connect(&host)?;
        match client.identify(muxy_protocol::ClientKind::Mobile) {
            Ok(_) | Err(ClientError::Server(_)) => {}
            Err(error) => return Err(error.into()),
        }
        Self::start(client, None, listener)
    }

    pub fn server_version(&self) -> String {
        self.client.server_info().build.version.clone()
    }

    /// The server's stable id, the same as `serverId` from pairing with it.
    pub fn server_id(&self) -> Result<String, MobileError> {
        Ok(self.client.catalog_page(None, None)?.server.to_string())
    }

    pub fn projects(&self) -> Result<Vec<Project>, MobileError> {
        Ok(self
            .client
            .catalog()?
            .projects
            .iter()
            .map(Project::from)
            .collect())
    }

    /// Every session of a project, including ended ones whose content is kept.
    pub fn sessions(&self, project_id: String) -> Result<Vec<Session>, MobileError> {
        let project = project(&project_id)?;
        Ok(all_sessions(&self.client, project)?
            .iter()
            .map(Session::from)
            .collect())
    }

    /// Starts a shell in the project's directory.
    pub fn create_session(
        &self,
        project_id: String,
        columns: u16,
        rows: u16,
    ) -> Result<Session, MobileError> {
        let project = project(&project_id)?;
        let catalog = self.client.catalog()?;
        let descriptor = find_project(&catalog.projects, project)?;
        let info = self.client.create_project_session(
            project,
            OperationId::new(),
            Path::new(OsStr::from_bytes(&descriptor.directory.0)),
            Size {
                cols: columns,
                rows,
            },
        )?;
        Ok(Session {
            id: info.id.get(),
            project_id,
            directory: String::from_utf8_lossy(&info.directory.0).into_owned(),
            status: SessionStatus::Live,
            owner: None,
            attached: false,
        })
    }

    /// Ends the session for every client; its panes close everywhere.
    pub fn end_session(&self, session_id: u64) -> Result<(), MobileError> {
        Ok(self.client.end_session(session(session_id)?)?)
    }

    /// Attaches as a watcher: the session keeps the size it has, and takes this device's
    /// grid (the size given here, or the last `Terminal::resize`) only once the user types.
    pub fn attach(
        &self,
        session_id: u64,
        columns: u16,
        rows: u16,
    ) -> Result<Arc<Terminal>, MobileError> {
        let session = session(session_id)?;
        let _attaching = self
            .attaching
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let told = Size {
            cols: columns,
            rows,
        };
        let attachment = self.client.attach(session, told)?;
        let channel = attachment.channel;
        let (view, acknowledge) = self.terminals.install(session, attachment);
        self.track();
        if let Some(seq) = acknowledge {
            self.client.ack(channel, seq)?;
        }
        Ok(Arc::new(Terminal {
            client: self.client.clone(),
            view,
            terminals: Arc::clone(&self.terminals),
            told: Mutex::new(told),
        }))
    }

    pub fn activity(&self) -> Result<Activity, MobileError> {
        Ok(self.client.activity()?.into())
    }

    /// Marks events as seen on every client.
    pub fn acknowledge_activity(&self, event_ids: Vec<u64>) -> Result<(), MobileError> {
        Ok(self.client.acknowledge_activity(event_ids)?)
    }

    /// Git in the project's repository. Doesn't talk to the server.
    pub fn git(&self, project_id: String) -> Result<Arc<GitRepository>, MobileError> {
        let project = project(&project_id)?;
        Ok(Arc::new(GitRepository::new(self.client.clone(), project)))
    }

    /// The project's folder. Doesn't talk to the server.
    pub fn files(&self, project_id: String) -> Result<Arc<ProjectFiles>, MobileError> {
        let project = project(&project_id)?;
        Ok(Arc::new(ProjectFiles::new(self.client.clone(), project)))
    }

    pub fn disconnect(&self) {
        self.client.disconnect();
    }
}

impl Connection {
    /// The protocol client, for the facade the React Native module drives.
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    /// The address this connection was made through, when it is a paired one.
    pub(crate) fn route(&self) -> Option<&RouteInfo> {
        self.route.as_ref()
    }

    /// Which transport carries the connection: `udp`, or `tls` for anything else.
    pub(crate) fn transport_name(&self) -> &'static str {
        match self.client.transport() {
            Transport::Udp => "udp",
            Transport::Stream | Transport::UdpFailed(_) => "tls",
        }
    }

    /// Connects over TLS through the first of the server's addresses that signs in (they are
    /// raced, local ones first), learns the server's current addresses, and, with
    /// `TransportMode::Auto`, moves to UDP when the server offers it and answers. Any UDP failure
    /// keeps the TLS connection.
    pub(crate) fn connect_direct(
        credential: ServerCredential,
        mode: TransportMode,
        listener: Arc<dyn ConnectionListener>,
    ) -> Result<Opened, MobileError> {
        let (endpoint, device) = remote(credential)?;
        let (tls, won) = Client::connect_remote_at(&endpoint, device)?;
        let addresses = route::learn(&tls, &endpoint.hosts, &won.host);
        let route_info = RouteInfo::new(&won);
        let (client, udp_note) = match mode {
            TransportMode::Tls => (tls, "turned off".to_owned()),
            TransportMode::Auto => upgrade(tls, &won.host, device),
        };
        Ok(Opened {
            connection: Self::start(client, Some(route_info.clone()), listener)?,
            udp_note,
            route: route_info,
            addresses,
        })
    }

    /// The title of each session that is live now, as the server's terminal sets
    /// it; an empty string when it set none. Asks the server to push the titles of
    /// sessions this connection has not attached, which takes a round trip or two.
    pub(crate) fn session_titles(&self, live: &[SessionId]) -> HashMap<SessionId, String> {
        let mut titles = self.terminals.titles();
        let attached = self.terminals.sessions();
        let wanted: Vec<SessionId> = live
            .iter()
            .copied()
            .filter(|session| !attached.contains(session))
            .collect();
        if !wanted.is_empty() {
            self.titles.begin();
            self.terminals.referenced();
            let mut references = attached;
            references.extend(&wanted);
            if self.client.sync_session_references(references).is_ok() {
                self.titles.wait(&wanted, TITLE_WAIT);
            }
            self.track();
            let known = self.titles.lock();
            for session in wanted {
                if let Some(title) = known.titles.get(&session) {
                    titles.insert(session, title.clone());
                }
            }
        }
        titles
    }

    /// Keeps the server's idea of which sessions this connection uses equal to
    /// the attached ones, once titles asked for more. Without it a session
    /// listed earlier would count as released and not as used when attached.
    pub(crate) fn track(&self) {
        self.terminals.track(&self.client);
    }

    fn start(
        client: Client,
        route: Option<RouteInfo>,
        listener: Arc<dyn ConnectionListener>,
    ) -> Result<Arc<Self>, MobileError> {
        let events = client.events().ok_or(MobileError::Disconnected)?;
        // Keeps `Client::rtt` fresh for the local echo and the app's latency readout.
        let _ = client.start_rtt_probe();
        let terminals = Arc::<Terminals>::default();
        let titles = Arc::<Titles>::default();
        let pump = (client.clone(), Arc::clone(&terminals), Arc::clone(&titles));
        thread::Builder::new()
            .name("atur-mobile-events".into())
            .spawn(move || deliver(&pump.0, &events, &pump.1, &pump.2, listener.as_ref()))
            .map_err(|error| MobileError::Unreachable {
                reason: error.to_string(),
            })?;
        Ok(Arc::new(Self {
            client,
            route,
            terminals,
            titles,
            attaching: Mutex::new(()),
        }))
    }
}

fn remote(credential: ServerCredential) -> Result<(RemoteEndpoint, DeviceCredential), MobileError> {
    let endpoint = RemoteEndpoint {
        hosts: credential.hosts,
        port: credential.port,
        fingerprint: credential
            .fingerprint
            .as_slice()
            .try_into()
            .map_err(|_| MobileError::InvalidCredential)?,
    };
    let device = DeviceCredential {
        device: credential
            .device_id
            .parse()
            .map_err(|_| MobileError::InvalidCredential)?,
        token: credential
            .token
            .as_slice()
            .try_into()
            .map_err(|_| MobileError::InvalidCredential)?,
    };
    Ok((endpoint, device))
}

/// Replaces a TLS connection with a UDP one to the same server, when the server
/// can offer one and answers it. Otherwise the TLS connection stays, with the
/// reason UDP was not used.
fn upgrade(tls: Client, host: &str, device: DeviceCredential) -> (Client, String) {
    if !tls.supports_udp() {
        return (tls, "older server".into());
    }
    let offer = match tls.request_udp() {
        Ok(offer) => offer,
        Err(ClientError::Server(error)) if error.code == ErrorCode::Unsupported => {
            return (tls, "turned off".into());
        }
        Err(error) => return (tls, format!("no offer: {error}")),
    };
    let settings = UdpSettings::default();
    match Client::connect_remote_udp(host, &offer, device, &settings) {
        Ok(udp) => {
            tls.disconnect();
            (udp, String::new())
        }
        Err(ClientError::Io(error)) if error.kind() == io::ErrorKind::TimedOut => (
            tls,
            format!("no answer in {} s", settings.connect_timeout.as_secs()),
        ),
        Err(error) => (tls, error.to_string()),
    }
}

fn project(id: &str) -> Result<ProjectId, MobileError> {
    id.parse().map_err(|_| MobileError::Server {
        reason: "Unknown project.".into(),
    })
}

pub(crate) fn find_project(
    projects: &[ProjectDescriptor],
    id: ProjectId,
) -> Result<&ProjectDescriptor, MobileError> {
    projects
        .iter()
        .find(|project| project.id == id)
        .ok_or_else(|| MobileError::Server {
            reason: "The project no longer exists.".into(),
        })
}

fn session(id: u64) -> Result<SessionId, MobileError> {
    SessionId::new(id).ok_or_else(|| MobileError::Server {
        reason: "Unknown session.".into(),
    })
}

/// Reads every page of one consistent listing, retrying if the catalog changes meanwhile.
fn all_sessions(client: &Client, project: ProjectId) -> Result<Vec<ProjectSession>, ClientError> {
    let mut changed = None;
    for _ in 0..8 {
        match session_pages(client, project) {
            Err(ClientError::Server(error)) if error.code == ErrorCode::CatalogChanged => {
                changed = Some(error);
            }
            result => return result,
        }
    }
    Err(changed.map_or(ClientError::Timeout, ClientError::Server))
}

fn session_pages(client: &Client, project: ProjectId) -> Result<Vec<ProjectSession>, ClientError> {
    let mut page = client.project_sessions(project, None, None)?;
    let revision = page.revision;
    let mut sessions = std::mem::take(&mut page.sessions);
    while let Some(after) = page.next {
        page = client.project_sessions(project, Some(after), Some(revision))?;
        if page.next.is_some_and(|next| next <= after) {
            return Err(ClientError::Protocol("invalid session pagination".into()));
        }
        sessions.append(&mut page.sessions);
    }
    Ok(sessions)
}

/// Applies frames and acknowledges them, then tells the app what to read again.
fn deliver(
    client: &Client,
    events: &Receiver<ClientEvent>,
    terminals: &Terminals,
    titles: &Titles,
    listener: &dyn ConnectionListener,
) {
    for event in events {
        let notice = match event {
            ClientEvent::Frame { channel, frame } => match terminals.frame(channel, frame) {
                Delivery::Applied { session, seq } => {
                    let _ = client.ack(channel, seq);
                    Some(ConnectionEvent::ScreenChanged {
                        session_id: session.get(),
                    })
                }
                Delivery::Held | Delivery::Unknown => None,
            },
            ClientEvent::Metadata { channel, event } => {
                terminals
                    .metadata(channel, event)
                    .map(|session| ConnectionEvent::MetadataChanged {
                        session_id: session.get(),
                    })
            }
            ClientEvent::SessionEnded { session, .. } => {
                terminals.ended(session);
                terminals.track(client);
                Some(ConnectionEvent::SessionEnded {
                    session_id: session.get(),
                })
            }
            ClientEvent::CatalogChanged { .. } => Some(ConnectionEvent::CatalogChanged),
            ClientEvent::SessionsChanged { .. } => Some(ConnectionEvent::SessionsChanged),
            ClientEvent::ActivityChanged { .. } => Some(ConnectionEvent::ActivityChanged),
            ClientEvent::GitChanged { project } => Some(ConnectionEvent::GitChanged {
                project_id: project.to_string(),
            }),
            ClientEvent::FilesChanged { project, changes } => Some(ConnectionEvent::FilesChanged {
                project_id: project.to_string(),
                paths: if changes.rescan {
                    Vec::new()
                } else {
                    changes.paths.iter().map(text).collect()
                },
            }),
            ClientEvent::ServerRestarting => Some(ConnectionEvent::ServerRestarting),
            ClientEvent::Disconnected => {
                listener.on_event(ConnectionEvent::Disconnected);
                return;
            }
            ClientEvent::SessionMetadata { session, metadata } => {
                titles.record(session, metadata.title);
                None
            }
            ClientEvent::ClipboardWrite { .. }
            | ClientEvent::Progress { .. }
            | ClientEvent::RemoteAccessChanged { .. } => None,
        };
        if let Some(notice) = notice {
            listener.on_event(notice);
        }
    }
}
