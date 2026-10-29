//! Measures a connection to a server on a bad link: how long a keystroke takes
//! to show up, how stale a program that redraws ten times a second looks, and
//! how long the screen takes to recover after the network was down.
//!
//! It runs against a server reached with `--host`, over ssh or UDP, and
//! prints one JSON object. See `scripts/transport-lab` for the lab that
//! shapes the network.
//!
//! ```text
//! transport_lab HOST [--udp] [--keys N] [--stale SECONDS] [--outage SECONDS]
//!               [--down CMD] [--up CMD]
//! ```
//!
//! `--down` and `--up` are shell commands that break and restore the network.

use std::io::{self, Write};
use std::process::Command;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use muxy_client::{Client, ClientEvent, RunGrid, SshTarget, Start, Transport, UdpSettings};
use muxy_protocol::{ChannelId, Size};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const SIZE: Size = Size {
    cols: 100,
    rows: 30,
};

fn main() -> Result {
    let mut arguments = std::env::args().skip(1);
    let host = arguments
        .next()
        .ok_or("usage: transport_lab HOST [options]")?;
    let (mut udp, mut keys, mut stale, mut outage) = (false, 40usize, 20u64, 10u64);
    let (mut down, mut up) = (None, None);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--udp" => udp = true,
            "--keys" => keys = arguments.next().ok_or("--keys N")?.parse()?,
            "--stale" => stale = arguments.next().ok_or("--stale SECONDS")?.parse()?,
            "--outage" => outage = arguments.next().ok_or("--outage SECONDS")?.parse()?,
            "--down" => down = arguments.next(),
            "--up" => up = arguments.next(),
            other => return Err(format!("unknown option {other}").into()),
        }
    }

    let mut target = SshTarget::new(&host)?;
    if udp {
        target = target.with_udp(UdpSettings::default());
    }
    let started = Instant::now();
    let client =
        Client::connect_ssh(&target, Start::IfNeeded)?.with_timeout(Duration::from_secs(30));
    let connect_ms = started.elapsed().as_secs_f64() * 1000.0;
    let transport = match client.transport() {
        Transport::Stream => "ssh".to_owned(),
        Transport::Udp => "udp".to_owned(),
        Transport::UdpFailed(reason) => format!("ssh (udp failed: {reason})"),
    };

    let session = client.create_session(std::path::Path::new("/tmp"), SIZE)?;
    let attachment = client.attach(session.id, SIZE)?;
    let channel = attachment.channel;
    let events = client.events().ok_or("no events")?;
    let mut screen = Screen {
        grid: attachment.grid,
        channel,
    };

    let mut report = serde_json::json!({
        "transport": transport,
        "connect_ms": connect_ms,
    });
    screen.settle(&client, &events, Duration::from_secs(8));
    client.send_input(channel, b"\x15clear\r")?;
    screen.settle(&client, &events, Duration::from_secs(4));

    report["echo"] = echo(&client, &events, &mut screen, keys)?;
    if stale > 0 {
        let (staleness, recovery) = staleness(
            &client,
            &events,
            &mut screen,
            stale,
            outage,
            down.as_deref(),
            up.as_deref(),
        )?;
        report["staleness_ms"] = staleness;
        if let Some(recovery) = recovery {
            report["recovery_ms"] = recovery;
        }
        client.send_input(channel, b"\x03")?;
        screen.settle(&client, &events, Duration::from_secs(3));
        client.send_input(channel, b"\x15clear\r")?;
        screen.settle(&client, &events, Duration::from_secs(3));
        report["echo_after"] = echo(&client, &events, &mut screen, 10)?;
    }
    writeln!(io::stdout(), "{report}")?;
    client.disconnect();
    Ok(())
}

struct Screen {
    grid: RunGrid,
    channel: ChannelId,
}

