//! Exercises the screen of the phone core against a real server and a real desktop: it pairs as
//! a phone through the facade over TLS, connects again with UDP when the server offers it, gets
//! the screen permission from the computer (a local connection to the server), watches the
//! desktop change colour and drives it with the pointer and the keyboard.
//!
//! ```text
//! phone_probe --server SOCKET [--credential FILE] [--expect-udp] COMMAND
//!   pair                Turn mobile access on, pair as a phone and keep the credential in FILE.
//!   refused             Check that a phone without the permission is refused, with the server's reason.
//!   allow | deny        Give the phone the screen permission, or take it away.
//!   colors TRANSPORT H264
//!                       Open the screen over `tls` or `auto` (UDP when the server offers it),
//!                       with H.264 offered (`on`) or not (`off`), and wait until the picture
//!                       follows the desktop's background (`swaymsg`). H.264 units that arrive
//!                       are checked and, where `ffmpeg` is there, decoded as well.
//!   input TRANSPORT     Move the pointer, click, type "x" and "Y" and turn the wheel.
//!   permission          Take the permission away while the screen is open: the screen closes at
//!                       once with the server's reason and opening is refused; then give it back.
//! ```
//!
//! It prints one JSON object per command. The CI job `screen-e2e` runs it under a headless sway
//! with `wayvnc`, and checks with `wev` what `input` did to the desktop.

use std::fs;
use std::io::{self, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use atur_mobile::{Direct, DirectListener, ScreenSink, direct_pair};
use atur_screen::decode::{FrameSink, H264Decoder, new_h264};
use atur_screen::{Frame, Pixels};
use muxy_client::Client;
use muxy_protocol::{DeviceId, ListenerStatus, RemoteAccessSettings};
use serde_json::{Value, json};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const WAIT: Duration = Duration::from_secs(25);
const RED: [u8; 3] = [200, 30, 30];
const BLUE: [u8; 3] = [30, 30, 200];

struct Options {
    server: PathBuf,
    credential: Option<PathBuf>,
    expect_udp: bool,
    command: String,
    rest: Vec<String>,
}

fn options() -> Result<Options> {
    let mut server = None;
    let mut credential = None;
    let mut expect_udp = false;
    let mut command = None;
    let mut rest = Vec::new();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match (argument.as_str(), command.is_some()) {
            ("--server", false) => server = arguments.next().map(PathBuf::from),
            ("--credential", false) => credential = arguments.next().map(PathBuf::from),
            ("--expect-udp", false) => expect_udp = true,
            (_, false) => command = Some(argument),
            (_, true) => rest.push(argument),
        }
    }
    Ok(Options {
        server: server.ok_or("usage: phone_probe --server SOCKET [--credential FILE] COMMAND")?,
        credential,
        expect_udp,
        command: command.ok_or("a command is required")?,
        rest,
    })
}

