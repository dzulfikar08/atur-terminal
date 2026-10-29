//! The faster path to a server on another computer: the same bridge protocol
//! over encrypted UDP, bootstrapped through ssh the way mosh does.
//!
//! The client asks the bridge for UDP (`atur-term stdio --udp`). After its
//! ready line the bridge prints one more line, `MUXY-UDP/1 {json}`, naming a
//! port and a one-time secret it generated, and waits on ssh and on the UDP
//! port at once. The client sends a first packet to that port at the host ssh
//! connected to. If an authentic answer comes within a few seconds, the
//! protocol moves to UDP and ssh is dropped. If not, the client carries on
//! over ssh exactly as before, and the bridge serves ssh as soon as bytes
//! arrive there. Whichever carries the first protocol bytes wins.

use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use muxy_protocol::transport::udp::{Options, Secret, UdpServer, UdpStatus};
use muxy_protocol::transport::{self, ByteStream};

use crate::ClientError;

const OFFER: &str = "MUXY-UDP/";
const OFFER_VERSION: &str = "1";
/// Longest offer line the client accepts.
const MAX_OFFER: usize = 1024;
const BUFFER: usize = 64 * 1024;
/// How long a closing bridge waits for its last bytes to be acknowledged.
const LINGER: Duration = Duration::from_secs(10);

/// How to use UDP, from the client's side of the bootstrap.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct UdpSettings {
    /// Ports the bridge may bind. Open them in the server's firewall, or reach
    /// the server through a network that needs none, such as Tailscale.
    pub ports: RangeInclusive<u16>,
    /// How long the bridge keeps a session whose client has gone quiet.
    pub server_idle: Duration,
    /// How long the client keeps trying before the connection counts as lost
    /// and the usual reconnect starts. Time the computer sleeps does not count.
    pub client_idle: Duration,
    /// How long a first UDP attempt may take before ssh carries on alone.
    pub connect_timeout: Duration,
    /// The host to send UDP to, when it is not the one ssh connects to: a
    /// Tailscale name for a server ssh reaches through a jump host, say.
    pub host: Option<String>,
}

impl Default for UdpSettings {
    fn default() -> Self {
        Self {
            ports: 60001..=61000,
            server_idle: Duration::from_secs(60 * 60),
            client_idle: Duration::from_secs(5 * 60),
            connect_timeout: Duration::from_secs(3),
            host: None,
        }
    }
}

impl UdpSettings {
    /// Settings from `ATUR_TERM_UDP_PORTS` (`LOW-HIGH`), `ATUR_TERM_UDP_IDLE`
    /// (seconds the server keeps a quiet session), `ATUR_TERM_UDP_TIMEOUT`
    /// (seconds before the client gives up on a silent server) and
    /// `ATUR_TERM_UDP_HOST`.
    #[must_use]
    pub fn from_environment(mut self) -> Self {
        if let Some(ports) = std::env::var("ATUR_TERM_UDP_PORTS")
            .ok()
            .and_then(|text| parse_ports(&text))
        {
            self.ports = ports;
        }
        if let Some(seconds) = seconds_from("ATUR_TERM_UDP_IDLE") {
            self.server_idle = seconds;
        }
        if let Some(seconds) = seconds_from("ATUR_TERM_UDP_TIMEOUT") {
            self.client_idle = seconds;
        }
        if let Some(host) = std::env::var("ATUR_TERM_UDP_HOST")
            .ok()
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty())
        {
            self.host = Some(host);
        }
        self
    }

    /// Whether `ATUR_TERM_UDP` asks for UDP.
    pub fn requested_by_environment() -> bool {
        std::env::var("ATUR_TERM_UDP").is_ok_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "on" | "true" | "yes" | "udp" | "auto"
            )
        })
    }
}

fn seconds_from(variable: &str) -> Option<Duration> {
    let seconds: u64 = std::env::var(variable).ok()?.trim().parse().ok()?;
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

/// `LOW-HIGH`, or one port.
pub fn parse_ports(text: &str) -> Option<RangeInclusive<u16>> {
    let (low, high) = if let Some((low, high)) = text.trim().split_once('-') {
        (low.trim().parse().ok()?, high.trim().parse().ok()?)
    } else {
        let port = text.trim().parse().ok()?;
        (port, port)
    };
    (low >= 1024 && low <= high).then_some(low..=high)
}

/// What the bridge on the server computer does for UDP (`stdio --udp`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UdpRelay {
    pub ports: RangeInclusive<u16>,
    pub idle: Duration,
}

impl Default for UdpRelay {
    fn default() -> Self {
        let defaults = UdpSettings::default().from_environment();
        Self {
            ports: defaults.ports,
            idle: defaults.server_idle,
        }
    }
}

impl From<&UdpSettings> for UdpRelay {
    fn from(settings: &UdpSettings) -> Self {
        Self {
            ports: settings.ports.clone(),
            idle: settings.server_idle,
        }
    }
}

