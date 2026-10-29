//! Choosing among the addresses of one paired server.
//!
//! A computer is usually reachable several ways at once: its address on the
//! local network, its Tailscale address, a name. They differ a lot in speed,
//! and the order they were saved in is no guide, so every connect tries them
//! the way browsers do (Happy Eyeballs): the likely fastest first, the next
//! after a short stagger or as soon as the previous one failed, and the first
//! connection that finishes signing in is kept. The others are closed. Every
//! attempt trusts only the pinned certificate, as a single one always did.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use muxy_protocol::transport::{ByteStream, tls};

use crate::ClientError;

/// How long an attempt gets to run alone before the next address also starts.
pub const STAGGER: Duration = Duration::from_millis(150);

/// What an address is, which says how fast it is likely to be.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RouteKind {
    /// The local network: private and link-local addresses, `.local` names, loopback.
    Local,
    /// Tailscale: 100.64.0.0/10, its IPv6 range and `*.ts.net` names.
    Tailscale,
    /// Anything else, such as a public address or a DNS name.
    Other,
}

impl RouteKind {
    /// Classifies a host as an address or a name.
    pub fn of(host: &str) -> Self {
        let host = host.trim().trim_end_matches('.');
        let literal = host
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(host);
        match literal.parse::<IpAddr>() {
            Ok(IpAddr::V4(address)) => Self::of_v4(address),
            Ok(IpAddr::V6(address)) => Self::of_v6(address),
            Err(_) => Self::of_name(host),
        }
    }

    /// `local`, `tailscale` or `other`: how the facade names it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Tailscale => "tailscale",
            Self::Other => "other",
        }
    }

    fn of_v4(address: Ipv4Addr) -> Self {
        let [first, second, ..] = address.octets();
        if first == 100 && (64..128).contains(&second) {
            Self::Tailscale
        } else if address.is_private() || address.is_link_local() || address.is_loopback() {
            Self::Local
        } else {
            Self::Other
        }
    }

    fn of_v6(address: Ipv6Addr) -> Self {
        let segments = address.segments();
        if segments[..3] == [0xfd7a, 0x115c, 0xa1e0] {
            Self::Tailscale
        } else if address.is_loopback()
            || segments[0] & 0xffc0 == 0xfe80
            || segments[0] & 0xfe00 == 0xfc00
        {
            Self::Local
        } else {
            Self::Other
        }
    }

    fn of_name(host: &str) -> Self {
        let name = host.as_bytes();
        if name.eq_ignore_ascii_case(b"localhost") || has_suffix(name, b".local") {
            Self::Local
        } else if has_suffix(name, b".ts.net") {
            Self::Tailscale
        } else {
            Self::Other
        }
    }
}

/// Whether `name` ends with `suffix`, ignoring ASCII case.
fn has_suffix(name: &[u8], suffix: &[u8]) -> bool {
    name.len() >= suffix.len() && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// The address a connection was made through, and what it cost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Route {
    pub host: String,
    pub kind: RouteKind,
    /// From the start of this address's own attempt (not of the race) to the end
    /// of the sign-in: connect, TLS handshake, Hello and authentication.
    pub elapsed: Duration,
}

/// One address of a race and when it starts, counted from the start of the race.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attempt {
    /// Position of the host in the list given.
    pub index: usize,
    pub kind: RouteKind,
    pub start: Duration,
}

/// The order the addresses are tried in and when each one starts if the ones
/// before it have not finished: local first, then Tailscale, then the rest, each
/// group in the order it was given, one `stagger` apart. An address that
/// appears twice (names ignore case) is tried once.
pub fn plan(hosts: &[String], stagger: Duration) -> Vec<Attempt> {
    let mut ranked: Vec<(RouteKind, usize)> = Vec::with_capacity(hosts.len());
    for (index, host) in hosts.iter().enumerate() {
        if !hosts[..index]
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(host))
        {
            ranked.push((RouteKind::of(host), index));
        }
    }
    ranked.sort_by_key(|(kind, _)| *kind);
    ranked
        .into_iter()
        .enumerate()
        .map(|(place, (kind, index))| Attempt {
            index,
            kind,
            start: stagger.saturating_mul(u32::try_from(place).unwrap_or(u32::MAX)),
        })
        .collect()
}

/// Why an attempt did not produce a connection.
enum Failure {
    /// The address was not reached, or presented another certificate.
    Connect(io::Error),
    /// The address answered but the sign-in did not complete.
    Finish(ClientError),
}