fn main() -> Result {
    let options = options()?;
    let report = match options.command.as_str() {
        "pair" => pair(&options)?,
        "refused" => refused(&options)?,
        "allow" => grant(&options, true)?,
        "deny" => grant(&options, false)?,
        "colors" => colors(&options)?,
        "input" => input(&options)?,
        "permission" => permission(&options)?,
        other => return Err(format!("unknown command {other}").into()),
    };
    writeln!(io::stdout(), "{report}")?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The computer's side: pairing and the permission

fn local(options: &Options) -> Result<Client> {
    Ok(Client::connect(&options.server)?)
}

fn pair(options: &Options) -> Result<Value> {
    let file = options
        .credential
        .as_ref()
        .ok_or("--credential is required")?;
    let local = local(options)?;
    let state = local.read_remote_access()?;
    if !(state.settings.enabled && state.status == ListenerStatus::Listening) {
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let state = local.write_remote_access(RemoteAccessSettings {
            enabled: true,
            port,
        })?;
        if state.status != ListenerStatus::Listening {
            return Err(format!("mobile access never listened: {:?}", state.status).into());
        }
    }
    let mut offer = local.start_pairing()?;
    offer.invite.hosts = vec!["127.0.0.1".into()];
    let credential = ok(&direct_pair(offer.invite.to_link(), "Phone probe".into()))?;
    fs::write(file, credential.to_string())?;
    Ok(json!({ "command": "pair", "device": credential["deviceId"] }))
}

fn credential(options: &Options) -> Result<Value> {
    let file = options
        .credential
        .as_ref()
        .ok_or("--credential is required")?;
    Ok(serde_json::from_slice(&fs::read(file)?)?)
}

fn device(options: &Options) -> Result<DeviceId> {
    let credential = credential(options)?;
    let id = credential["deviceId"]
        .as_str()
        .ok_or("the credential has no device id")?;
    Ok(id
        .parse()
        .map_err(|_| "the credential's device id is not valid")?)
}

fn grant(options: &Options, allowed: bool) -> Result<Value> {
    local(options)?.set_device_screen(device(options)?, allowed)?;
    Ok(json!({ "command": if allowed { "allow" } else { "deny" } }))
}

// ---------------------------------------------------------------------------------------------
// The phone

/// What the sink received, and what the newest picture shows.
#[derive(Default)]
struct Seen {
    /// Colour at the centre of the newest picture, and its size.
    picture: Option<([u8; 3], (u32, u32))>,
    pictures: u64,
    units: u64,
    /// Whether the first unit starts with an Annex B start code and starts a context.
    first_unit_ok: Option<bool>,
    decode_error: Option<String>,
}

type SharedSeen = Arc<Mutex<Seen>>;

fn lock_seen(seen: &Mutex<Seen>) -> MutexGuard<'_, Seen> {
    seen.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The colour at the centre of a picture of `width` x `height` pixels in `pixels`.
fn centre(width: u32, height: u32, pixels: &[u8], bgra: bool) -> Option<[u8; 3]> {
    let (width, height) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    let at = ((height / 2) * width + width / 2) * 4;
    let pixel = pixels.get(at..at + 3)?;
    Some(if bgra {
        [pixel[2], pixel[1], pixel[0]]
    } else {
        [pixel[0], pixel[1], pixel[2]]
    })
}

fn store(target: &Mutex<Seen>, width: u32, height: u32, colour: Option<[u8; 3]>) {
    if let Some(colour) = colour {
        let mut seen = lock_seen(target);
        seen.picture = Some((colour, (width, height)));
        seen.pictures += 1;
    }
}

/// A picture the decoder made from video.
#[allow(
    irrefutable_let_patterns,
    reason = "macOS pictures can also be surfaces"
)]
fn decoded(target: &Mutex<Seen>, frame: &Frame) {
    if let Pixels::Bgra(bytes) = &frame.pixels {
        store(
            target,
            frame.width,
            frame.height,
            centre(frame.width, frame.height, bytes, true),
        );
    }
}

struct Sink {
    seen: SharedSeen,
    /// Decodes the video units where this machine can, to check what they show.
    decoder: Mutex<Option<Box<dyn H264Decoder>>>,
}

impl Sink {
    fn new(seen: &SharedSeen) -> Self {
        let target = Arc::clone(seen);
        let frames: FrameSink = Arc::new(move |frame| decoded(&target, &frame));
        Self {
            seen: Arc::clone(seen),
            decoder: Mutex::new(new_h264(frames)),
        }
    }
}

impl ScreenSink for Sink {
    fn on_video(&self, annex_b: Vec<u8>, reset: bool) {
        {
            let mut seen = lock_seen(&self.seen);
            seen.units += 1;
            if seen.first_unit_ok.is_none() {
                seen.first_unit_ok = Some(reset && annex_b.starts_with(&[0, 0, 0, 1]));
            }
        }
        let mut decoder = self.decoder.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(decoder) = decoder.as_mut()
            && let Err(error) = decoder.decode(&annex_b, reset)
        {
            lock_seen(&self.seen).decode_error = Some(error.to_string());
        }
    }

    fn on_frame(&self, width: u32, height: u32, rgba: Vec<u8>) {
        store(
            &self.seen,
            width,
            height,
            centre(width, height, &rgba, false),
        );
    }
}

struct Collector(Mutex<Sender<Value>>);

impl DirectListener for Collector {
    fn on_message(&self, json: String) {
        if let Ok(value) = serde_json::from_str(&json) {
            let _ = self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(value);
        }
    }
}

fn ok(answer: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(answer)?;
    match value.get("ok") {
        Some(ok) => Ok(ok.clone()),
        None => Err(format!("error answer: {answer}").into()),
    }
}

/// A connected facade.
struct Phone {
    direct: Arc<Direct>,
    events: Receiver<Value>,
    seen: SharedSeen,
    /// The connect answer.
    info: Value,
    /// Whether this machine decodes H.264 itself.
    decodes: bool,
}

