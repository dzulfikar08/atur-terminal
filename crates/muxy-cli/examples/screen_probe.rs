//! Exercises screen mirroring against a real desktop: through a server's screen
//! channel, or straight to a VNC socket, so the two can be compared.
//!
//! ```text
//! screen_probe (--server SOCKET | --direct VNC_SOCKET) COMMAND
//!   colors N       Flip the desktop's background between two colours N times with
//!                  `swaymsg`, and report how long each flip took to show in the frame.
//!   expect R G B   Wait until the centre of the picture has this colour.
//!   input          Move the pointer to the centre, click, and type "a" and "B".
//!   clipboard-out TEXT
//!                  Send TEXT as the local clipboard.
//!   clipboard-in TEXT
//!                  Wait until the remote clipboard becomes TEXT.
//! ```
//!
//! It prints one JSON object per command. The CI job `screen-e2e` runs it under a
//! headless sway with `wayvnc`.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use atur_screen::{
    Pixels, PointerButtons, Screen, ScreenOptions, ScreenState, ScreenTransport, keys,
};
use muxy_client::{Client, ScreenEvent, ScreenLink};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const WAIT: Duration = Duration::from_secs(20);

/// A screen channel on a server.
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

/// A VNC socket, with no relay in between.
struct Direct {
    reader: Mutex<UnixStream>,
    writer: Mutex<UnixStream>,
}

impl ScreenTransport for Direct {
    fn recv(&self) -> Option<Vec<u8>> {
        let mut buffer = vec![0; 16 * 1024];
        let count = self.reader.lock().ok()?.read(&mut buffer).ok()?;
        buffer.truncate(count);
        (count > 0).then_some(buffer)
    }

    fn send(&self, bytes: &[u8]) -> std::result::Result<(), String> {
        let mut writer = self.writer.lock().map_err(|error| error.to_string())?;
        writer.write_all(bytes).map_err(|error| error.to_string())
    }

    fn processed(&self, _bytes: usize) {}

    fn close(&self) {
        if let Ok(stream) = self.writer.lock() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn main() -> Result {
    let mut arguments = std::env::args().skip(1);
    let (via, path) = match (arguments.next().as_deref(), arguments.next()) {
        (Some("--server"), Some(path)) => ("relay", PathBuf::from(path)),
        (Some("--direct"), Some(path)) => ("direct", PathBuf::from(path)),
        _ => return Err("usage: screen_probe (--server SOCKET | --direct SOCKET) COMMAND".into()),
    };
    let command = arguments.next().ok_or("a command is required")?;
    let rest: Vec<String> = arguments.collect();
    let screen = if via == "relay" {
        let client = Client::connect(&path)?;
        let link = client.open_screen()?;
        // The link keeps its own handle to the connection.
        Screen::start(Channel(link), ScreenOptions::default(), || {})
    } else {
        let stream = UnixStream::connect(&path)?;
        Screen::start(
            Direct {
                reader: Mutex::new(stream.try_clone()?),
                writer: Mutex::new(stream),
            },
            ScreenOptions::default(),
            || {},
        )
    };
    let (width, height) = wait_ready(&screen)?;
    let report = match command.as_str() {
        "colors" => colors(
            via,
            &screen,
            width,
            height,
            rest.first().map_or(Ok(20), |n| n.parse())?,
        )?,
        "expect" => {
            let wanted = rgb(&rest)?;
            let started = Instant::now();
            wait_for_colour(&screen, width, height, wanted)?;
            format!(
                "{{\"via\":\"{via}\",\"command\":\"expect\",\"ms\":{}}}",
                started.elapsed().as_millis()
            )
        }
        "input" => {
            input(&screen, width, height);
            format!("{{\"via\":\"{via}\",\"command\":\"input\"}}")
        }
        "clipboard-out" => {
            screen.send_clipboard(rest.first().ok_or("text required")?);
            thread::sleep(Duration::from_secs(1));
            format!("{{\"via\":\"{via}\",\"command\":\"clipboard-out\"}}")
        }
        "clipboard-in" => {
            let wanted = rest.first().ok_or("text required")?;
            let deadline = Instant::now() + WAIT;
            loop {
                if screen.take_clipboard().as_deref() == Some(wanted.as_str()) {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(format!("the remote clipboard never became {wanted:?}").into());
                }
                thread::sleep(Duration::from_millis(50));
            }
            format!("{{\"via\":\"{via}\",\"command\":\"clipboard-in\"}}")
        }
        other => return Err(format!("unknown command {other}").into()),
    };
    writeln!(io::stdout(), "{report}")?;
    screen.close();
    Ok(())
}

fn rgb(arguments: &[String]) -> Result<[u8; 3]> {
    let mut parts = arguments.iter().map(|part| part.parse::<u8>());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(red), Some(green), Some(blue)) => Ok([red?, green?, blue?]),
        _ => Err("expect needs R G B".into()),
    }
}

