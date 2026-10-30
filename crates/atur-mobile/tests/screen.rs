//! The computer's screen through the facade, against an in-process server over TLS.
//!
//! The server starts its capture program like it does for a user; here that program is a Python
//! stand-in for `wayvnc` (`fake_wayvnc.py`: an RFB 3.8 server on a unix socket that sends ZRLE
//! pictures or the H.264 fixture's access units, logs the input it receives and echoes clipboard
//! text), and the desktop is a socket named `wayland-7` in a private runtime directory. The server
//! reads where `wayvnc` is, and the runtime directory, from the environment once, and the capture
//! program is shared by every viewer, so everything runs in one test, in a fixed order.
#![cfg(target_os = "linux")]
#![allow(
    unsafe_code,
    reason = "the test points the in-process server at its stand-in for wayvnc through the environment"
)]

use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use atur_mobile::{Direct, DirectListener, ScreenSink, direct_pair};
use muxy_client::Client;
use muxy_protocol::transport::Listener;
use muxy_protocol::transport::tls::TlsListener;
use muxy_protocol::{DeviceId, ListenerStatus, RemoteAccessSettings};
use muxy_server::connection::{serve, serve_remote};
use muxy_server::{Registry, ServerEvent, ServerSettings};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const TIMEOUT: Duration = Duration::from_secs(20);

/// What `muxy-server` starts instead of `wayvnc`.
const FAKE_WAYVNC: &str = include_str!("fake_wayvnc.py");

/// The H.264 fixture of `atur-screen`: 320x180, 20 pictures.
const FIXTURE: &[u8] = include_bytes!("../../atur-screen/tests/fixtures/solid-cbp.h264");
const WIDTH: usize = 320;
const HEIGHT: usize = 180;

/// Splits Annex B data into NAL units without their start codes.
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut zeros = 0;
    for (index, &byte) in data.iter().enumerate() {
        if byte == 1 && zeros >= 2 {
            starts.push(index + 1);
            zeros = 0;
        } else if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    let mut units = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).map_or(data.len(), |next| next - 3);
        let mut unit = &data[start..end];
        while let [rest @ .., 0] = unit {
            unit = rest;
        }
        units.push(unit);
    }
    units
}

/// Groups NAL units into access units, one per RFB rectangle: a new unit starts at an
/// SEI/SPS/PPS/AUD that follows picture data, or at a slice with `first_mb_in_slice == 0`.
fn access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut units: Vec<Vec<u8>> = Vec::new();
    let mut has_picture = false;
    for nal in split_nals(data) {
        let kind = nal[0] & 0x1f;
        let is_slice = kind == 1 || kind == 5;
        let starts_unit = has_picture
            && ((6..=9).contains(&kind)
                || (is_slice && nal.get(1).is_some_and(|byte| byte & 0x80 != 0)));
        if starts_unit || units.is_empty() {
            units.push(Vec::new());
            has_picture = false;
        }
        if let Some(unit) = units.last_mut() {
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(nal);
        }
        has_picture |= is_slice;
    }
    units
}

/// The desktop the server mirrors: a runtime directory with a Wayland socket in it, the
/// stand-in program and the files that steer it. Sets the environment the server reads.
struct Desktop {
    directory: PathBuf,
    units: Vec<Vec<u8>>,
    _socket: UnixListener,
}