/// What the race needs to know about the pinned server.
#[derive(Clone)]
pub(crate) struct Target {
    pub(crate) hosts: Vec<String>,
    pub(crate) port: u16,
    pub(crate) fingerprint: [u8; 32],
    pub(crate) timeout: Duration,
}

/// Connects to `host` and finishes with `finish`, timing the whole.
fn attempt<T>(
    target: &Target,
    host: &str,
    finish: &dyn Fn(Box<dyn ByteStream>) -> Result<T, ClientError>,
) -> Result<(T, Duration), Failure> {
    let started = Instant::now();
    let stream = tls::connect(host, target.port, target.fingerprint, target.timeout)
        .map_err(Failure::Connect)?;
    let value = finish(stream).map_err(Failure::Finish)?;
    Ok((value, started.elapsed()))
}

fn route(target: &Target, index: usize, elapsed: Duration) -> Route {
    let host = target.hosts[index].clone();
    Route {
        kind: RouteKind::of(&host),
        host,
        elapsed,
    }
}

/// Connects to the first address that completes `finish`, a sign-in or any
/// other step that proves the address leads to the server. One address is
/// tried on this thread; several are raced as [`plan`] says, and whatever the
/// losers produce is dropped, which closes it.
pub(crate) fn race<T, F>(
    target: &Target,
    stagger: Duration,
    finish: F,
) -> Result<(T, Route), ClientError>
where
    T: Send + 'static,
    F: Fn(Box<dyn ByteStream>) -> Result<T, ClientError> + Send + Sync + 'static,
{
    let attempts = plan(&target.hosts, stagger);
    let mut failures = Failures::default();
    match attempts.as_slice() {
        [] => {}
        [only] => {
            return match attempt(target, &target.hosts[only.index], &finish) {
                Ok((value, elapsed)) => Ok((value, route(target, only.index, elapsed))),
                Err(failure) => {
                    failures.add(failure);
                    Err(failures.finish())
                }
            };
        }
        _ => {
            let finish = Arc::new(finish);
            let target = Arc::new(target.clone());
            return run(&target, &attempts, &finish, &mut failures);
        }
    }
    Err(failures.finish())
}

/// The error to report when no address worked: a failed sign-in says more than
/// a certificate that did not match, which says more than an unreachable address.
#[derive(Default)]
struct Failures {
    finish: Option<ClientError>,
    mismatch: bool,
    connect: Option<io::Error>,
}

impl Failures {
    fn add(&mut self, failure: Failure) {
        match failure {
            Failure::Finish(error) => {
                self.finish.get_or_insert(error);
            }
            Failure::Connect(error) => {
                self.mismatch |= tls::is_identity_mismatch(&error);
                self.connect = Some(error);
            }
        }
    }

    fn finish(self) -> ClientError {
        if let Some(error) = self.finish {
            error
        } else if self.mismatch {
            ClientError::IdentityMismatch
        } else {
            ClientError::Io(self.connect.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "the server has no known address")
            }))
        }
    }
}

type Finished<T> = (usize, Result<(T, Duration), Failure>);