fn wait_ready(screen: &Screen) -> Result<(u32, u32)> {
    let deadline = Instant::now() + WAIT;
    loop {
        match screen.state() {
            ScreenState::Ready(info) if screen.frame().is_some() => {
                return Ok((info.width, info.height));
            }
            ScreenState::Closed(reason) => {
                return Err(format!("the screen closed at the start: {reason}").into());
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err("no picture arrived".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// The colour at the centre of the newest picture, as red, green, blue.
#[allow(
    irrefutable_let_patterns,
    reason = "macOS pictures can also be surfaces"
)]
fn centre(screen: &Screen, width: u32, height: u32) -> Option<[u8; 3]> {
    let frame = screen.frame()?;
    let Pixels::Bgra(bytes) = &frame.pixels else {
        return None;
    };
    let (x, y) = (frame.width.min(width) / 2, frame.height.min(height) / 2);
    let at = ((y * frame.width + x) * 4) as usize;
    let pixel = bytes.get(at..at + 3)?;
    Some([pixel[2], pixel[1], pixel[0]])
}

fn near(seen: [u8; 3], wanted: [u8; 3]) -> bool {
    seen.iter()
        .zip(wanted)
        .all(|(seen, wanted)| seen.abs_diff(wanted) <= 12)
}

fn wait_for_colour(screen: &Screen, width: u32, height: u32, wanted: [u8; 3]) -> Result {
    let deadline = Instant::now() + WAIT;
    loop {
        if centre(screen, width, height).is_some_and(|seen| near(seen, wanted)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the picture never showed {wanted:?}; it shows {:?}",
                centre(screen, width, height)
            )
            .into());
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn set_background(colour: [u8; 3]) -> Result {
    let hex = format!("#{:02x}{:02x}{:02x}", colour[0], colour[1], colour[2]);
    let status = Command::new("swaymsg")
        .args(["output", "*", "bg", &hex, "solid_color"])
        .output()?;
    if status.status.success() {
        Ok(())
    } else {
        Err(format!(
            "swaymsg failed: {}",
            String::from_utf8_lossy(&status.stderr)
        )
        .into())
    }
}

fn colors(via: &str, screen: &Screen, width: u32, height: u32, flips: usize) -> Result<String> {
    let palette = [[200, 30, 30], [30, 30, 200], [30, 200, 30]];
    set_background(palette[2])?;
    wait_for_colour(screen, width, height, palette[2])?;
    let mut times = Vec::new();
    for flip in 0..flips {
        let colour = palette[flip % 2];
        // Let the encoder go idle, as it would between a user's actions.
        thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        set_background(colour)?;
        wait_for_colour(screen, width, height, colour)?;
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    let at = |fraction: f64| times[((times.len() - 1) as f64 * fraction).round() as usize];
    let stats = screen.stats();
    Ok(format!(
        "{{\"via\":\"{via}\",\"command\":\"colors\",\"flips\":{flips},\"codec\":\"{}\",\
         \"p50_ms\":{:.1},\"p90_ms\":{:.1},\"max_ms\":{:.1},\"picture\":\"{width}x{height}\"}}",
        stats.codec,
        at(0.5),
        at(0.9),
        at(1.0),
    ))
}

fn input(screen: &Screen, width: u32, height: u32) {
    let (x, y) = (
        i32::try_from(width / 2).unwrap_or(0),
        i32::try_from(height / 2).unwrap_or(0),
    );
    let none = PointerButtons::default();
    let left = PointerButtons { left: true, ..none };
    screen.pointer(x - 40, y - 20, none);
    thread::sleep(Duration::from_millis(100));
    screen.pointer(x, y, none);
    thread::sleep(Duration::from_millis(100));
    screen.pointer(x, y, left);
    thread::sleep(Duration::from_millis(60));
    screen.pointer(x, y, none);
    thread::sleep(Duration::from_millis(200));
    for (key, shifted) in [("a", false), ("b", true)] {
        let keysym = keys::keysym(key, Some(if shifted { "B" } else { key }), shifted);
        if shifted {
            screen.key(keys::SHIFT_L, true);
        }
        if let Some(keysym) = keysym {
            screen.key(keysym, true);
            thread::sleep(Duration::from_millis(40));
            screen.key(keysym, false);
        }
        if shifted {
            screen.key(keys::SHIFT_L, false);
        }
        thread::sleep(Duration::from_millis(100));
    }
    screen.scroll(x, y, 0, 1);
    thread::sleep(Duration::from_millis(300));
}
