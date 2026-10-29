//! Reaching a server over UDP after ssh starts the bridge. A fake ssh runs the
//! remote command on this computer; the UDP leg is real loopback traffic.

#[path = "support/ssh.rs"]
mod remote;
mod support;

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use muxy_client::{Client, ClientEvent, Start, Transport, UdpSettings};
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
        matches!(client.transport(), Transport::UdpFailed(reason) if reason.contains("no UDP port")),
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