impl Phone {
    /// Connects with the stored credential over `transport`: `tls`, or `auto` for UDP when the
    /// server offers it. The screen feature must be there.
    fn connect(options: &Options, transport: &str) -> Result<Self> {
        let mut credential = credential(options)?;
        credential["transport"] = json!(transport);
        let (sender, events) = mpsc::channel();
        let direct = Direct::new(Arc::new(Collector(Mutex::new(sender))));
        let info = ok(&direct.connect(credential.to_string()))?;
        if info["screen"] != json!(true) {
            return Err(format!("the server does not offer the screen: {info}").into());
        }
        if transport == "auto" && options.expect_udp && info["transport"] != "udp" {
            return Err(format!("UDP was expected: {}", info["udpNote"]).into());
        }
        let seen = SharedSeen::default();
        let sink = Sink::new(&seen);
        let decodes = sink
            .decoder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        direct.set_screen_sink(Some(Arc::new(sink)));
        Ok(Self {
            direct,
            events,
            seen,
            info,
            decodes,
        })
    }

    fn call(&self, method: &str, args: &Value) -> Result<Value> {
        ok(&self.direct.call(method.into(), args.to_string()))
    }

    fn fire(&self, method: &str, args: &Value) -> Result {
        ok(&self.direct.fire(method.into(), args.to_string())).map(drop)
    }

    /// The error code and reason `screenOpen` answers with.
    fn open_refusal(&self) -> Result<(String, String)> {
        let answer = self
            .direct
            .call("screenOpen".into(), json!({ "h264": false }).to_string());
        let answer: Value = serde_json::from_str(&answer)?;
        let error = answer
            .get("error")
            .ok_or_else(|| format!("screenOpen was not refused: {answer}"))?;
        Ok((
            error["code"].as_str().unwrap_or_default().to_owned(),
            error["reason"].as_str().unwrap_or_default().to_owned(),
        ))
    }