fn run<T, F>(
    target: &Arc<Target>,
    plan: &[Attempt],
    finish: &Arc<F>,
    failures: &mut Failures,
) -> Result<(T, Route), ClientError>
where
    T: Send + 'static,
    F: Fn(Box<dyn ByteStream>) -> Result<T, ClientError> + Send + Sync + 'static,
{
    let (sender, finished) = mpsc::channel::<Finished<T>>();
    let mut sender = Some(sender);
    let over = Arc::new(AtomicBool::new(false));
    let begin = Instant::now();
    let mut launched = 0;
    let mut running = 0_usize;
    loop {
        let due = plan.get(launched).map(|next| begin + next.start);
        if let (Some(due), Some(outbox)) = (due, sender.as_ref())
            && (running == 0 || Instant::now() >= due)
        {
            let index = plan[launched].index;
            launched += 1;
            match spawn_attempt(target, index, finish, &over, outbox) {
                Ok(()) => running += 1,
                Err(error) => failures.add(Failure::Connect(error)),
            }
            if launched == plan.len() {
                sender = None;
            }
            continue;
        }
        if running == 0 {
            break;
        }
        let received = match due {
            Some(due) => finished.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => finished
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match received {
            Ok((index, Ok((value, elapsed)))) => {
                over.store(true, Ordering::Release);
                return Ok((value, route(target, index, elapsed)));
            }
            Ok((_, Err(failure))) => {
                running -= 1;
                failures.add(failure);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    over.store(true, Ordering::Release);
    Err(std::mem::take(failures).finish())
}

fn spawn_attempt<T, F>(
    target: &Arc<Target>,
    index: usize,
    finish: &Arc<F>,
    over: &Arc<AtomicBool>,
    results: &mpsc::Sender<Finished<T>>,
) -> io::Result<()>
where
    T: Send + 'static,
    F: Fn(Box<dyn ByteStream>) -> Result<T, ClientError> + Send + Sync + 'static,
{
    let target = Arc::clone(target);
    let finish = Arc::clone(finish);
    let over = Arc::clone(over);
    let results = results.clone();
    thread::Builder::new()
        .name("atur-route".into())
        .spawn(move || {
            if over.load(Ordering::Acquire) {
                return;
            }
            let outcome = attempt(&target, &target.hosts[index], finish.as_ref());
            // After the race is decided nobody listens: a late connection is dropped here.
            let _ = results.send((index, outcome));
        })
        .map(|_handle| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn hosts_are_classified_by_what_they_are() {
        for local in [
            "192.168.1.5",
            "10.184.172.54",
            "172.16.0.9",
            "172.31.255.1",
            "169.254.3.4",
            "127.0.0.2",
            "fe80::1",
            "fd12:3456::1",
            "[fe80::1]",
            "laptop.local",
            "Laptop.LOCAL.",
            "localhost",
        ] {
            assert_eq!(RouteKind::of(local), RouteKind::Local, "{local}");
        }
        for tailscale in [
            "100.64.0.1",
            "100.100.100.100",
            "100.127.255.254",
            "fd7a:115c:a1e0::1",
            "box.tail1234.ts.net",
            "BOX.TS.NET",
        ] {
            assert_eq!(
                RouteKind::of(tailscale),
                RouteKind::Tailscale,
                "{tailscale}"
            );
        }
        for other in [
            "203.0.113.7",
            "100.63.0.1",
            "100.128.0.1",
            "172.32.0.1",
            "2001:db8::1",
            "box.example.com",
            "",
        ] {
            assert_eq!(RouteKind::of(other), RouteKind::Other, "{other}");
        }
    }

    #[test]
    fn local_goes_first_then_tailscale_then_the_rest() {
        let list = hosts(&[
            "box.example.com",
            "100.101.102.103",
            "192.168.1.5",
            "203.0.113.7",
            "mac.local",
        ]);
        let plan = plan(&list, Duration::from_millis(150));
        let order: Vec<&str> = plan.iter().map(|a| list[a.index].as_str()).collect();
        assert_eq!(
            order,
            [
                "192.168.1.5",
                "mac.local",
                "100.101.102.103",
                "box.example.com",
                "203.0.113.7"
            ]
        );
        let starts: Vec<u128> = plan.iter().map(|a| a.start.as_millis()).collect();
        assert_eq!(starts, [0, 150, 300, 450, 600]);
        let kinds: Vec<RouteKind> = plan.iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            [
                RouteKind::Local,
                RouteKind::Local,
                RouteKind::Tailscale,
                RouteKind::Other,
                RouteKind::Other
            ]
        );
    }

    #[test]
    fn one_address_starts_at_once_and_nothing_is_a_plan_of_nothing() {
        let list = hosts(&["100.64.0.2"]);
        assert_eq!(
            plan(&list, STAGGER),
            [Attempt {
                index: 0,
                kind: RouteKind::Tailscale,
                start: Duration::ZERO
            }]
        );
        assert!(plan(&[], STAGGER).is_empty());
    }

    #[test]
    fn a_repeated_address_is_tried_once() {
        let list = hosts(&["Mac.local", "192.168.1.5", "mac.LOCAL", "192.168.1.5"]);
        let indexes: Vec<usize> = plan(&list, STAGGER).iter().map(|a| a.index).collect();
        assert_eq!(indexes, [0, 1]);
    }

    #[test]
    fn the_stagger_does_not_overflow() {
        let list = hosts(&["a.example", "b.example"]);
        let plan = plan(&list, Duration::MAX);
        assert_eq!(plan[0].start, Duration::ZERO);
        assert_eq!(plan[1].start, Duration::MAX);
    }
}
