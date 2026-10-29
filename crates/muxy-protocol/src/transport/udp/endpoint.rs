//! Sockets, threads, and the byte stream that callers read and write.
//!
//! Two threads serve one connection: one blocks on the socket and feeds
//! packets to the state machine, the other asks the state machine what to send
//! and when to wake. Everything lives behind one lock; sending a datagram
//! takes microseconds.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::ops::RangeInclusive;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};
use rustix::event::{PollFd, PollFlags, Timespec};

use super::connection::{Connection, Stats};
use super::crypto::{Opener, Role, Sealer, Secret};
use crate::transport::{ByteStream, StreamCancellation};

/// Reads give up on a silent socket this often to notice a closed stream.
const SOCKET_TICK: Duration = Duration::from_millis(200);
/// A client that hears nothing opens a fresh socket this often, since the old
/// one may be tied to a network that is gone.
const REBIND_AFTER: Duration = Duration::from_secs(6);
/// Gaps between the first packets a client sends while connecting.
const HELLO_GAPS: [u64; 8] = [0, 100, 150, 250, 400, 600, 900, 1300];
const HELLO_REPEAT: Duration = Duration::from_millis(700);
/// Packets sent per wake before checking on the rest of the work.
const SEND_BURST: usize = 64;
/// Unauthentic packets from unknown addresses the server examines per second.
const UNKNOWN_SOURCE_RATE: f64 = 50.0;
const UNKNOWN_SOURCE_BURST: f64 = 100.0;

/// How long an endpoint waits before it gives a silent peer up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    /// With no authentic packet for this long, the stream fails. Only time the
    /// computer is awake counts, so a laptop's sleep does not.
    pub idle_timeout: Duration,
    /// A server nobody has contacted after this long gives up.
    pub claim_timeout: Duration,
}