impl Desktop {
    fn new() -> TestResult<Self> {
        let directory =
            std::env::temp_dir().join(format!("atur-phone-screen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory)?;
        fs::create_dir(directory.join("run"))?;
        fs::set_permissions(directory.join("run"), fs::Permissions::from_mode(0o700))?;
        let socket = UnixListener::bind(directory.join("run/wayland-7"))?;
        let program = directory.join("wayvnc");
        fs::write(&program, FAKE_WAYVNC)?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;
        let units = access_units(FIXTURE);
        let mut packed = Vec::new();
        for unit in &units {
            packed.extend_from_slice(&u32::try_from(unit.len())?.to_be_bytes());
            packed.extend_from_slice(unit);
        }
        fs::write(directory.join("units.bin"), packed)?;
        // SAFETY: this is the only test of the binary and it runs this before it starts a
        // server, so no other thread reads or writes the environment yet.
        unsafe {
            std::env::set_var("ATUR_TERM_WAYVNC", &program);
            std::env::set_var("ATUR_TERM_SCREEN_GPU", "0");
            std::env::set_var("FAKE_WAYVNC_DIR", &directory);
            std::env::set_var("XDG_RUNTIME_DIR", directory.join("run"));
            std::env::remove_var("WAYLAND_DISPLAY");
        }
        let desktop = Self {
            directory,
            units,
            _socket: socket,
        };
        desktop.set_colour([200, 30, 60])?;
        desktop.set_size(WIDTH, HEIGHT)?;
        Ok(desktop)
    }

    /// Replaces a steering file in one step, so the program never reads half of it.
    fn write(&self, name: &str, text: &str) -> TestResult {
        let staging = self.directory.join(format!("{name}.new"));
        fs::write(&staging, text)?;
        fs::rename(staging, self.directory.join(name))?;
        Ok(())
    }

    fn set_colour(&self, [red, green, blue]: [u8; 3]) -> TestResult {
        self.write("colour", &format!("{red:02x}{green:02x}{blue:02x}"))
    }

    fn set_size(&self, width: usize, height: usize) -> TestResult {
        self.write("size", &format!("{width}x{height}"))
    }

    /// Makes the stand-in exit as it starts, like a `wayvnc` that cannot capture.
    fn set_failing(&self, failing: bool) -> TestResult {
        let marker = self.directory.join("fail");
        if failing {
            fs::write(marker, "")?;
        } else {
            let _ = fs::remove_file(marker);
        }
        Ok(())
    }

    /// What the stand-in program logged, a line each.
    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.directory.join("input.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Waits until the log satisfies `done`.
    fn wait_log(&self, what: &str, done: impl Fn(&[String]) -> bool) -> TestResult {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let log = self.log();
            if done(&log) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("the desktop never logged {what}; it logged {log:?}").into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until the log holds the lines `wanted`, in this order (others may lie between).
    fn wait_logged(&self, wanted: &[&str]) -> TestResult {
        self.wait_log(&format!("{wanted:?}"), |log| {
            let mut lines = log.iter();
            wanted.iter().all(|want| lines.any(|line| line == want))
        })
    }

    fn count(&self, line: &str) -> usize {
        self.log().iter().filter(|logged| *logged == line).count()
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

// ---------------------------------------------------------------------------------------------
// An in-process server whose network listener is a real TLS listener on loopback

type Subscribers = Arc<Mutex<Vec<Sender<ServerEvent>>>>;

struct Server {
    registry: Arc<Registry>,
    subscribers: Subscribers,
}

fn subscribe(subscribers: &Subscribers) -> Receiver<ServerEvent> {
    let (sender, events) = mpsc::channel();
    subscribers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(sender);
    events
}

impl Server {
    fn new() -> Self {
        let (sender, events) = mpsc::channel::<ServerEvent>();
        let subscribers = Subscribers::default();
        let sinks = Arc::clone(&subscribers);
        thread::spawn(move || {
            for event in events {
                sinks
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .retain(|sink| sink.send(event.clone()).is_ok());
            }
        });
        let slot = Arc::new(OnceLock::<Weak<Registry>>::new());
        let owner = Arc::clone(&slot);
        let remote_subscribers = Arc::clone(&subscribers);
        let running = Mutex::new(None::<Arc<TlsListener>>);
        let registry = Registry::new(
            ServerSettings {
                default_shell: Some(PathBuf::from("/bin/sh")),
                ..ServerSettings::default()
            },
            sender,
        )
        .with_address_source(|| vec!["127.0.0.1".to_owned()]);
        let registry = Arc::new(registry.with_remote_listener(move |listening| {
            let mut running = running.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(previous) = running.take() {
                previous.close();
            }
            let Some((port, identity)) = listening else {
                return ListenerStatus::Disabled;
            };
            match TlsListener::bind(SocketAddr::from(([127, 0, 0, 1], port)), &identity) {
                Ok(listener) => {
                    let listener = Arc::new(listener);
                    *running = Some(Arc::clone(&listener));
                    let registry = owner.get().cloned().unwrap_or_default();
                    let subscribers = Arc::clone(&remote_subscribers);
                    thread::spawn(move || accept(&listener, &registry, &subscribers));
                    ListenerStatus::Listening
                }
                Err(error) => ListenerStatus::Failed(error.to_string()),
            }
        }));
        let _ = slot.set(Arc::downgrade(&registry));
        Self {
            registry,
            subscribers,
        }
    }

    fn local(&self) -> TestResult<Client> {
        let (client, server) = UnixStream::pair()?;
        let registry = Arc::clone(&self.registry);
        let events = subscribe(&self.subscribers);
        thread::spawn(move || {
            let _ = serve(Box::new(server), registry, events);
        });
        Ok(Client::from_stream(Box::new(client))?)
    }
}

fn accept(listener: &TlsListener, registry: &Weak<Registry>, subscribers: &Subscribers) {
    while let Ok(stream) = listener.accept() {
        let Some(registry) = registry.upgrade() else {
            return;
        };
        let Some(admission) = registry.admit_remote() else {
            continue;
        };
        let events = subscribe(subscribers);
        thread::spawn(move || {
            let _ = serve_remote(stream, registry, events, admission);
        });
    }
}

/// Turns on mobile access at a free port.
fn listen(local: &Client) -> TestResult {
    let mut status = ListenerStatus::Disabled;
    for _ in 0..8 {
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        status = local
            .write_remote_access(RemoteAccessSettings {
                enabled: true,
                port,
            })?
            .status;
        if status == ListenerStatus::Listening {
            return Ok(());
        }
    }
    Err(format!("mobile access never listened: {status:?}").into())
}

/// Pairs a phone with `server`; the credential asks for TLS only. Returns the computer's local
/// connection, which grants the screen permission, the credential and the phone's id.
fn pair(server: &Server) -> TestResult<(Client, Value, DeviceId)> {
    let local = server.local()?;
    listen(&local)?;
    let mut offer = local.start_pairing()?;
    offer.invite.hosts = vec!["127.0.0.1".into()];
    let mut credential = ok(&direct_pair(offer.invite.to_link(), "Test phone".into()))?;
    credential["transport"] = json!("tls");
    let device = local
        .read_remote_access()?
        .devices
        .first()
        .map(|device| device.id)
        .ok_or("the phone is not listed")?;
    Ok((local, credential, device))
}

// ---------------------------------------------------------------------------------------------
// The phone

/// A picture or a video unit the core handed to the sink.
enum Media {
    Video {
        unit: Vec<u8>,
        reset: bool,
    },
    Frame {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

struct Sink(Mutex<Sender<Media>>);

impl ScreenSink for Sink {
    fn on_video(&self, annex_b: Vec<u8>, reset: bool) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(Media::Video {
                unit: annex_b,
                reset,
            });
    }

    fn on_frame(&self, width: u32, height: u32, rgba: Vec<u8>) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(Media::Frame {
                width,
                height,
                rgba,
            });
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

fn ok(answer: &str) -> TestResult<Value> {
    let value: Value = serde_json::from_str(answer)?;
    match value.get("ok") {
        Some(ok) => Ok(ok.clone()),
        None => Err(format!("error answer: {answer}").into()),
    }
}

/// A connected facade with the sink and the events it receives.
struct Phone {
    direct: Arc<Direct>,
    events: Receiver<Value>,
    media: Receiver<Media>,
    info: Value,
}

impl Phone {
    fn connect(credential: &Value) -> TestResult<Self> {
        let (sender, events) = mpsc::channel();
        let direct = Direct::new(Arc::new(Collector(Mutex::new(sender))));
        let info = ok(&direct.connect(credential.to_string()))?;
        let (media_sender, media) = mpsc::channel();
        direct.set_screen_sink(Some(Arc::new(Sink(Mutex::new(media_sender)))));
        Ok(Self {
            direct,
            events,
            media,
            info,
        })
    }

    fn call(&self, method: &str, args: &Value) -> TestResult<Value> {
        ok(&self.direct.call(method.into(), args.to_string()))
    }

    fn fire(&self, method: &str, args: &Value) -> TestResult {
        ok(&self.direct.fire(method.into(), args.to_string())).map(drop)
    }

    fn open(&self, h264: bool) -> TestResult<Value> {
        self.call("screenOpen", &json!({ "viewOnly": false, "h264": h264 }))
    }

    /// The code and reason `screenOpen` is refused with.
    fn open_refused(&self) -> TestResult<(String, String)> {
        let answer = self
            .direct
            .call("screenOpen".into(), json!({ "h264": false }).to_string());
        let answer: Value = serde_json::from_str(&answer)?;
        let error = answer
            .get("error")
            .ok_or_else(|| format!("expected an error, got {answer}"))?;
        Ok((
            error["code"].as_str().unwrap_or_default().to_owned(),
            error["reason"].as_str().unwrap_or_default().to_owned(),
        ))
    }

    /// The next event for which `done` holds.
    fn event(&self, what: &str, mut done: impl FnMut(&Value) -> bool) -> TestResult<Value> {
        let deadline = Instant::now() + TIMEOUT;
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

    fn screen_state(&self, state: &str) -> TestResult<Value> {
        self.event(&format!("the screen to be {state}"), |event| {
            event["t"] == "screen" && event["state"] == state
        })
    }

    /// The next `ready` event about a desktop `width` pixels wide.
    fn ready_at(&self, width: usize) -> TestResult<Value> {
        self.event(&format!("a desktop {width} pixels wide"), |event| {
            event["t"] == "screen" && event["state"] == "ready" && event["width"] == width
        })
    }

    /// The next video unit.
    fn unit(&self, what: &str) -> TestResult<(Vec<u8>, bool)> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let item = self
                .media
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| format!("timed out waiting for {what}"))?;
            if let Media::Video { unit, reset } = item {
                return Ok((unit, reset));
            }
        }
    }

    /// The next picture of `width` x `height` whose first pixel is `rgb`; its bytes.
    fn picture(&self, rgb: [u8; 3], width: usize, height: usize) -> TestResult<Vec<u8>> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let item = self
                .media
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| {
                    format!("timed out waiting for a {width}x{height} picture of {rgb:?}")
                })?;
            if let Media::Frame {
                width: w,
                height: h,
                rgba,
            } = item
                && (w, h) == (u32::try_from(width)?, u32::try_from(height)?)
                && rgba.starts_with(&[rgb[0], rgb[1], rgb[2], 255])
            {
                return Ok(rgba);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The scenario

#[test]
fn a_phone_views_and_controls_the_desktop_through_the_facade() -> TestResult {
    let desktop = Desktop::new()?;
    let server = Server::new();
    let (local, credential, device) = pair(&server)?;

    without_the_screen_feature(&credential)?;
    refused_without_permission(&credential, &desktop)?;
    local.set_device_screen(device, true)?;
    capture_cannot_start(&credential, &desktop)?;
    pictures(&credential, &desktop)?;
    input(&credential, &desktop)?;
    clipboard_and_statistics(&credential, &desktop)?;
    resizing_and_refreshing(&credential, &desktop)?;
    closing_and_reopening(&credential)?;
    video(&credential, &desktop)?;
    permission_turned_off(&credential, &local, device)?;
    local.set_device_screen(device, true)?;
    closing_with_the_connection(&credential)?;
    Ok(())
}

/// A server that does not list the screen feature: the connect answer says so, opening says why.
fn without_the_screen_feature(credential: &Value) -> TestResult {
    // SAFETY: only this thread reads or writes the environment while the server is idle.
    unsafe { std::env::set_var("ATUR_TERM_SCREEN", "0") };
    let phone = Phone::connect(credential);
    // SAFETY: as above.
    unsafe { std::env::remove_var("ATUR_TERM_SCREEN") };
    let phone = phone?;
    assert_eq!(phone.info["screen"], json!(false));
    assert_eq!(phone.open_refused()?.0, "Unsupported");
    Ok(())
}

/// A phone the computer did not allow gets the server's own explanation; opening needs a sink.
fn refused_without_permission(credential: &Value, desktop: &Desktop) -> TestResult {
    let phone = Phone::connect(credential)?;
    assert_eq!(phone.info["screen"], json!(true));
    phone.direct.set_screen_sink(None);
    assert_eq!(phone.open_refused()?.0, "Internal", "opening needs a sink");

    let (sender, _media) = mpsc::channel();
    phone
        .direct
        .set_screen_sink(Some(Arc::new(Sink(Mutex::new(sender)))));
    let (code, reason) = phone.open_refused()?;
    assert_eq!(code, "ScreenNotAllowed");
    assert!(
        reason.contains("not allowed"),
        "the server's reason, as it is: {reason}"
    );
    assert!(desktop.log().is_empty(), "the capture never started");
    Ok(())
}

/// An allowed phone, but the computer's capture program does not start.
fn capture_cannot_start(credential: &Value, desktop: &Desktop) -> TestResult {
    let phone = Phone::connect(credential)?;
    desktop.set_failing(true)?;
    let refused = phone.open_refused();
    desktop.set_failing(false)?;
    let (code, reason) = refused?;
    assert_eq!(code, "ScreenUnavailable");
    assert!(reason.contains("wayvnc"), "the server's reason: {reason}");
    Ok(())
}

/// ZRLE pictures arrive as R, G, B, A, whatever order the session keeps, and follow the desktop.
fn pictures(credential: &Value, desktop: &Desktop) -> TestResult {
    let phone = Phone::connect(credential)?;
    phone.open(false)?;
    phone.screen_state("connecting")?;
    let ready = phone.screen_state("ready")?;
    assert_eq!(
        (ready["width"].as_u64(), ready["height"].as_u64()),
        (Some(320), Some(180))
    );
    assert_eq!(ready["name"], "fake-desktop");

    let rgba = phone.picture([200, 30, 60], WIDTH, HEIGHT)?;
    assert_eq!(rgba.len(), WIDTH * HEIGHT * 4);
    assert!(
        rgba.chunks_exact(4)
            .all(|pixel| pixel[..3] == [200, 30, 60] && pixel[3] == 255)
    );
    desktop.set_colour([10, 200, 90])?;
    phone.picture([10, 200, 90], WIDTH, HEIGHT)?;
    desktop.wait_log("the ZRLE encodings", |log| {
        log.iter().any(|line| line.starts_with("encodings 16,1,0,"))
    })?;
    Ok(())
}

/// Pointer, wheel, keys and typed text reach the desktop in the order they were fired; a
/// view-only screen drops them.
fn input(credential: &Value, desktop: &Desktop) -> TestResult {
    let phone = Phone::connect(credential)?;
    phone.open(false)?;
    phone.screen_state("ready")?;
    phone.fire("screenPointer", &json!({ "x": 10, "y": 20, "buttons": 1 }))?;
    phone.fire(
        "screenPointer",
        &json!({ "x": 9999, "y": -5, "buttons": 0 }),
    )?;
    phone.fire("screenKey", &json!({ "keysym": 65, "down": true }))?;
    phone.fire("screenKey", &json!({ "keysym": 65, "down": false }))?;
    phone.fire("screenType", &json!({ "text": "\u{e9}\n" }))?;
    phone.fire(
        "screenScroll",
        &json!({ "x": 50, "y": 60, "dx": 0, "dy": 1 }),
    )?;
    desktop.wait_logged(&[
        "pointer 1 10 20",
        "pointer 0 319 0",
        "key 1 65",
        "key 0 65",
        "key 1 233",
        "key 0 233",
        "key 1 65293",
        "key 0 65293",
        "pointer 8 50 60",
        "pointer 0 50 60",
    ])?;

    phone.fire("screenViewOnly", &json!({ "viewOnly": true }))?;
    phone.fire("screenPointer", &json!({ "x": 30, "y": 40, "buttons": 1 }))?;
    phone.fire("screenKey", &json!({ "keysym": 66, "down": true }))?;
    phone.fire("screenViewOnly", &json!({ "viewOnly": false }))?;
    phone.fire("screenKey", &json!({ "keysym": 67, "down": true }))?;
    phone.fire("screenReleaseAll", &json!({}))?;
    desktop.wait_logged(&["key 1 67", "key 0 67"])?;
    let log = desktop.log();
    assert!(!log.contains(&"pointer 1 30 40".to_owned()), "{log:?}");
    assert!(!log.contains(&"key 1 66".to_owned()), "{log:?}");
    Ok(())
}

/// Clipboard text goes out and the desktop's comes back as an event; statistics come about once
/// a second.
fn clipboard_and_statistics(credential: &Value, desktop: &Desktop) -> TestResult {
    let phone = Phone::connect(credential)?;
    phone.open(false)?;
    phone.screen_state("ready")?;
    phone.fire("screenClipboard", &json!({ "text": "hello" }))?;
    desktop.wait_logged(&["cut hello"])?;
    let clipboard = phone.event("the desktop's clipboard", |event| {
        event["t"] == "screenClipboard"
    })?;
    assert_eq!(clipboard["text"], "echo:hello");

    let stats = phone.event("statistics", |event| event["t"] == "screenStats")?;
    assert_eq!(stats["codec"], "zrle");
    for key in ["fps", "kbps", "decodeMs"] {
        assert!(stats[key].is_number(), "{key}: {stats}");
    }
    assert!(stats["dropped"].is_u64(), "{stats}");
    Ok(())
}

/// A desktop that changes size is announced again and pictures follow its size; a refresh asks
/// the server for a whole update.
fn resizing_and_refreshing(credential: &Value, desktop: &Desktop) -> TestResult {
    let phone = Phone::connect(credential)?;
    phone.open(false)?;
    phone.screen_state("ready")?;
    desktop.set_size(200, 100)?;
    let resized = phone.ready_at(200)?;
    assert_eq!(resized["height"], 100);
    let rgba = phone.picture([10, 200, 90], 200, 100)?;
    assert_eq!(rgba.len(), 200 * 100 * 4);
    desktop.set_size(WIDTH, HEIGHT)?;
    phone.ready_at(WIDTH)?;
    phone.picture([10, 200, 90], WIDTH, HEIGHT)?;

    // The resizes ask for whole updates too; let those reach the desktop first.
    thread::sleep(Duration::from_millis(300));
    let asked = desktop.count("full-update");
    phone.fire("screenRefresh", &json!({}))?;
    desktop.wait_log("a refresh", |log| {
        log.iter().filter(|line| *line == "full-update").count() > asked
    })
}

/// Closing is idempotent and reported once; opening again closes the previous screen first.
fn closing_and_reopening(credential: &Value) -> TestResult {
    let phone = Phone::connect(credential)?;
    phone.open(false)?;
    phone.screen_state("ready")?;
    phone.call("screenClose", &json!({}))?;
    let closed = phone.screen_state("closed")?;
    assert_eq!(closed["reason"], "");
    phone.call("screenClose", &json!({}))?;
    phone.fire("screenPointer", &json!({ "x": 1, "y": 1, "buttons": 0 }))?;

    phone.call("screenOpen", &json!({ "viewOnly": true, "h264": false }))?;
    phone.screen_state("ready")?;
    phone.open(false)?;
    let mut states = Vec::new();
    phone.event(
        "the first screen to close and the second to be ready",
        |event| {
            if event["t"] == "screen" {
                states.push(event["state"].as_str().unwrap_or_default().to_owned());
            }
            states.iter().any(|state| state == "ready")
        },
    )?;
    assert_eq!(states, ["closed", "connecting", "ready"]);
    phone.call("screenClose", &json!({}))?;
    Ok(())
}

/// H.264 access units go to the sink as the server sent them, with the reset flag.
fn video(credential: &Value, desktop: &Desktop) -> TestResult {
    let phone = Phone::connect(credential)?;
    phone.open(true)?;
    phone.screen_state("ready")?;
    let (unit, reset) = phone.unit("the first access unit")?;
    assert!(reset, "the first unit starts a context");
    assert_eq!(unit, desktop.units[0]);
    assert!(unit.starts_with(&[0, 0, 0, 1]));
    let (unit, reset) = phone.unit("the second access unit")?;
    assert!(!reset);
    assert_eq!(unit, desktop.units[1]);
    desktop.wait_log("the H.264 encodings", |log| {
        log.iter()
            .any(|line| line.starts_with("encodings 50,16,1,0,"))
    })?;
    let stats = phone.event("statistics", |event| {
        event["t"] == "screenStats"
            && event["codec"]
                .as_str()
                .is_some_and(|codec| codec.starts_with("h264"))
    })?;
    assert_eq!(stats["codec"], "h264 (native)");

    // A refresh restarts the stream: a new context, which begins with the first unit again.
    phone.fire("screenRefresh", &json!({}))?;
    loop {
        let (unit, reset) = phone.unit("a unit that starts a context")?;
        if reset {
            assert_eq!(unit, desktop.units[0]);
            break;
        }
    }
    phone.call("screenClose", &json!({}))?;
    Ok(())
}

/// Turning the permission off ends a viewer at once, with the server's reason; opening is refused.
fn permission_turned_off(credential: &Value, local: &Client, device: DeviceId) -> TestResult {
    let phone = Phone::connect(credential)?;
    phone.open(false)?;
    phone.screen_state("ready")?;
    local.set_device_screen(device, false)?;
    let closed = phone.screen_state("closed")?;
    let reason = closed["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("turned off"),
        "the server's reason: {reason}"
    );
    assert_eq!(phone.open_refused()?.0, "ScreenNotAllowed");
    Ok(())
}

/// Disconnecting ends the screen.
fn closing_with_the_connection(credential: &Value) -> TestResult {
    let phone = Phone::connect(credential)?;
    phone.open(false)?;
    phone.screen_state("ready")?;
    phone.direct.disconnect();
    let closed = phone.screen_state("closed")?;
    assert_eq!(closed["reason"], "");
    assert_eq!(phone.open_refused()?.0, "Disconnected");
    Ok(())
}
