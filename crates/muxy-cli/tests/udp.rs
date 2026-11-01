//! Reaching a server over UDP after ssh starts the bridge. A fake ssh runs the
//! remote command on this computer; the UDP leg is real loopback traffic.

#[path = "support/ssh.rs"]
mod remote;
mod support;

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use atur_screen::{Screen, ScreenOptions, ScreenState, ScreenTransport};
use muxy_client::{Client, ClientEvent, ScreenEvent, ScreenLink, Start, Transport, UdpSettings};
use muxy_protocol::transport::udp::{Options, Secret};
use muxy_protocol::{ChannelId, Size};
use remote::{FakeRemote, Result};

const TIMEOUT: Duration = Duration::from_secs(15);
const SIZE: Size = Size { cols: 80, rows: 24 };

fn settings(ports: std::ops::RangeInclusive<u16>, host: &str) -> UdpSettings {
    UdpSettings {
        ports,
        host: Some(host.into()),
        connect_timeout: Duration::from_millis(1500),
        ..UdpSettings::default()
    }
}

fn shows(
    events: &Receiver<ClientEvent>,
    client: &Client,
    channel: ChannelId,
    needle: &str,
) -> Result {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now()))? {
            ClientEvent::Frame {
                channel: framed,
                frame,
            } if framed == channel => {
                let text: String = frame
                    .rows
                    .iter()
                    .flat_map(|row| row.runs.iter().map(|run| run.text.as_str()))
                    .collect();
                if text.contains(needle) {
                    return Ok(());
                }
                client.ack(channel, frame.seq)?;
            }
            ClientEvent::Disconnected => return Err("disconnected".into()),
            _ => {}
        }
    }
}

fn carries_a_terminal(client: &Client, word: &str) -> Result {
    let session = client.create_session(std::path::Path::new("/tmp"), SIZE)?;
    let attachment = client.attach(session.id, SIZE)?;
    let events = client.events().ok_or("no events")?;
    client.send_input(
        attachment.channel,
        format!("echo {word}$((6*7))\r").as_bytes(),
    )?;
    shows(&events, client, attachment.channel, &format!("{word}42"))
}

#[test]
fn udp_carries_the_session_after_ssh_has_gone() -> Result {
    let remote = FakeRemote::detached()?;
    let target = remote
        .target()?
        .with_udp(settings(62_100..=62_199, "127.0.0.1"));
    let client = Client::connect_ssh(&target, Start::IfNeeded)?;
    assert_eq!(client.transport(), &Transport::Udp);
    client.ping()?;
    carries_a_terminal(&client, "udp")?;
    client.disconnect();
    Ok(())
}

#[test]
fn a_session_outlives_its_connection_and_a_new_one_reconnects() -> Result {
    let remote = FakeRemote::detached()?;
    let target = remote
        .target()?
        .with_udp(settings(62_200..=62_299, "127.0.0.1"));
    let first = Client::connect_ssh(&target, Start::IfNeeded)?;
    assert_eq!(first.transport(), &Transport::Udp);
    let session = first.create_session(std::path::Path::new("/tmp"), SIZE)?;
    first.disconnect();

    let second = Client::connect_ssh(&target, Start::Never)?;
    assert_eq!(second.transport(), &Transport::Udp);
    assert!(
        second
            .list_sessions()?
            .iter()
            .any(|listed| listed.id == session.id)
    );
    carries_a_terminal(&second, "again")?;
    Ok(())
}

#[test]
fn an_unreachable_port_falls_back_to_ssh_without_losing_the_connection() -> Result {
    let remote = FakeRemote::detached()?;
    // A documentation address nobody answers at.
    let target = remote
        .target()?
        .with_udp(settings(62_300..=62_399, "192.0.2.1"));
    let started = Instant::now();
    let client = Client::connect_ssh(&target, Start::IfNeeded)?;
    assert!(
        matches!(client.transport(), Transport::UdpFailed(reason) if reason.contains("no answer")),
        "{:?}",
        client.transport()
    );
    assert!(started.elapsed() < Duration::from_secs(12));
    client.ping()?;
    carries_a_terminal(&client, "ssh")?;
    Ok(())
}