impl Screen {
    /// Applies what arrives for `duration`.
    fn settle(
        &mut self,
        client: &Client,
        events: &std::sync::mpsc::Receiver<ClientEvent>,
        duration: Duration,
    ) {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            self.pump(
                client,
                events,
                end.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(20)),
            );
        }
    }

    /// Applies one event, waiting up to `wait` for it. False when the
    /// connection ended.
    fn pump(
        &mut self,
        client: &Client,
        events: &std::sync::mpsc::Receiver<ClientEvent>,
        wait: Duration,
    ) -> bool {
        match events.recv_timeout(wait) {
            Ok(ClientEvent::Frame { channel, frame }) if channel == self.channel => {
                self.grid.apply(&frame);
                let _ = client.ack(channel, frame.seq);
                true
            }
            Ok(ClientEvent::Disconnected) | Err(RecvTimeoutError::Disconnected) => false,
            _ => true,
        }
    }

    fn contains(&self, text: &str) -> bool {
        (0..usize::from(SIZE.rows)).any(|row| self.grid.row_text(row).contains(text))
    }
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = ((sorted.len() as f64) * fraction) as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn summary(mut values: Vec<f64>, misses: usize) -> serde_json::Value {
    values.sort_by(f64::total_cmp);
    serde_json::json!({
        "n": values.len(),
        "missed": misses,
        "p50": percentile(&values, 0.5),
        "p90": percentile(&values, 0.9),
        "p99": percentile(&values, 0.99),
        "max": values.last().copied().unwrap_or(f64::NAN),
    })
}

/// Types characters, one at a time, and times each one's appearance.
fn echo(
    client: &Client,
    events: &std::sync::mpsc::Receiver<ClientEvent>,
    screen: &mut Screen,
    keys: usize,
) -> Result<serde_json::Value> {
    let mut typed = String::from("zq");
    client.send_input(screen.channel, b"zq")?;
    screen.settle(client, events, Duration::from_secs(3));
    let (mut latencies, mut misses) = (Vec::new(), 0);
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..keys {
        if typed.len() > 40 {
            client.send_input(screen.channel, b"\x15zq")?;
            typed = "zq".into();
            screen.settle(client, events, Duration::from_secs(3));
        }
        let character = char::from(b'a' + u8::try_from(next() % 16)?);
        let wanted = format!("{typed}{character}");
        let sent = Instant::now();
        client.send_input(screen.channel, character.to_string().as_bytes())?;
        let mut shown = false;
        while sent.elapsed() < Duration::from_secs(20) {
            screen.pump(client, events, Duration::from_millis(5));
            if screen.contains(&wanted) {
                shown = true;
                break;
            }
        }
        typed = wanted;
        if shown {
            latencies.push(sent.elapsed().as_secs_f64() * 1000.0);
        } else {
            misses += 1;
        }
        let pause = Duration::from_millis(250 + next() % 150);
        screen.settle(client, events, pause);
    }
    Ok(summary(latencies, misses))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(0))
}

/// The wall-clock time a program printed on the first row, if any.
fn shown_time(screen: &Screen) -> Option<u64> {
    let row = screen.grid.row_text(0);
    let digits: String = row.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 13)
        .then(|| digits[..13].parse().ok())
        .flatten()
}

fn run(command: &str) {
    let _ = Command::new("sh").args(["-c", command]).status();
}

/// Runs a program that prints the time ten times a second, samples how old
/// the displayed time is, and optionally breaks the network for a while.
fn staleness(
    client: &Client,
    events: &std::sync::mpsc::Receiver<ClientEvent>,
    screen: &mut Screen,
    seconds: u64,
    outage: u64,
    down: Option<&str>,
    up: Option<&str>,
) -> Result<(serde_json::Value, Option<serde_json::Value>)> {
    client.send_input(screen.channel, b"\x15clear\r")?;
    screen.settle(client, events, Duration::from_secs(3));
    let program = "while :; do printf '\\033[1;1H%s' $(date +%s%3N); sleep 0.1; done\r";
    client.send_input(screen.channel, program.as_bytes())?;
    screen.settle(client, events, Duration::from_secs(6));
    let mut ages = Vec::new();
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        screen.pump(client, events, Duration::from_millis(25));
        if let Some(shown) = shown_time(screen) {
            ages.push(now_ms().saturating_sub(shown) as f64);
        }
    }
    let mut recovery = None;
    if let (Some(down), Some(up)) = (down, up) {
        run(down);
        screen.settle(client, events, Duration::from_secs(outage));
        let restored = now_ms();
        run(up);
        let restored_at = Instant::now();
        let limit = restored_at + Duration::from_secs(60);
        while Instant::now() < limit {
            screen.pump(client, events, Duration::from_millis(5));
            if shown_time(screen).is_some_and(|shown| shown >= restored) {
                recovery = Some(restored_at.elapsed().as_secs_f64() * 1000.0);
                break;
            }
        }
        // Let the stream settle again before the next measurement.
        screen.settle(client, events, Duration::from_secs(5));
    }
    Ok((
        summary(ages, 0),
        recovery.map(|milliseconds| serde_json::json!(milliseconds)),
    ))
}
