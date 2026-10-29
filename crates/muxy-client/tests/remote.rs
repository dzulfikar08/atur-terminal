use std::io;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use muxy_client::{Client, ClientError, RemoteEndpoint, Route, RouteKind};
use muxy_protocol::transport::Listener;
use muxy_protocol::transport::tls::TlsListener;
use muxy_protocol::{ListenerStatus, PairingOffer, RemoteAccessSettings};
use muxy_server::connection::{serve, serve_remote};
use muxy_server::{Registry, ServerEvent, ServerSettings};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// An in-process server whose network listener is a real TLS listener on loopback.
struct Server {
    registry: Arc<Registry>,
    _events: mpsc::Receiver<ServerEvent>,
}

impl Server {
    fn new() -> Self {
        Self::bound(Ipv4Addr::LOCALHOST)
    }

    /// A server whose network listener binds `address`: loopback, one other
    /// loopback address (`127.0.0.2` is loopback on Linux), or all of them.
    fn bound(address: Ipv4Addr) -> Self {
        let (sender, events) = mpsc::channel();
        let slot = Arc::new(OnceLock::<Weak<Registry>>::new());
        let owner = Arc::clone(&slot);
        let running = Mutex::new(None::<Arc<TlsListener>>);
        let registry = Arc::new(
            Registry::new(
                ServerSettings {
                    default_shell: Some(PathBuf::from("/bin/sh")),
                    ..ServerSettings::default()
                },
                sender,
            )
            .with_remote_listener(move |listening| {
                let mut running = running.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(previous) = running.take() {
                    previous.close();
                }
                let Some((port, identity)) = listening else {
                    return ListenerStatus::Disabled;
                };
                match TlsListener::bind(SocketAddr::from((address, port)), &identity) {
                    Ok(listener) => {
                        let listener = Arc::new(listener);
                        *running = Some(Arc::clone(&listener));
                        let registry = owner.get().cloned().unwrap_or_default();
                        thread::spawn(move || accept(&listener, &registry));
                        ListenerStatus::Listening
                    }
                    Err(error) => ListenerStatus::Failed(error.to_string()),
                }
            }),
        );
        let _ = slot.set(Arc::downgrade(&registry));
        Self {
            registry,
            _events: events,
        }
    }

    fn local(&self) -> TestResult<Client> {
        let (client, server) = UnixStream::pair()?;
        let registry = Arc::clone(&self.registry);
        thread::spawn(move || {
            let (_keep, events) = mpsc::channel();
            let _ = serve(Box::new(server), registry, events);
        });
        Ok(Client::from_stream(Box::new(client))?)
    }

    /// Enables mobile access and returns the local client with a fresh offer.
    /// Tests running alongside can take the chosen port before the server
    /// listens on it, so a taken port is replaced.
    fn pairing(&self) -> TestResult<(Client, PairingOffer)> {
        self.pairing_via("127.0.0.1", None)
    }

    /// Like [`Server::pairing`], with the invite listing `host`. With a `delay`,
    /// a proxy also listens on `127.0.0.2` at the server's port and passes each
    /// connection on to `127.0.0.1` only after the delay: a slow route to the
    /// same server.
    fn pairing_via(
        &self,
        host: &str,
        delay: Option<Duration>,
    ) -> TestResult<(Client, PairingOffer)> {
        let local = self.local()?;
        let mut status = ListenerStatus::Disabled;
        for _ in 0..8 {
            let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
            let proxy = match delay {
                Some(_) => match TcpListener::bind((Ipv4Addr::new(127, 0, 0, 2), port)) {
                    Ok(listener) => Some(listener),
                    Err(_) => continue,
                },
                None => None,
            };
            status = local
                .write_remote_access(RemoteAccessSettings {
                    enabled: true,
                    port,
                })?
                .status;
            if status == ListenerStatus::Listening {
                if let (Some(listener), Some(delay)) = (proxy, delay) {
                    slow_proxy(listener, port, delay);
                }
                break;
            }
        }
        assert_eq!(status, ListenerStatus::Listening);
        let mut offer = local.start_pairing()?;
        offer.invite.hosts = vec![host.into()];
        Ok((local, offer))
    }
}

/// Accepts on `listener` and connects to `127.0.0.1:port` only `delay` after
/// each connection arrived, then copies bytes both ways.
fn slow_proxy(listener: TcpListener, port: u16, delay: Duration) {
    thread::spawn(move || {
        while let Ok((client, _)) = listener.accept() {
            thread::spawn(move || {
                thread::sleep(delay);
                if let Ok(upstream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
                    let _ = copy_both_ways(client, upstream);
                }
            });
        }
    });
}