/// A server's answer to a request for UDP.
#[derive(Debug)]
pub(crate) enum Offer {
    Available { port: u16, secret: Secret },
    Unavailable(String),
}

pub(crate) fn offer_line(port: u16, secret: &Secret) -> Vec<u8> {
    format!(
        "{OFFER}{OFFER_VERSION} {{\"port\":{port},\"key\":\"{}\"}}\n",
        secret.encode()
    )
    .into_bytes()
}

fn unavailable_line(reason: &str) -> Vec<u8> {
    let reason: String = reason
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect();
    format!("{OFFER}0 {reason}\n").into_bytes()
}

/// Reads the offer line that follows the ready line. A bridge that does not
/// know UDP never prints one, so the caller bounds the wait.
pub(crate) fn read_offer(reader: &mut impl Read) -> io::Result<Offer> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > MAX_OFFER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the UDP offer is too long",
            ));
        }
    }
    Ok(parse_offer(&String::from_utf8_lossy(&line)))
}

fn parse_offer(line: &str) -> Offer {
    let malformed =
        || Offer::Unavailable("the server sent a UDP offer this build cannot read".into());
    let Some(rest) = line.trim().strip_prefix(OFFER) else {
        return malformed();
    };
    let (version, body) = rest.split_once(' ').unwrap_or((rest, ""));
    match version {
        "0" => Offer::Unavailable(body.trim().to_owned()),
        OFFER_VERSION => {
            let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(body) else {
                return malformed();
            };
            let port = fields
                .get("port")
                .and_then(serde_json::Value::as_u64)
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port != 0);
            let secret = fields
                .get("key")
                .and_then(serde_json::Value::as_str)
                .and_then(Secret::decode);
            match (port, secret) {
                (Some(port), Some(secret)) => Offer::Available { port, secret },
                _ => malformed(),
            }
        }
        _ => Offer::Unavailable(format!("the server speaks UDP offer version {version}")),
    }
}

/// Connects to an offer at the host's addresses.
pub(crate) fn connect(
    addresses: &[IpAddr],
    port: u16,
    secret: &Secret,
    settings: &UdpSettings,
) -> io::Result<Box<dyn ByteStream>> {
    let targets: Vec<SocketAddr> = addresses
        .iter()
        .map(|address| SocketAddr::new(*address, port))
        .collect();
    let options = Options {
        idle_timeout: settings.client_idle,
        ..Options::CLIENT
    };
    transport::udp::connect(&targets, secret, settings.connect_timeout, options)
        .map(|stream| Box::new(stream) as Box<dyn ByteStream>)
}

/// What a bridge learns first about who will use it.
enum First {
    /// ssh delivered the first bytes, so the client chose ssh.
    Ssh(Vec<u8>, File),
    /// ssh closed.
    SshEnded,
    /// UDP delivered the first bytes.
    Wire(Vec<u8>, Box<dyn Read + Send>),
    WireEnded,
}

/// Runs the bridge with UDP on offer. `output` already carries the ready
/// line.
pub(crate) fn serve(
    server: Box<dyn ByteStream>,
    relay: &UdpRelay,
    input: File,
    mut output: File,
) -> Result<(), ClientError> {
    let secret = Secret::generate()?;
    let options = Options {
        idle_timeout: relay.idle,
        ..Options::SERVER
    };
    let udp = match UdpServer::bind(relay.ports.clone(), &secret, options) {
        Ok(udp) => udp,
        Err(error) => {
            output.write_all(&unavailable_line(&format!(
                "no UDP port available in {}-{}: {error}",
                relay.ports.start(),
                relay.ports.end()
            )))?;
            transport::relay(server, input, output)?;
            return Ok(());
        }
    };
    output.write_all(&offer_line(udp.port(), &secret))?;
    let status = udp.status();
    let stream = udp.into_stream();
    let cancellation = stream.cancellation()?;
    let (wire_reader, wire_writer) = Box::new(stream).split()?;

    let (events, first) = mpsc::channel();
    let ssh_events = events.clone();
    thread::Builder::new()
        .name("udp-relay-ssh".into())
        .spawn(move || {
            let mut input = input;
            let mut buffer = vec![0; BUFFER];
            let event = match read_some(&mut input, &mut buffer) {
                Some(bytes) => First::Ssh(bytes, input),
                None => First::SshEnded,
            };
            let _ = ssh_events.send(event);
        })?;
    thread::Builder::new()
        .name("udp-relay-wire".into())
        .spawn(move || {
            let mut reader = wire_reader;
            let mut buffer = vec![0; BUFFER];
            let event = match read_some(&mut reader, &mut buffer) {
                Some(bytes) => First::Wire(bytes, reader),
                None => First::WireEnded,
            };
            let _ = events.send(event);
        })?;

    loop {
        match first.recv() {
            Ok(First::Ssh(bytes, input)) => {
                cancellation.cancel();
                transport::relay(server, Cursor::new(bytes).chain(input), output)?;
                return Ok(());
            }
            // ssh closes once UDP works, and the first UDP bytes may still be
            // on their way. Before any UDP contact it means the client left.
            Ok(First::SshEnded) if status.claimed() => {}
            Ok(First::SshEnded | First::WireEnded) | Err(_) => {
                cancellation.cancel();
                return Ok(());
            }
            Ok(First::Wire(bytes, reader)) => {
                drop(output);
                detach();
                return carry(server, &bytes, reader, wire_writer, &status, &*cancellation);
            }
        }
    }
}