#[test]
fn a_bridge_that_cannot_bind_says_so_and_ssh_carries_on() -> Result {
    let remote = FakeRemote::detached()?;
    let squatter = std::net::UdpSocket::bind(("0.0.0.0", 0))?;
    let port = squatter.local_addr()?.port();
    let target = remote
        .target()?
        .with_udp(settings(port..=port, "127.0.0.1"));
    let client = Client::connect_ssh(&target, Start::IfNeeded)?;
    assert!(
        matches!(
            client.transport(),
            Transport::UdpFailed(reason)
                if reason.contains("no UDP port") || reason.contains("no answer")
        ),
        "{:?}",
        client.transport()
    );
    client.ping()?;
    Ok(())
}

#[test]
fn the_bridge_answers_only_the_holder_of_its_secret() -> Result {
    let remote = FakeRemote::new()?;
    let mut bridge = remote
        .muxy(&["stdio", "--udp", "--udp-ports", "62400-62499"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut lines = BufReader::new(bridge.stdout.take().ok_or("stdout")?);
    let (mut ready, mut offer) = (String::new(), String::new());
    lines.read_line(&mut ready)?;
    lines.read_line(&mut offer)?;
    assert_eq!(ready.trim(), "ATUR-STDIO/1");
    let json: serde_json::Value =
        serde_json::from_str(offer.trim().trim_start_matches("ATUR-UDP/1 "))?;
    let port = u16::try_from(json["port"].as_u64().ok_or("port")?)?;
    assert!((62_400..=62_499).contains(&port));
    let secret = Secret::decode(json["key"].as_str().ok_or("key")?).ok_or("secret")?;

    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    // Garbage and a wrong secret get nothing.
    let probe = std::net::UdpSocket::bind(("127.0.0.1", 0))?;
    probe.set_read_timeout(Some(Duration::from_millis(400)))?;
    probe.send_to(&[7; 64], address)?;
    assert!(probe.recv_from(&mut [0; 64]).is_err(), "a reply to garbage");
    let wrong = muxy_protocol::transport::udp::connect(
        &[address],
        &Secret::from_bytes([3; 32]),
        Duration::from_millis(600),
        Options::CLIENT,
    );
    assert_eq!(
        wrong.err().map(|error| error.kind()),
        Some(std::io::ErrorKind::TimedOut)
    );

    // The right one still gets in afterwards, and the bridge starts serving it.
    let stream = muxy_protocol::transport::udp::connect(
        &[address],
        &secret,
        Duration::from_secs(3),
        Options::CLIENT,
    )?;
    let client = Client::from_stream(Box::new(stream))?;
    client.ping()?;
    client.disconnect();
    drop(bridge.stdin.take());
    let _ = bridge.kill();
    let _ = bridge.wait();
    Ok(())
}

/// A stand-in for wayvnc as strict as the real one about the handshake: it
/// reads exactly the version line, then exactly one security type byte, then
/// `ClientInit`, and writes a line to its log for everything the viewer sends.
/// A security type other than 1 is "Illegal security type", as in wayvnc.
const STRICT_WAYVNC: &str = r#"#!/usr/bin/env python3
import os, socket, struct, sys, threading, time

parent = os.getppid()
log = open(os.environ["FAKE_WAYVNC_LOG"], "a", buffering=1)
path = sys.argv[-1][len("unix:"):]
server = socket.socket(socket.AF_UNIX)
server.bind(path)
server.listen(4)

def exact(connection, count):
    data = b""
    while len(data) < count:
        chunk = connection.recv(count - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data

def serve(connection):
    try:
        connection.sendall(b"RFB 003.008\n")
        version = exact(connection, 12)
        log.write("version:%s\n" % version.decode().strip())
        connection.sendall(bytes([1, 1]))
        chosen = exact(connection, 1)[0]
        if chosen != 1:
            log.write("bad:security type %d\n" % chosen)
            return
        connection.sendall(struct.pack(">I", 0))
        exact(connection, 1)
        name = b"fake"
        pixel = bytes([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0])
        connection.sendall(struct.pack(">HH", 64, 48) + pixel + struct.pack(">I", len(name)) + name)
        log.write("init\n")
        sent = False
        while True:
            kind = exact(connection, 1)[0]
            if kind == 0:
                exact(connection, 19)
            elif kind == 2:
                count = struct.unpack(">H", exact(connection, 3)[1:])[0]
                exact(connection, 4 * count)
            elif kind == 3:
                exact(connection, 9)
                if not sent:
                    sent = True
                    rect = struct.pack(">HHHHi", 0, 0, 64, 48, 0)
                    connection.sendall(bytes([0, 0, 0, 1]) + rect + bytes([30, 30, 200, 0]) * (64 * 48))
            elif kind == 4:
                exact(connection, 7)
            elif kind == 5:
                exact(connection, 5)
            elif kind == 6:
                length = struct.unpack(">I", exact(connection, 7)[3:])[0]
                log.write("clip:%s\n" % exact(connection, length).decode("latin-1"))
            elif kind == 150:
                exact(connection, 9)
            else:
                log.write("bad:message type %d\n" % kind)
                return
    except EOFError:
        pass
    finally:
        connection.close()

def watch():
    while os.getppid() == parent:
        time.sleep(0.2)
    os._exit(0)

threading.Thread(target=watch, daemon=True).start()
while True:
    connection, _ = server.accept()
    threading.Thread(target=serve, args=(connection,), daemon=True).start()
"#;

struct Channel(ScreenLink);

impl ScreenTransport for Channel {
    fn recv(&self) -> Option<Vec<u8>> {
        match self.0.recv()? {
            ScreenEvent::Data(bytes) => Some(bytes),
            ScreenEvent::Closed(_) => None,
        }
    }

    fn send(&self, bytes: &[u8]) -> std::result::Result<(), String> {
        self.0.send(bytes).map_err(|error| error.to_string())
    }

    fn processed(&self, bytes: usize) {
        self.0.processed(bytes);
    }

    fn close(&self) {
        self.0.close();
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) -> Result {
    let deadline = Instant::now() + TIMEOUT;
    while !condition() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// The screen channel over UDP, with a viewer that has a clipboard to announce
/// and input to send from the first moment, as a Mac with text copied does.
#[test]
fn udp_carries_a_screen_handshake_with_input_ready_at_the_start() -> Result {
    use std::os::unix::fs::PermissionsExt;

    let remote = FakeRemote::detached_with_script(
        "export ATUR_TERM_WAYVNC=\"$ATUR_TERM_DIR/wayvnc\" \
         XDG_RUNTIME_DIR=\"$ATUR_TERM_DIR/run\" ATUR_TERM_SCREEN_GPU=0 \
         FAKE_WAYVNC_LOG=\"$ATUR_TERM_DIR/wayvnc.log\"",
    )?;
    let program = remote.profile().join("wayvnc");
    std::fs::write(&program, STRICT_WAYVNC)?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
    let run = remote.profile().join("run");
    std::fs::create_dir(&run)?;
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700))?;
    // A Wayland desktop is a socket named wayland-N in the runtime directory.
    let _desktop = std::os::unix::net::UnixListener::bind(run.join("wayland-7"))?;
    let log = remote.profile().join("wayvnc.log");

    let target = remote
        .target()?
        .with_udp(settings(62_500..=62_599, "127.0.0.1"));
    let client = Client::connect_ssh(&target, Start::IfNeeded)?;
    assert_eq!(client.transport(), &Transport::Udp);
    let link = client.open_screen()?;
    let screen = Screen::start(Channel(link), ScreenOptions::default(), || {});
    screen.send_clipboard("udp clipboard");
    screen.key(0x61, true);
    screen.key(0x61, false);

    wait_until("the screen to be ready with a picture", || {
        matches!(screen.state(), ScreenState::Ready(_)) && screen.frame().is_some()
    })?;
    wait_until("the clipboard to reach the capture", || {
        std::fs::read_to_string(&log).is_ok_and(|text| text.contains("clip:udp clipboard"))
    })?;
    let seen = std::fs::read_to_string(&log)?;
    assert!(seen.contains("version:RFB 003.008"), "{seen}");
    assert!(!seen.contains("bad:"), "{seen}");
    // The handshake finishes before the clipboard is announced.
    let init = seen.find("init").ok_or("no init")?;
    assert!(seen.find("clip:").ok_or("no clip")? > init, "{seen}");
    screen.close();
    client.disconnect();
    Ok(())
}