    fn event(&self, what: &str, mut done: impl FnMut(&Value) -> bool) -> Result<Value> {
        let deadline = Instant::now() + WAIT;
        loop {
            let event = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| format!("timed out waiting for {what}"))?;
            if done(&event) {
                return Ok(event);
            }
        }
    }

    /// Opens the screen and waits until the desktop's size is known; returns it.
    fn open(&self, h264: bool) -> Result<(i64, i64)> {
        self.call("screenOpen", &json!({ "viewOnly": false, "h264": h264 }))?;
        let ready = self.event("the screen to be ready", |event| {
            event["t"] == "screen" && (event["state"] == "ready" || event["state"] == "closed")
        })?;
        if ready["state"] == "closed" {
            return Err(format!("the screen closed at the start: {}", ready["reason"]).into());
        }
        Ok((
            ready["width"].as_i64().unwrap_or(0),
            ready["height"].as_i64().unwrap_or(0),
        ))
    }

    /// The codec in the newest statistics event, within a few seconds.
    fn codec(&self) -> String {
        let mut codec = String::new();
        let deadline = Instant::now() + Duration::from_secs(4);
        while let Ok(event) = self
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if event["t"] == "screenStats"
                && let Some(text) = event["codec"].as_str()
                && !text.is_empty()
            {
                text.clone_into(&mut codec);
                break;
            }
        }
        codec
    }

    fn centre(&self) -> Option<[u8; 3]> {
        lock_seen(&self.seen).picture.map(|(colour, _)| colour)
    }

    /// Waits until pictures or video units arrive.
    fn wait_media(&self) -> Result {
        let deadline = Instant::now() + WAIT;
        loop {
            {
                let seen = lock_seen(&self.seen);
                if seen.pictures > 0 || seen.units > 0 {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err("neither a picture nor a video unit arrived".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until the picture's centre has `wanted` colour; how long that took, in milliseconds.
    fn wait_colour(&self, wanted: [u8; 3]) -> Result<f64> {
        let started = Instant::now();
        loop {
            if self.centre().is_some_and(|colour| near(colour, wanted)) {
                return Ok(started.elapsed().as_secs_f64() * 1000.0);
            }
            if started.elapsed() >= WAIT {
                return Err(format!(
                    "the picture never showed {wanted:?}; it shows {:?}",
                    self.centre()
                )
                .into());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

fn near(seen: [u8; 3], wanted: [u8; 3]) -> bool {
    seen.iter()
        .zip(wanted)
        .all(|(seen, wanted)| seen.abs_diff(wanted) <= 12)
}

fn set_background(colour: [u8; 3]) -> Result {
    let hex = format!("#{:02x}{:02x}{:02x}", colour[0], colour[1], colour[2]);
    let output = Command::new("swaymsg")
        .args(["output", "*", "bg", &hex, "solid_color"])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "swaymsg failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

fn transport(options: &Options) -> Result<&str> {
    match options.rest.first().map(String::as_str) {
        Some(transport @ ("tls" | "auto")) => Ok(transport),
        _ => Err("the transport is `tls` or `auto`".into()),
    }
}

// ---------------------------------------------------------------------------------------------
// The commands

fn refused(options: &Options) -> Result<Value> {
    let phone = Phone::connect(options, "tls")?;
    let (code, reason) = phone.open_refusal()?;
    if code != "ScreenNotAllowed" || reason.trim().is_empty() {
        return Err(
            format!("expected ScreenNotAllowed with a reason, got {code}: {reason}").into(),
        );
    }
    Ok(json!({ "command": "refused", "code": code, "reason": reason }))
}

fn colors(options: &Options) -> Result<Value> {
    let transport = transport(options)?;
    let h264 = match options.rest.get(1).map(String::as_str) {
        Some("on") => true,
        Some("off") => false,
        _ => return Err("H.264 is `on` or `off`".into()),
    };
    let phone = Phone::connect(options, transport)?;
    let (width, height) = phone.open(h264)?;
    set_background(RED)?;
    phone.wait_media()?;
    let (units, first_unit_ok) = {
        let seen = lock_seen(&phone.seen);
        (seen.units, seen.first_unit_ok)
    };
    let mut report = json!({
        "command": "colors",
        "transport": phone.info["transport"],
        "udp_note": phone.info["udpNote"],
        "picture": format!("{width}x{height}"),
        "h264_offered": h264,
        "video_units": units,
    });
    if units > 0 {
        // The server sent H.264: the first unit must be usable as it is.
        if first_unit_ok != Some(true) {
            return Err("the first video unit is not Annex B or does not start a context".into());
        }
        if !phone.decodes {
            report["colors_checked"] = json!(false);
            report["codec"] = json!(phone.codec());
            phone.call("screenClose", &json!({}))?;
            return Ok(report);
        }
    }
    let red_ms = phone.wait_colour(RED)?;
    set_background(BLUE)?;
    let blue_ms = phone.wait_colour(BLUE)?;
    set_background(RED)?;
    let red_again_ms = phone.wait_colour(RED)?;
    let decode_error = lock_seen(&phone.seen).decode_error.clone();
    if let Some(error) = decode_error {
        return Err(format!("decoding failed: {error}").into());
    }
    report["colors_checked"] = json!(true);
    report["first_ms"] = json!(red_ms.round());
    report["to_blue_ms"] = json!(blue_ms.round());
    report["back_to_red_ms"] = json!(red_again_ms.round());
    report["codec"] = json!(phone.codec());
    phone.call("screenClose", &json!({}))?;
    Ok(report)
}

fn input(options: &Options) -> Result<Value> {
    let phone = Phone::connect(options, transport(options)?)?;
    let (width, height) = phone.open(false)?;
    let (x, y) = (width / 2, height / 2);
    let pause = |millis| thread::sleep(Duration::from_millis(millis));
    phone.fire(
        "screenPointer",
        &json!({ "x": x - 40, "y": y - 20, "buttons": 0 }),
    )?;
    pause(100);
    phone.fire("screenPointer", &json!({ "x": x, "y": y, "buttons": 0 }))?;
    pause(100);
    phone.fire("screenPointer", &json!({ "x": x, "y": y, "buttons": 1 }))?;
    pause(60);
    phone.fire("screenPointer", &json!({ "x": x, "y": y, "buttons": 0 }))?;
    pause(200);
    phone.fire("screenType", &json!({ "text": "x" }))?;
    pause(100);
    // A capital the way a soft keyboard sends it: Shift held around the letter.
    phone.fire("screenKey", &json!({ "keysym": 0xffe1, "down": true }))?;
    phone.fire("screenType", &json!({ "text": "Y" }))?;
    phone.fire("screenKey", &json!({ "keysym": 0xffe1, "down": false }))?;
    pause(100);
    phone.fire("screenScroll", &json!({ "x": x, "y": y, "dx": 0, "dy": 1 }))?;
    pause(400);
    phone.call("screenClose", &json!({}))?;
    Ok(json!({ "command": "input", "transport": phone.info["transport"] }))
}

fn permission(options: &Options) -> Result<Value> {
    let local = local(options)?;
    let device = device(options)?;
    let phone = Phone::connect(options, "tls")?;
    phone.open(false)?;
    local.set_device_screen(device, false)?;
    let closed = phone.event("the screen to close", |event| {
        event["t"] == "screen" && event["state"] == "closed"
    })?;
    let reason = closed["reason"].as_str().unwrap_or_default().to_owned();
    if reason.trim().is_empty() {
        return Err("the screen closed without the server's reason".into());
    }
    let (code, _) = phone.open_refusal()?;
    if code != "ScreenNotAllowed" {
        return Err(format!("opening after the permission was taken away gave {code}").into());
    }
    local.set_device_screen(device, true)?;
    phone.open(false)?;
    phone.call("screenClose", &json!({}))?;
    Ok(json!({ "command": "permission", "closed_with": reason }))
}
