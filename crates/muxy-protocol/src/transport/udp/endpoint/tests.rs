//! Endpoints over real loopback sockets, with a proxy that drops, delays, and
//! moves traffic.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use super::*;

const SECRET: [u8; 32] = [9; 32];

fn secret() -> Secret {
    Secret::from_bytes(SECRET)
}

fn loopback(port: u16) -> SocketAddr {
    (Ipv4Addr::LOCALHOST, port).into()
}

fn options() -> Options {
    Options {
        idle_timeout: Duration::from_secs(30),
        claim_timeout: Duration::from_secs(30),
    }
}

fn server() -> UdpServer {
    UdpServer::bind(40_000..=40_999, &secret(), options()).unwrap()
}

/// Forwards datagrams between a client and a server.
struct Proxy {
    port: u16,
    blackhole: Arc<AtomicBool>,
    loss: Arc<AtomicU64>,
    upstream: Arc<Mutex<Arc<UdpSocket>>>,
    client: Arc<Mutex<Option<SocketAddr>>>,
    front: Arc<UdpSocket>,
    forwarded: Arc<AtomicU64>,
}

impl Proxy {
    fn start(server: SocketAddr) -> Self {
        let front = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap());
        let port = front.local_addr().unwrap().port();
        let first = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap());
        let proxy = Self {
            port,
            blackhole: Arc::new(AtomicBool::new(false)),
            loss: Arc::new(AtomicU64::new(0)),
            upstream: Arc::new(Mutex::new(Arc::clone(&first))),
            client: Arc::new(Mutex::new(None)),
            front: Arc::clone(&front),
            forwarded: Arc::new(AtomicU64::new(0)),
        };
        proxy.relay_replies(first);
        let (blackhole, loss, upstream, client, forwarded) = (
            Arc::clone(&proxy.blackhole),
            Arc::clone(&proxy.loss),
            Arc::clone(&proxy.upstream),
            Arc::clone(&proxy.client),
            Arc::clone(&proxy.forwarded),
        );
        thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            let mut state = 12345u64;
            while let Ok((length, from)) = front.recv_from(&mut buffer) {
                *client.lock().unwrap() = Some(from);
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let drop_it = (state >> 33) % 1000 < loss.load(Ordering::Relaxed);
                if blackhole.load(Ordering::Relaxed) || drop_it {
                    continue;
                }
                forwarded.fetch_add(1, Ordering::Relaxed);
                let socket = Arc::clone(&upstream.lock().unwrap());
                let _ = socket.send_to(&buffer[..length], server);
            }
        });
        proxy
    }

    fn relay_replies(&self, socket: Arc<UdpSocket>) {
        let (front, client, blackhole) = (
            Arc::clone(&self.front),
            Arc::clone(&self.client),
            Arc::clone(&self.blackhole),
        );
        thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            while let Ok((length, _)) = socket.recv_from(&mut buffer) {
                let target = *client.lock().unwrap();
                if let Some(target) = target
                    && !blackhole.load(Ordering::Relaxed)
                {
                    let _ = front.send_to(&buffer[..length], target);
                }
            }
        });
    }

    /// The server now sees the client at a new address, as after a change of
    /// network.
    fn roam(&self) {
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap());
        self.relay_replies(Arc::clone(&socket));
        *self.upstream.lock().unwrap() = socket;
    }

    fn set_blackhole(&self, on: bool) {
        self.blackhole.store(on, Ordering::Relaxed);
    }
}

fn pair() -> (UdpStatus, UdpStream, UdpStream, Proxy) {
    let server = server();
    let status = server.status();
    let proxy = Proxy::start(loopback(server.port()));
    let client = connect(
        &[loopback(proxy.port)],
        &secret(),
        Duration::from_secs(5),
        options(),
    )
    .unwrap();
    (status, server.into_stream(), client, proxy)
}

fn halves(stream: UdpStream) -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
    Box::new(stream).split().unwrap()
}

fn read_exactly(reader: &mut dyn Read, length: usize) -> Vec<u8> {
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    bytes
}

#[test]
fn bytes_flow_both_ways_after_the_handshake() {
    let (status, server, client, _proxy) = pair();
    assert!(status.wait_claimed(Duration::from_secs(1)));
    let (mut server_reader, mut server_writer) = halves(server);
    let (mut client_reader, mut client_writer) = halves(client);
    client_writer.write_all(b"hello server").unwrap();
    assert_eq!(read_exactly(&mut *server_reader, 12), b"hello server");
    server_writer.write_all(b"hello client").unwrap();
    assert_eq!(read_exactly(&mut *client_reader, 12), b"hello client");
}

#[test]
fn a_megabyte_survives_ten_percent_loss() {
    let (_status, server, client, proxy) = pair();
    proxy.loss.store(100, Ordering::Relaxed);
    let (mut server_reader, _server_writer) = halves(server);
    let (_client_reader, mut client_writer) = halves(client);
    let data: Vec<u8> = (0..1 << 20)
        .map(|index: u32| u8::try_from(index % 251).unwrap())
        .collect();
    let sending = data.clone();
    let writer = thread::spawn(move || {
        client_writer.write_all(&sending).unwrap();
        client_writer
    });
    assert_eq!(read_exactly(&mut *server_reader, data.len()), data);
    drop(writer.join().unwrap());
}