impl Options {
    pub const CLIENT: Self = Self {
        idle_timeout: Duration::from_secs(5 * 60),
        claim_timeout: Duration::from_secs(60),
    };
    pub const SERVER: Self = Self {
        idle_timeout: Duration::from_secs(60 * 60),
        claim_timeout: Duration::from_secs(60),
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Closed {
    /// This end closed the stream.
    Local,
    /// The peer said to drop it.
    Reset,
    /// Nothing was heard for too long.
    Idle,
}

struct Limiter {
    tokens: f64,
    at: Instant,
}

impl Limiter {
    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        self.tokens = (self.tokens + elapsed * UNKNOWN_SOURCE_RATE).min(UNKNOWN_SOURCE_BURST);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct State {
    connection: Connection,
    sealer: Sealer,
    opener: Opener,
    role: Role,
    socket: Arc<UdpSocket>,
    peer: Option<SocketAddr>,
    claimed: bool,
    closed: Option<Closed>,
    options: Options,
    started: Instant,
    limiter: Limiter,
    last_rebind: Instant,
    send_failures: u32,
}

struct Shared {
    state: Mutex<State>,
    readable: Condvar,
    writable: Condvar,
    wake: Condvar,
    claim: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn close(&self, state: &mut State, why: Closed) {
        if state.closed.is_none() {
            state.closed = Some(why);
        }
        self.readable.notify_all();
        self.writable.notify_all();
        self.wake.notify_all();
        self.claim.notify_all();
    }

    fn close_local(&self) {
        let mut state = self.lock();
        if state.closed.is_none() {
            state.connection.abort();
            self.close(&mut state, Closed::Local);
        }
    }

    /// Handles one datagram from the socket.
    fn receive(&self, from: SocketAddr, datagram: &[u8], plain: &mut Vec<u8>) {
        let now = Instant::now();
        let mut state = self.lock();
        if state.closed.is_some() {
            return;
        }
        let known = state.peer == Some(from);
        if state.role == Role::Server && !known && !state.limiter.take(now) {
            return;
        }
        plain.clear();
        let Some(counter) = state.opener.open(datagram, plain) else {
            return;
        };
        if state.connection.on_packet(now, plain).is_err() {
            return;
        }
        if state.role == Role::Server {
            // The newest packet says where the client is now.
            if state.opener.highest() == Some(counter) {
                state.peer = Some(from);
            }
            if !state.claimed {
                state.claimed = true;
                self.claim.notify_all();
            }
        }
        if state.connection.reset_by_peer() {
            self.close(&mut state, Closed::Reset);
            return;
        }
        state.send_failures = 0;
        self.readable.notify_all();
        self.writable.notify_all();
        self.wake.notify_one();
    }
}

fn receive_loop(shared: &Shared) {
    let mut buffer = vec![0u8; 2048];
    let mut plain = Vec::with_capacity(2048);
    loop {
        let socket = {
            let state = shared.lock();
            if state.closed.is_some() {
                return;
            }
            Arc::clone(&state.socket)
        };
        match socket.recv_from(&mut buffer) {
            Ok((length, from)) => shared.receive(from, &buffer[..length], &mut plain),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            // ICMP errors and a socket swapped out from under us.
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn send_loop(shared: &Shared) {
    let mut plain = Vec::with_capacity(2048);
    let mut wire = Vec::with_capacity(2048);
    let mut state = shared.lock();
    loop {
        let now = Instant::now();
        if let Some(closed) = state.closed {
            if closed == Closed::Local {
                transmit(&mut state, now, &mut plain, &mut wire);
            }
            return;
        }
        let silent = now.saturating_duration_since(state.connection.last_heard());
        let patience = if state.claimed || state.role == Role::Client {
            state.options.idle_timeout
        } else {
            state.options.claim_timeout
        };
        if silent > patience {
            shared.close(&mut state, Closed::Idle);
            continue;
        }
        if state.role == Role::Client {
            maybe_rebind(&mut state, now, silent);
        }
        transmit(&mut state, now, &mut plain, &mut wire);

        let mut deadline = state.connection.last_heard() + patience;
        if state.peer.is_some()
            && let Some(wake) = state.connection.next_wakeup(now)
        {
            deadline = deadline.min(wake);
        }
        let timeout = deadline
            .saturating_duration_since(now)
            .clamp(Duration::from_millis(1), Duration::from_secs(1));
        state = shared
            .wake
            .wait_timeout(state, timeout)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

fn transmit(state: &mut State, now: Instant, plain: &mut Vec<u8>, wire: &mut Vec<u8>) {
    let Some(peer) = state.peer else {
        return;
    };
    for _ in 0..SEND_BURST {
        plain.clear();
        if !state.connection.poll_transmit(now, plain) {
            break;
        }
        wire.clear();
        state.sealer.seal(plain, wire);
        if state.socket.send_to(wire, peer).is_err() {
            state.send_failures += 1;
        }
    }
}

/// A client's socket can be tied to a network that is gone, such as a Wi-Fi
/// interface it left. Binding again picks up whatever route exists now.
fn maybe_rebind(state: &mut State, now: Instant, silent: Duration) {
    let failing = state.send_failures >= 5;
    if !(silent > REBIND_AFTER || failing)
        || now.saturating_duration_since(state.last_rebind) < REBIND_AFTER
    {
        return;
    }
    let Some(peer) = state.peer else {
        return;
    };
    if let Ok(socket) = bind_any(peer.is_ipv6()) {
        state.socket = Arc::new(socket);
        state.send_failures = 0;
    }
    state.last_rebind = now;
}

fn bind_any(ipv6: bool) -> io::Result<UdpSocket> {
    let address: SocketAddr = if ipv6 {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let socket = UdpSocket::bind(address)?;
    socket.set_read_timeout(Some(SOCKET_TICK))?;
    Ok(socket)
}

fn start(
    role: Role,
    socket: UdpSocket,
    peer: Option<SocketAddr>,
    connection: Connection,
    sealer: Sealer,
    opener: Opener,
    options: Options,
) -> io::Result<UdpStream> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(SOCKET_TICK))?;
    let now = Instant::now();
    let claimed = role == Role::Client;
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            connection,
            sealer,
            opener,
            role,
            socket: Arc::new(socket),
            peer,
            claimed,
            closed: None,
            options,
            started: now,
            limiter: Limiter {
                tokens: UNKNOWN_SOURCE_BURST,
                at: now,
            },
            last_rebind: now,
            send_failures: 0,
        }),
        readable: Condvar::new(),
        writable: Condvar::new(),
        wake: Condvar::new(),
        claim: Condvar::new(),
    });
    let receiving = Arc::clone(&shared);
    thread::Builder::new()
        .name("udp-receive".into())
        .spawn(move || receive_loop(&receiving))?;
    let sending = Arc::clone(&shared);
    thread::Builder::new()
        .name("udp-send".into())
        .spawn(move || send_loop(&sending))?;
    Ok(UdpStream {
        handle: Arc::new(Handle { shared }),
    })
}

/// Closes the stream once the last reader, writer, and cancellation are gone.
struct Handle {
    shared: Arc<Shared>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.close_local();
    }
}

/// The byte stream, ready to split into a reader and a writer.
#[derive(Clone)]
pub struct UdpStream {
    handle: Arc<Handle>,
}

impl std::fmt::Debug for UdpStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("UdpStream").finish_non_exhaustive()
    }
}