/// The next bytes from `reader`, or `None` once it has ended or failed.
fn read_some(reader: &mut impl Read, buffer: &mut [u8]) -> Option<Vec<u8>> {
    loop {
        match reader.read(buffer) {
            Ok(0) => return None,
            Ok(read) => return Some(buffer[..read].to_vec()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// Leaves ssh's session so that its closing does not end the bridge, and
/// lets ssh finish now: nothing of ssh's stays open here.
fn detach() {
    let _ = rustix::process::setsid();
    if let Ok(null) = File::options().read(true).write(true).open("/dev/null") {
        let _ = rustix::stdio::dup2_stdin(&null);
        let _ = rustix::stdio::dup2_stdout(&null);
        let _ = rustix::stdio::dup2_stderr(&null);
    }
}

/// Joins the server and the UDP stream until either ends.
fn carry(
    server: Box<dyn ByteStream>,
    first: &[u8],
    wire_reader: Box<dyn Read + Send>,
    wire_writer: Box<dyn Write + Send>,
    status: &UdpStatus,
    cancellation: &dyn transport::StreamCancellation,
) -> Result<(), ClientError> {
    enum Ended {
        Server,
        Wire,
    }
    let server_cancellation = server.cancellation()?;
    let (server_reader, mut server_writer) = server.split()?;
    server_writer.write_all(first)?;
    let (ended, wait) = mpsc::channel();
    let down = ended.clone();
    thread::Builder::new()
        .name("udp-relay-down".into())
        .spawn(move || {
            copy(server_reader, wire_writer);
            let _ = down.send(Ended::Server);
        })?;
    thread::Builder::new()
        .name("udp-relay-up".into())
        .spawn(move || {
            copy(wire_reader, server_writer);
            let _ = ended.send(Ended::Wire);
        })?;
    if let Ok(Ended::Server) = wait.recv() {
        // The server closed; let the client read the rest first.
        status.wait_drained(LINGER);
    }
    server_cancellation.cancel();
    cancellation.cancel();
    Ok(())
}

/// Copies until either side ends. Dropping the writer ends that direction
/// for the reader at the other end.
fn copy(mut reader: impl Read, mut writer: impl Write) {
    let mut buffer = vec![0; BUFFER];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                if writer
                    .write_all(&buffer[..read])
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offer_round_trips() {
        let secret = Secret::generate().unwrap();
        let line = offer_line(60123, &secret);
        let mut reader = Cursor::new(line);
        match read_offer(&mut reader).unwrap() {
            Offer::Available {
                port,
                secret: parsed,
            } => {
                assert_eq!(port, 60123);
                assert_eq!(parsed, secret);
            }
            Offer::Unavailable(reason) => panic!("{reason}"),
        }
    }

    #[test]
    fn a_refusal_carries_its_reason_and_garbage_is_not_trusted() {
        let refused = read_offer(&mut Cursor::new(unavailable_line("no ports\u{7}"))).unwrap();
        assert!(matches!(refused, Offer::Unavailable(reason) if reason == "no ports"));
        for line in [
            "MUXY-UDP/1 {\"port\":0,\"key\":\"x\"}\n",
            "MUXY-UDP/1 {\"port\":70000,\"key\":\"x\"}\n",
            "MUXY-UDP/1 not json\n",
            "hello\n",
        ] {
            let parsed = read_offer(&mut Cursor::new(line.as_bytes().to_vec())).unwrap();
            assert!(matches!(parsed, Offer::Unavailable(_)), "{line}");
        }
        assert!(matches!(
            parse_offer("MUXY-UDP/9 {}"),
            Offer::Unavailable(reason) if reason.contains("version 9")
        ));
        let long = vec![b'a'; MAX_OFFER + 10];
        assert!(read_offer(&mut Cursor::new(long)).is_err());
        assert!(read_offer(&mut Cursor::new(b"MUXY-UDP/1".to_vec())).is_err());
    }

    #[test]
    fn port_ranges_parse() {
        assert_eq!(parse_ports("60001-61000"), Some(60001..=61000));
        assert_eq!(parse_ports(" 7000 "), Some(7000..=7000));
        assert_eq!(parse_ports("61000-60001"), None);
        assert_eq!(parse_ports("80"), None);
        assert_eq!(parse_ports("a-b"), None);
    }
}