#[test]
fn a_wrong_secret_gets_no_answer_and_claims_nothing() {
    let server = server();
    let status = server.status();
    let started = Instant::now();
    let error = connect(
        &[loopback(server.port())],
        &Secret::from_bytes([1; 32]),
        Duration::from_millis(700),
        options(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!status.claimed());
    assert_eq!(
        status.stats().packets_sent,
        0,
        "a refused packet gets no reply"
    );
}

#[test]
fn garbage_and_replays_are_ignored_and_floods_do_not_block_the_client() {
    let server = server();
    let status = server.status();
    let port = server.port();
    let attacker = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    for index in 0..20_000u32 {
        let mut garbage = vec![0u8; 20 + (index % 200) as usize];
        garbage[..4].copy_from_slice(&index.to_be_bytes());
        attacker.send_to(&garbage, loopback(port)).unwrap();
    }
    assert!(!status.claimed());
    let client = connect(
        &[loopback(port)],
        &secret(),
        Duration::from_secs(5),
        options(),
    )
    .unwrap();
    assert!(status.wait_claimed(Duration::from_secs(1)));
    drop(client);
}

#[test]
fn the_server_follows_a_client_that_changes_address() {
    let (status, server, client, proxy) = pair();
    let (mut server_reader, mut server_writer) = halves(server);
    let (mut client_reader, mut client_writer) = halves(client);
    client_writer.write_all(b"before").unwrap();
    assert_eq!(read_exactly(&mut *server_reader, 6), b"before");
    let first = status.peer().unwrap();
    proxy.roam();
    client_writer.write_all(b"after").unwrap();
    assert_eq!(read_exactly(&mut *server_reader, 5), b"after");
    assert_ne!(status.peer().unwrap(), first);
    server_writer.write_all(b"reply").unwrap();
    assert_eq!(read_exactly(&mut *client_reader, 5), b"reply");
}

#[test]
fn the_stream_resumes_after_an_outage_without_reconnecting() {
    let (_status, server, client, proxy) = pair();
    let (mut server_reader, mut server_writer) = halves(server);
    let (mut client_reader, mut client_writer) = halves(client);
    client_writer.write_all(b"one").unwrap();
    assert_eq!(read_exactly(&mut *server_reader, 3), b"one");
    proxy.set_blackhole(true);
    client_writer.write_all(b"two").unwrap();
    server_writer.write_all(b"down").unwrap();
    thread::sleep(Duration::from_secs(3));
    let restored = Instant::now();
    proxy.set_blackhole(false);
    assert_eq!(read_exactly(&mut *server_reader, 3), b"two");
    assert_eq!(read_exactly(&mut *client_reader, 4), b"down");
    assert!(
        restored.elapsed() < Duration::from_secs(2),
        "{:?}",
        restored.elapsed()
    );
}

#[test]
fn a_silent_client_ends_the_session_after_the_idle_timeout() {
    let server = UdpServer::bind(
        41_000..=41_999,
        &secret(),
        Options {
            idle_timeout: Duration::from_millis(800),
            claim_timeout: Duration::from_secs(30),
        },
    )
    .unwrap();
    let proxy = Proxy::start(loopback(server.port()));
    let client = connect(
        &[loopback(proxy.port)],
        &secret(),
        Duration::from_secs(5),
        options(),
    )
    .unwrap();
    let (mut reader, _writer) = halves(server.into_stream());
    proxy.set_blackhole(true);
    let started = Instant::now();
    let error = reader.read(&mut [0; 8]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(client);
}

#[test]
fn an_unclaimed_server_gives_up() {
    let server = UdpServer::bind(
        42_000..=42_999,
        &secret(),
        Options {
            idle_timeout: Duration::from_secs(30),
            claim_timeout: Duration::from_millis(500),
        },
    )
    .unwrap();
    let (mut reader, _writer) = halves(server.into_stream());
    let error = reader.read(&mut [0; 8]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn closing_one_end_tells_the_other_at_once() {
    let (_status, server, client, _proxy) = pair();
    let (mut server_reader, _server_writer) = halves(server);
    let cancellation = client.cancellation().unwrap();
    cancellation.cancel();
    let started = Instant::now();
    let error = server_reader.read(&mut [0; 8]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn ending_the_writer_delivers_everything_then_end_of_stream() {
    let (status, server, client, _proxy) = pair();
    let (mut server_reader, _server_writer) = halves(server);
    let (_client_reader, mut client_writer) = halves(client);
    client_writer.write_all(b"last words").unwrap();
    drop(client_writer);
    let mut received = Vec::new();
    server_reader.read_to_end(&mut received).unwrap();
    assert_eq!(received, b"last words");
    assert!(status.claimed());
}

#[test]
fn the_port_comes_from_the_requested_range() {
    let server = UdpServer::bind(43_210..=43_210, &secret(), options()).unwrap();
    assert_eq!(server.port(), 43_210);
    let second = UdpServer::bind(43_210..=43_210, &secret(), options());
    assert_eq!(second.unwrap_err().kind(), io::ErrorKind::AddrInUse);
    #[allow(clippy::reversed_empty_ranges)]
    let empty = 5..=4;
    assert!(UdpServer::bind(empty, &secret(), options()).is_err());
}