impl UdpStream {
    pub fn status(&self) -> UdpStatus {
        UdpStatus {
            shared: Arc::clone(&self.handle.shared),
        }
    }
}

impl ByteStream for UdpStream {
    fn cancellation(&self) -> io::Result<Box<dyn StreamCancellation>> {
        Ok(Box::new(Cancellation {
            shared: Arc::clone(&self.handle.shared),
        }))
    }

    #[allow(clippy::type_complexity)]
    fn split(self: Box<Self>) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        Ok((
            Box::new(Reader {
                handle: Arc::clone(&self.handle),
            }),
            Box::new(Writer {
                handle: Arc::clone(&self.handle),
            }),
        ))
    }
}

struct Cancellation {
    shared: Arc<Shared>,
}

impl StreamCancellation for Cancellation {
    fn cancel(&self) {
        self.shared.close_local();
    }
}

struct Reader {
    handle: Arc<Handle>,
}

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let shared = &self.handle.shared;
        let mut state = shared.lock();
        loop {
            let read = state.connection.read(buffer);
            if read > 0 {
                shared.wake.notify_one();
                return Ok(read);
            }
            if state.connection.eof() {
                return Ok(0);
            }
            match state.closed {
                Some(Closed::Local) => return Ok(0),
                Some(Closed::Reset) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "the other end closed the connection",
                    ));
                }
                Some(Closed::Idle) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "no answer over UDP",
                    ));
                }
                None => {}
            }
            state = shared
                .readable
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

struct Writer {
    handle: Arc<Handle>,
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let shared = &self.handle.shared;
        let mut state = shared.lock();
        loop {
            if let Some(closed) = state.closed {
                return Err(match closed {
                    Closed::Idle => io::Error::new(io::ErrorKind::TimedOut, "no answer over UDP"),
                    Closed::Reset => io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "the other end closed the connection",
                    ),
                    Closed::Local => {
                        io::Error::new(io::ErrorKind::BrokenPipe, "the stream is closed")
                    }
                });
            }
            let written = state.connection.write(bytes);
            if written > 0 {
                shared.wake.notify_one();
                return Ok(written);
            }
            state = shared
                .writable
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Writer {
    /// Ends this side of the stream once what was written is delivered.
    fn drop(&mut self) {
        let shared = &self.handle.shared;
        let mut state = shared.lock();
        state.connection.finish();
        shared.wake.notify_one();
    }
}

/// A view of a connection for those who do not read or write it.
#[derive(Clone)]
pub struct UdpStatus {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for UdpStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("UdpStatus").finish_non_exhaustive()
    }
}