fn copy_both_ways(mut client: TcpStream, mut upstream: TcpStream) -> io::Result<()> {
    let mut from_client = client.try_clone()?;
    let mut to_upstream = upstream.try_clone()?;
    let forward = thread::spawn(move || {
        let _ = io::copy(&mut from_client, &mut to_upstream);
        let _ = to_upstream.shutdown(Shutdown::Write);
    });
    io::copy(&mut upstream, &mut client)?;
    let _ = client.shutdown(Shutdown::Write);
    let _ = forward.join();
    Ok(())
}

fn accept(listener: &TlsListener, registry: &Weak<Registry>) {
    while let Ok(stream) = listener.accept() {
        let Some(registry) = registry.upgrade() else {
            return;
        };
        let Some(admission) = registry.admit_remote() else {
            continue;
        };
        thread::spawn(move || {
            let (_keep, events) = mpsc::channel();
            let _ = serve_remote(stream, registry, events, admission);
        });
    }
}

#[test]
fn an_unreachable_host_falls_back_and_a_foreign_certificate_is_a_mismatch() -> TestResult {
    let server = Server::new();
    let (_local, offer) = server.pairing()?;
    let mut endpoint = RemoteEndpoint::from(&offer.invite);
    endpoint.hosts.insert(0, "::1".into());
    let (_phone, paired) = Client::pair(&offer.invite, "Phone")?;
    Client::connect_remote(&endpoint, paired.credential)?;
    endpoint.fingerprint = [0; 32];
    assert!(matches!(
        Client::connect_remote(&endpoint, paired.credential),
        Err(ClientError::IdentityMismatch)
    ));
    Ok(())
}

fn hosts(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// An endpoint for `hosts` of the server that made `offer`.
fn endpoint_for(offer: &PairingOffer, hosts: Vec<String>) -> RemoteEndpoint {
    let mut endpoint = RemoteEndpoint::from(&offer.invite);
    endpoint.hosts = hosts;
    endpoint
}

/// Pairs with the server of `offer`. The offer's secret works once: a test that connects
/// more than once pairs here once and reuses the credential.
fn pair(offer: &PairingOffer) -> TestResult<muxy_protocol::DeviceCredential> {
    let (_phone, paired) = Client::pair(&offer.invite, "Phone")?;
    Ok(paired.credential)
}

/// Pairs once, then returns an endpoint for `hosts`.
fn paired_endpoint(
    offer: &PairingOffer,
    hosts: Vec<String>,
) -> TestResult<(RemoteEndpoint, muxy_protocol::DeviceCredential)> {
    Ok((endpoint_for(offer, hosts), pair(offer)?))
}

/// One line per race, which the `route` job copies into its summary.
#[allow(
    clippy::print_stdout,
    reason = "the route job copies this line into its summary"
)]
fn report(case: &str, route: &Route, total: Duration) {
    println!(
        "route race winner={} ms={} kind={} total_ms={} case={case}",
        route.host,
        route.elapsed.as_millis(),
        route.kind.name(),
        total.as_millis()
    );
}

// 127.0.0.2 is loopback on Linux only.
#[cfg(target_os = "linux")]
#[test]
fn the_faster_address_wins_even_when_it_is_listed_second() -> TestResult {
    let server = Server::new();
    let (_local, offer) = server.pairing_via("127.0.0.1", Some(Duration::from_millis(1500)))?;
    let (endpoint, credential) = paired_endpoint(&offer, hosts(&["127.0.0.2", "127.0.0.1"]))?;
    let started = Instant::now();
    let (client, route) = Client::connect_remote_at(&endpoint, credential)?;
    let total = started.elapsed();
    report("1500ms-proxy-listed-first", &route, total);
    assert_eq!(route.host, "127.0.0.1");
    assert_eq!(route.kind, RouteKind::Local);
    assert!(route.elapsed < Duration::from_millis(1000), "{route:?}");
    // The delayed address would have taken 1.5 s: the race did not wait for it.
    assert!(total < Duration::from_millis(1400), "{total:?}");
    client.ping()?;
    Ok(())
}