impl UdpStatus {
    /// An authentic packet has arrived (always true for clients).
    pub fn claimed(&self) -> bool {
        self.shared.lock().claimed
    }

    /// Waits up to `timeout` for the first authentic packet.
    pub fn wait_claimed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        loop {
            if state.claimed {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || state.closed.is_some() {
                return false;
            }
            state = self
                .shared
                .claim
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Waits up to `timeout` until everything written and the end of the
    /// stream are acknowledged, or the connection is closed.
    pub fn wait_drained(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        loop {
            if state.closed.is_some() || state.connection.fin_acknowledged() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .shared
                .writable
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    pub fn stats(&self) -> Stats {
        self.shared.lock().connection.stats()
    }

    pub fn smoothed_rtt(&self) -> Option<Duration> {
        self.shared.lock().connection.smoothed_rtt()
    }

    /// Where the peer was last heard from.
    pub fn peer(&self) -> Option<SocketAddr> {
        self.shared.lock().peer
    }

    /// How long since anything authentic arrived.
    pub fn silence(&self) -> Duration {
        let state = self.shared.lock();
        Instant::now().saturating_duration_since(state.connection.last_heard())
    }

    /// Time since the connection started.
    pub fn age(&self) -> Duration {
        Instant::now().saturating_duration_since(self.shared.lock().started)
    }

    /// Bytes of the stream, sent and not yet acknowledged or never sent.
    pub fn backlog(&self) -> bool {
        self.shared.lock().connection.unsent_or_unacked()
    }
}

/// The listening end: a socket with the secret, waiting for the client.
#[derive(Debug)]
pub struct UdpServer {
    stream: UdpStream,
    port: u16,
}

impl UdpServer {
    /// Binds a random free port in `ports`. Both IPv6 and IPv4 clients reach
    /// it where the system allows.
    pub fn bind(ports: RangeInclusive<u16>, secret: &Secret, options: Options) -> io::Result<Self> {
        let (low, high) = (*ports.start(), *ports.end());
        if low == 0 || low > high {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the UDP port range is empty",
            ));
        }
        let count = u32::from(high - low) + 1;
        let mut start = [0u8; 4];
        SystemRandom::new()
            .fill(&mut start)
            .map_err(|_| io::Error::other("no random numbers available"))?;
        let start = u32::from_be_bytes(start) % count;
        let mut last = io::Error::new(
            io::ErrorKind::AddrInUse,
            "every UDP port in the range is in use",
        );
        for step in 0..count {
            let port = low + u16::try_from((start + step) % count).unwrap_or(0);
            match bind_dual_stack(port) {
                Ok(socket) => {
                    return Self::on(socket, port, secret, options);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
                    ) =>
                {
                    last = error;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    /// Serves on an already bound socket.
    pub fn on(socket: UdpSocket, port: u16, secret: &Secret, options: Options) -> io::Result<Self> {
        let stream = start(
            Role::Server,
            socket,
            None,
            Connection::new(Instant::now(), false),
            Sealer::new(secret, Role::Server)?,
            Opener::new(secret, Role::Server)?,
            options,
        )?;
        Ok(Self { stream, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn status(&self) -> UdpStatus {
        self.stream.status()
    }

    pub fn into_stream(self) -> UdpStream {
        self.stream
    }
}

fn bind_dual_stack(port: u16) -> io::Result<UdpSocket> {
    use rustix::net::{AddressFamily, SocketType, bind, socket, sockopt};
    let v6 = (|| {
        let fd = socket(AddressFamily::INET6, SocketType::DGRAM, None)?;
        sockopt::set_ipv6_v6only(&fd, false)?;
        bind(&fd, &SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0))?;
        Ok::<_, rustix::io::Errno>(UdpSocket::from(fd))
    })();
    match v6 {
        Ok(socket) => Ok(socket),
        Err(error)
            if error == rustix::io::Errno::AFNOSUPPORT
                || error == rustix::io::Errno::ADDRNOTAVAIL =>
        {
            UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
        }
        Err(error) => Err(io::Error::from(error)),
    }
}

/// Connects to a server at one of `addresses`, resolved from the host ssh
/// used. Packets go to every address until one answers; the first answer
/// wins. Fails with `TimedOut` when none does within `timeout`.
pub fn connect(
    addresses: &[SocketAddr],
    secret: &Secret,
    timeout: Duration,
    options: Options,
) -> io::Result<UdpStream> {
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no address to connect to",
        ));
    }
    let mut sockets: Vec<(UdpSocket, bool)> = Vec::new();
    for address in addresses {
        if !sockets.iter().any(|(_, ipv6)| *ipv6 == address.is_ipv6()) {
            let socket = bind_any(address.is_ipv6())?;
            socket.set_nonblocking(true)?;
            sockets.push((socket, address.is_ipv6()));
        }
    }
    let mut sealer = Sealer::new(secret, Role::Client)?;
    let mut opener = Opener::new(secret, Role::Client)?;
    let mut connection = Connection::new(Instant::now(), true);
    let started = Instant::now();
    let mut next_hello = 0usize;
    let mut next_at = started;
    let (mut plain, mut wire, mut buffer) = (Vec::new(), Vec::new(), vec![0u8; 2048]);
    loop {
        let now = Instant::now();
        if now.saturating_duration_since(started) >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the server did not answer over UDP",
            ));
        }
        if now >= next_at {
            plain.clear();
            connection.hello(now, &mut plain);
            wire.clear();
            sealer.seal(&plain, &mut wire);
            for address in addresses {
                if let Some((socket, _)) =
                    sockets.iter().find(|(_, ipv6)| *ipv6 == address.is_ipv6())
                {
                    let _ = socket.send_to(&wire, address);
                }
            }
            next_at += match HELLO_GAPS.get(next_hello + 1) {
                Some(&later) => Duration::from_millis(later),
                None => HELLO_REPEAT,
            };
            next_hello += 1;
        }
        let wait = next_at
            .saturating_duration_since(Instant::now())
            .min(timeout.saturating_sub(now.saturating_duration_since(started)));
        let mut polls: Vec<PollFd<'_>> = sockets
            .iter()
            .map(|(socket, _)| PollFd::new(socket, PollFlags::IN))
            .collect();
        let limit = Timespec::try_from(wait).map_err(io::Error::other)?;
        match rustix::event::poll(&mut polls, Some(&limit)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(io::Error::from(error)),
        }
        let readable: Vec<bool> = polls
            .iter()
            .map(|poll| poll.revents().contains(PollFlags::IN))
            .collect();
        drop(polls);
        for (index, ready) in readable.into_iter().enumerate() {
            if !ready {
                continue;
            }
            while let Ok((length, from)) = sockets[index].0.recv_from(&mut buffer) {
                plain.clear();
                let now = Instant::now();
                if opener.open(&buffer[..length], &mut plain).is_none()
                    || connection.on_packet(now, &plain).is_err()
                {
                    continue;
                }
                let (socket, _) = sockets.swap_remove(index);
                let peer = addresses
                    .iter()
                    .copied()
                    .find(|address| same_host(*address, from))
                    .unwrap_or(from);
                // Replies come from the address the hello went to.
                return start(
                    Role::Client,
                    socket,
                    Some(peer),
                    connection,
                    sealer,
                    opener,
                    options,
                );
            }
        }
    }
}

/// Whether `reply` came from `address`, allowing for an IPv4 address that a
/// dual-stack socket reports mapped into IPv6.
fn same_host(address: SocketAddr, reply: SocketAddr) -> bool {
    let unmap = |address: SocketAddr| match address {
        SocketAddr::V6(v6) => v6
            .ip()
            .to_ipv4_mapped()
            .map_or(address, |v4| SocketAddr::new(v4.into(), v6.port())),
        SocketAddr::V4(_) => address,
    };
    unmap(address) == unmap(reply)
}

#[cfg(test)]
mod tests;