// 127.0.0.2 is loopback on Linux only.
#[cfg(target_os = "linux")]
#[test]
fn the_delayed_address_wins_when_it_is_the_only_one_that_answers() -> TestResult {
    let server = Server::bound(Ipv4Addr::new(127, 0, 0, 2));
    let (_local, offer) = server.pairing_via("127.0.0.2", None)?;
    // 127.0.0.1 and 127.0.0.3 refuse at once: the next address starts without waiting a stagger.
    let (endpoint, credential) =
        paired_endpoint(&offer, hosts(&["127.0.0.1", "127.0.0.3", "127.0.0.2"]))?;
    let started = Instant::now();
    let (client, route) = Client::connect_remote_at(&endpoint, credential)?;
    let total = started.elapsed();
    report("only-one-that-answers", &route, total);
    assert_eq!(route.host, "127.0.0.2");
    assert!(total < Duration::from_secs(2), "{total:?}");
    client.ping()?;
    Ok(())
}

#[test]
fn a_single_address_is_connected_as_before() -> TestResult {
    let server = Server::new();
    let (_local, offer) = server.pairing()?;
    let (endpoint, credential) = paired_endpoint(&offer, hosts(&["127.0.0.1"]))?;
    let (client, route) = Client::connect_remote_at(&endpoint, credential)?;
    assert_eq!(route.host, "127.0.0.1");
    assert_eq!(route.kind, RouteKind::Local);
    client.ping()?;
    // No address at all is an error, not a hang.
    let endpoint = endpoint_for(&offer, Vec::new());
    assert!(matches!(
        Client::connect_remote_at(&endpoint, credential),
        Err(ClientError::Io(_))
    ));
    Ok(())
}

/// A wrong certificate on the addresses that answer is a mismatch, whichever of them
/// is tried first and whatever the ones that refuse say.
#[test]
fn a_race_reports_a_foreign_certificate_as_a_mismatch() -> TestResult {
    let server = Server::bound(Ipv4Addr::UNSPECIFIED);
    let (_local, offer) = server.pairing_via("127.0.0.1", None)?;
    let (mut endpoint, credential) =
        paired_endpoint(&offer, hosts(&["127.0.0.3", "127.0.0.1", "127.0.0.2"]))?;
    endpoint.fingerprint = [0; 32];
    assert!(matches!(
        Client::connect_remote_at(&endpoint, credential),
        Err(ClientError::IdentityMismatch)
    ));
    Ok(())
}

/// The CI `route` job shapes `127.0.0.2` alone (a 120 ms delay each way, see
/// `scripts/route-netem.sh`) and runs the tests below with `--ignored`.
#[test]
#[ignore = "needs 127.0.0.2 delayed with scripts/route-netem.sh on"]
fn netem_the_fast_address_beats_a_delayed_one_listed_first() -> TestResult {
    let server = Server::bound(Ipv4Addr::UNSPECIFIED);
    let (_local, offer) = server.pairing_via("127.0.0.1", None)?;
    // Alone, the delayed address needs several round trips of 240 ms.
    let credential = pair(&offer)?;
    let alone = endpoint_for(&offer, hosts(&["127.0.0.2"]));
    let started = Instant::now();
    let (_client, route) = Client::connect_remote_at(&alone, credential)?;
    report("delayed-alone-netem", &route, started.elapsed());
    assert!(
        route.elapsed >= Duration::from_millis(600),
        "127.0.0.2 is not delayed: {route:?}"
    );
    let both = endpoint_for(&offer, hosts(&["127.0.0.2", "127.0.0.1"]));
    let started = Instant::now();
    let (client, route) = Client::connect_remote_at(&both, credential)?;
    let total = started.elapsed();
    report("fast-and-delayed-netem", &route, total);
    assert_eq!(route.host, "127.0.0.1");
    assert_eq!(route.kind, RouteKind::Local);
    assert!(total < Duration::from_millis(600), "{total:?}");
    client.ping()?;
    Ok(())
}

#[test]
#[ignore = "needs 127.0.0.2 delayed with scripts/route-netem.sh on"]
fn netem_the_delayed_address_wins_when_it_is_the_only_one_reachable() -> TestResult {
    let server = Server::bound(Ipv4Addr::new(127, 0, 0, 2));
    let (_local, offer) = server.pairing_via("127.0.0.2", None)?;
    let (endpoint, credential) =
        paired_endpoint(&offer, hosts(&["127.0.0.1", "127.0.0.3", "127.0.0.2"]))?;
    let started = Instant::now();
    let (client, route) = Client::connect_remote_at(&endpoint, credential)?;
    let total = started.elapsed();
    report("only-one-that-answers-netem", &route, total);
    assert_eq!(route.host, "127.0.0.2");
    assert!(route.elapsed >= Duration::from_millis(600), "{route:?}");
    client.ping()?;
    Ok(())
}
