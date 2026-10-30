//! The computer's desktop on the phone, over the connection the facade already holds.
//!
//! The server relays the bytes of a `wayvnc` capture on one channel of the connection
//! (`OpenScreen`); `atur-screen` speaks RFB over it. This module joins the two to the app:
//!
//! * [`Link`] is the session's byte pipe, a `muxy_client::ScreenLink` that reports every
//!   handled chunk back, which is what lets the server send more;
//! * a pump thread, woken by the session, hands the app what changed: state and size, the newest
//!   picture of a ZRLE or raw update (as RGBA, at most about 30 a second), clipboard text and
//!   statistics once a second;
//! * H.264 is decoded by the phone's hardware, not here: [`Forward`] is the session's decoder and
//!   passes every access unit on to [`ScreenSink::on_video`], and returning from that call is
//!   what acknowledges the bytes;
//! * input arrives as `fire` requests, queued with typing, and goes straight to the session.
//!
//! Nothing queues: the RFB reader hands over a few chunks, the session keeps one picture, and a
//! slow sink slows the acknowledgements, so the server sends only the newest.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use atur_screen::decode::{DecodeError, H264Decoder};
use atur_screen::{
    Frame, Pixels, PointerButtons, Screen, ScreenInfo, ScreenOptions, ScreenState, ScreenTransport,
    keys,
};
use muxy_client::{Client, ClientError, ScreenEvent, ScreenLink};
use muxy_protocol::ErrorCode;
use serde_json::{Value, json};

use crate::MobileError;
use crate::direct::{Answer, DirectListener, Fail, bad, lock, str_arg};

/// The least time between two pictures handed to the sink: about 30 a second.
const PICTURE_INTERVAL: Duration = Duration::from_millis(33);
/// How often statistics go to the app.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

const RETURN: u32 = 0xff0d;
const TAB: u32 = 0xff09;
const BACKSPACE: u32 = 0xff08;

/// What the phone's decoder is called in the statistics.
#[cfg(target_os = "android")]
const DECODER: &str = "mediacodec";
#[cfg(target_os = "ios")]
const DECODER: &str = "videotoolbox";
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const DECODER: &str = "native";

/// Receives the screen's media on a core thread, in order; return fast.
///
/// The core waits for each call, so a sink that returns late slows the stream down instead of
/// building a queue.
#[uniffi::export(with_foreign)]
pub trait ScreenSink: Send + Sync {
    /// One H.264 access unit, Annex B (start codes), SPS and PPS inline on key frames. `reset`:
    /// the server started a new context, so drop reference state (and set the decoder up again
    /// from the SPS and PPS in this unit). Returning is what lets the core acknowledge the bytes
    /// to the server (flow control).
    fn on_video(&self, annex_b: Vec<u8>, reset: bool);
    /// A complete picture from ZRLE or raw updates: `width * height * 4` bytes in R, G, B, A
    /// order, A = 255, stride `width * 4`. Newest only: the core drops pictures that arrive
    /// while the sink is busy with the previous one.
    fn on_frame(&self, width: u32, height: u32, rgba: Vec<u8>);
}

/// Where the viewer finds the sink the app set; the app may replace it while a screen is open.
pub(crate) type SinkCell = Mutex<Option<Arc<dyn ScreenSink>>>;

/// The connection's one screen, if open.
#[derive(Default)]
pub(crate) struct ScreenSlot {
    current: Mutex<Option<Arc<Viewer>>>,
}

impl ScreenSlot {
    /// Ends the viewer, if any. Safe to call again.
    pub(crate) fn close(&self) {
        let viewer = lock(&self.current).take();
        if let Some(viewer) = viewer {
            viewer.close();
        }
    }

    /// Carries out one input; nothing happens without a screen.
    pub(crate) fn input(&self, input: Input) {
        let viewer = lock(&self.current).clone();
        if let Some(viewer) = viewer {
            viewer.apply(input);
        }
    }

    /// Forgets a viewer that ended by itself.
    fn release(&self, viewer: &Arc<Viewer>) {
        let mut current = lock(&self.current);
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, viewer))
        {
            *current = None;
        }
    }
}

/// Dropping the connection's facade ends its screen; the reporting thread holds only a weak
/// reference, so it does not keep the screen alive.
impl Drop for ScreenSlot {
    fn drop(&mut self) {
        self.close();
    }
}

/// `screenOpen`, `screenClose`: the requests that wait for the server. `None` for any other method.
pub(crate) fn call(
    slot: &Arc<ScreenSlot>,
    client: &Client,
    sink: &Arc<SinkCell>,
    listener: &Arc<dyn DirectListener>,
    method: &str,
    args: &Value,
) -> Option<Answer> {
    match method {
        "screenOpen" => Some(open(slot, client, sink, listener, args)),
        "screenClose" => {
            slot.close();
            Some(Ok(json!({})))
        }
        _ => None,
    }
}

/// Opens the channel and starts the session. The RFB handshake goes on in the background; the
/// app learns how it went from `screen` events.
fn open(
    slot: &Arc<ScreenSlot>,
    client: &Client,
    sink: &Arc<SinkCell>,
    listener: &Arc<dyn DirectListener>,
    args: &Value,
) -> Answer {
    let view_only = args
        .get("viewOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let h264 = args.get("h264").and_then(Value::as_bool).unwrap_or(false);
    if lock(sink).is_none() {
        return Err(Fail::Bad("no screen sink is set".into()));
    }
    // One screen per connection. The server may take seconds to start its capture, so the old
    // one goes first, before this request waits.
    slot.close();
    let link = client.open_screen().map_err(refusal)?;
    let remote_reason = Arc::new(Mutex::new(None));
    let signal = Arc::new(Signal::default());
    let wake = Arc::clone(&signal);
    let decoder: Box<dyn H264Decoder> = Box::new(Forward {
        sink: Arc::clone(sink),
    });
    let screen = Screen::start_with_decoder(
        Link {
            link,
            reason: Arc::clone(&remote_reason),
        },
        ScreenOptions {
            view_only,
            prefer_h264: h264,
        },
        move || wake.notify(),
        move |_| Some(decoder),
    );
    let viewer = Arc::new(Viewer {
        screen,
        signal,
        ended: Mutex::new(false),
        listener: Arc::clone(listener),
        sink: Arc::clone(sink),
        remote_reason,
    });
    let previous = lock(&slot.current).replace(Arc::clone(&viewer));
    if let Some(previous) = previous {
        previous.close();
    }
    let pump = Arc::clone(&viewer);
    let owner = Arc::downgrade(slot);
    let started = thread::Builder::new()
        .name("atur-direct-screen".into())
        .spawn(move || {
            pump.run();
            if let Some(owner) = owner.upgrade() {
                owner.release(&pump);
            }
        });
    if let Err(error) = started {
        slot.release(&viewer);
        viewer.close();
        return Err(Fail::Core(MobileError::Unreachable {
            reason: error.to_string(),
        }));
    }
    Ok(json!({}))
}

/// Tells why a screen could not be opened: the server refused this phone, the server could not
/// capture its desktop, or there is no screen feature at all.
fn refusal(error: ClientError) -> Fail {
    Fail::Core(match error {
        ClientError::Server(reply) if reply.code == ErrorCode::Unauthorized => {
            MobileError::ScreenNotAllowed {
                reason: reply.message,
            }
        }
        ClientError::Server(reply) => MobileError::ScreenUnavailable {
            reason: reply.message,
        },
        ClientError::Invalid(ErrorCode::Unsupported) => MobileError::Unsupported,
        other => other.into(),
    })
}

/// Wakes the pump: a flag plus a condition variable, so waiting costs nothing.
#[derive(Default)]
struct Signal {
    pending: Mutex<bool>,
    changed: Condvar,
}

impl Signal {
    fn notify(&self) {
        *lock(&self.pending) = true;
        self.changed.notify_all();
    }

    /// Waits for a notification for at most `timeout`; whether one came.
    fn wait(&self, timeout: Duration) -> bool {
        let mut pending = lock(&self.pending);
        if !*pending {
            pending = self
                .changed
                .wait_timeout(pending, timeout)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        std::mem::take(&mut *pending)
    }
}

/// The session's byte pipe to the server: the screen channel of the connection.
struct Link {
    link: ScreenLink,
    /// Why the server ended the stream, when it said.
    reason: Arc<Mutex<Option<String>>>,
}

impl ScreenTransport for Link {
    fn recv(&self) -> Option<Vec<u8>> {
        match self.link.recv()? {
            ScreenEvent::Data(bytes) => Some(bytes),
            ScreenEvent::Closed(reason) => {
                if !reason.trim().is_empty() {
                    *lock(&self.reason) = Some(reason);
                }
                None
            }
        }
    }

    fn send(&self, bytes: &[u8]) -> Result<(), String> {
        self.link.send(bytes).map_err(|error| error.to_string())
    }

    fn processed(&self, bytes: usize) {
        self.link.processed(bytes);
    }

    fn close(&self) {
        self.link.close();
    }
}

/// The session's H.264 decoder on a phone: the platform decodes, so every access unit goes to the
/// sink as it came. The session calls it while it handles a chunk and acknowledges the chunk
/// afterwards.
struct Forward {
    sink: Arc<SinkCell>,
}

impl H264Decoder for Forward {
    fn name(&self) -> String {
        DECODER.to_owned()
    }

    fn decode(&mut self, annex_b: &[u8], reset: bool) -> Result<(), DecodeError> {
        let sink = lock(&self.sink).clone();
        if let Some(sink) = sink {
            sink.on_video(annex_b.to_vec(), reset);
        }
        Ok(())
    }
}

/// One open screen: the session and the thread that reports on it.
struct Viewer {
    screen: Screen,
    signal: Arc<Signal>,
    /// Whether the closing event went out. Also orders every event of this viewer: none follows
    /// the closing one, and a close by the app cannot overtake an event being sent.
    ended: Mutex<bool>,
    listener: Arc<dyn DirectListener>,
    sink: Arc<SinkCell>,
    remote_reason: Arc<Mutex<Option<String>>>,
}

impl Viewer {
    /// Sends an event; `false` once the viewer is over.
    fn emit(&self, event: &Value) -> bool {
        let ended = lock(&self.ended);
        if *ended {
            return false;
        }
        self.listener.on_message(event.to_string());
        true
    }

    fn is_over(&self) -> bool {
        *lock(&self.ended)
    }

    /// Sends the closing event, once.
    fn end(&self, reason: &str) {
        let mut ended = lock(&self.ended);
        if std::mem::replace(&mut *ended, true) {
            return;
        }
        self.listener
            .on_message(json!({ "t": "screen", "state": "closed", "reason": reason }).to_string());
    }

    /// The app ends the viewer.
    fn close(&self) {
        self.screen.close();
        self.end("");
        self.signal.notify();
    }

    /// The server's own words for why the stream ended, else the session's.
    fn closing_reason(&self, session: &str) -> String {
        lock(&self.remote_reason)
            .clone()
            .filter(|remote| !remote.trim().is_empty())
            .unwrap_or_else(|| session.to_owned())
    }

    fn apply(&self, input: Input) {
        let screen = &self.screen;
        match input {
            Input::Pointer { x, y, buttons } => screen.pointer(x, y, buttons),
            Input::Scroll { x, y, dx, dy } => screen.scroll(x, y, dx, dy),
            Input::Key { keysym, down } => screen.key(keysym, down),
            Input::Type(text) => type_text(screen, &text),
            Input::ViewOnly(view_only) => screen.set_view_only(view_only),
            Input::Clipboard(text) => screen.send_clipboard(&text),
            Input::ReleaseAll => screen.release_all(),
            Input::Refresh => screen.refresh(),
        }
    }

    /// Reports to the app until the screen is over.
    fn run(&self) {
        if !self.emit(&json!({ "t": "screen", "state": "connecting" })) {
            return;
        }
        let mut announced: Option<ScreenInfo> = None;
        let mut last_id = 0;
        let mut last_picture: Option<Instant> = None;
        let mut picture_maybe = false;
        let mut next_stats = Instant::now() + STATS_INTERVAL;
        let mut timeout = STATS_INTERVAL;
        loop {
            if self.signal.wait(timeout) {
                picture_maybe = true;
            }
            if self.is_over() {
                return;
            }
            match self.screen.state() {
                ScreenState::Connecting => {}
                ScreenState::Ready(info) => {
                    if announced.as_ref() != Some(&info) {
                        let sent = self.emit(&json!({
                            "t": "screen",
                            "state": "ready",
                            "width": info.width,
                            "height": info.height,
                            "name": info.name,
                        }));
                        if !sent {
                            return;
                        }
                        announced = Some(info);
                    }
                }
                ScreenState::Closed(reason) => {
                    self.end(&self.closing_reason(&reason));
                    return;
                }
            }
            let mut picture_wait = None;
            if picture_maybe {
                let wait = last_picture.map_or(Duration::ZERO, |at| {
                    PICTURE_INTERVAL.saturating_sub(at.elapsed())
                });
                if wait.is_zero() {
                    picture_maybe = false;
                    if let Some(frame) = self.screen.frame()
                        && frame.id != last_id
                    {
                        last_id = frame.id;
                        self.show(&frame);
                        last_picture = Some(Instant::now());
                    }
                } else {
                    picture_wait = Some(wait);
                }
            }
            if let Some(text) = self.screen.take_clipboard()
                && !self.emit(&json!({ "t": "screenClipboard", "text": text }))
            {
                return;
            }
            if announced.is_some() && Instant::now() >= next_stats {
                next_stats = Instant::now() + STATS_INTERVAL;
                if !self.emit(&self.stats()) {
                    return;
                }
            }
            timeout = next_stats.saturating_duration_since(Instant::now());
            if let Some(wait) = picture_wait {
                timeout = timeout.min(wait);
            }
        }
    }

    /// Hands the picture to the sink, which the core waits for.
    fn show(&self, frame: &Frame) {
        let sink = lock(&self.sink).clone();
        if let Some(sink) = sink
            && let Some(rgba) = rgba(frame)
        {
            sink.on_frame(frame.width, frame.height, rgba);
        }
    }

    fn stats(&self) -> Value {
        let stats = self.screen.stats();
        json!({
            "t": "screenStats",
            "codec": stats.codec,
            "fps": tenths(stats.fps),
            "kbps": tenths(stats.kbps),
            "dropped": stats.dropped,
            "decodeMs": tenths(stats.decode_ms),
        })
    }
}

fn tenths(value: f32) -> f64 {
    (f64::from(value) * 10.0).round() / 10.0
}

/// The picture as R, G, B, A bytes, converted from the session's B, G, R, A once. `None` when
/// the picture is not plain pixels or its size does not match its bytes.
fn rgba(frame: &Frame) -> Option<Vec<u8>> {
    match &frame.pixels {
        Pixels::Bgra(bgra) => {
            let expected = usize::try_from(frame.width)
                .ok()?
                .checked_mul(usize::try_from(frame.height).ok()?)?
                .checked_mul(4)?;
            if bgra.len() != expected {
                return None;
            }
            let mut out = vec![0_u8; expected];
            for (to, from) in out.chunks_exact_mut(4).zip(bgra.chunks_exact(4)) {
                to[0] = from[2];
                to[1] = from[1];
                to[2] = from[0];
                to[3] = 255;
            }
            Some(out)
        }
        #[cfg(target_os = "macos")]
        Pixels::Surface(_) => None,
    }
}

/// Presses and releases the key of each character; `\n` is Return, `\t` Tab, `\u{8}` `BackSpace`.
fn type_text(screen: &Screen, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let keysym = match c {
            // A Windows line ending is one Return.
            '\r' if chars.peek() == Some(&'\n') => None,
            '\r' | '\n' => Some(RETURN),
            '\t' => Some(TAB),
            '\u{8}' => Some(BACKSPACE),
            other => keys::char_keysym(other),
        };
        let Some(keysym) = keysym else { continue };
        screen.key(keysym, true);
        screen.key(keysym, false);
    }
}

/// What the phone does at the computer's screen; fired, so ordered with the typing.
#[derive(Debug)]
pub(crate) enum Input {
    Pointer {
        x: i32,
        y: i32,
        buttons: PointerButtons,
    },
    Scroll {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
    Key {
        keysym: u32,
        down: bool,
    },
    Type(String),
    ViewOnly(bool),
    Clipboard(String),
    ReleaseAll,
    Refresh,
}

/// The methods of `fire` that are screen input.
const INPUT_METHODS: [&str; 8] = [
    "screenPointer",
    "screenScroll",
    "screenKey",
    "screenType",
    "screenViewOnly",
    "screenClipboard",
    "screenReleaseAll",
    "screenRefresh",
];

pub(crate) fn is_input(method: &str) -> bool {
    INPUT_METHODS.contains(&method)
}

impl Input {
    /// `None` when `method` is not screen input.
    pub(crate) fn parse(method: &str, args: &Value) -> Option<Result<Self, Fail>> {
        let input = match method {
            "screenPointer" => int_arg(args, "x").and_then(|x| {
                Ok(Self::Pointer {
                    x,
                    y: int_arg(args, "y")?,
                    buttons: buttons(args),
                })
            }),
            "screenScroll" => int_arg(args, "x").and_then(|x| {
                Ok(Self::Scroll {
                    x,
                    y: int_arg(args, "y")?,
                    dx: optional_int_arg(args, "dx")?,
                    dy: optional_int_arg(args, "dy")?,
                })
            }),
            "screenKey" => keysym_arg(args).and_then(|keysym| {
                Ok(Self::Key {
                    keysym,
                    down: bool_arg(args, "down")?,
                })
            }),
            "screenType" => str_arg(args, "text").map(Self::Type),
            "screenViewOnly" => bool_arg(args, "viewOnly").map(Self::ViewOnly),
            "screenClipboard" => str_arg(args, "text").map(Self::Clipboard),
            "screenReleaseAll" => Ok(Self::ReleaseAll),
            "screenRefresh" => Ok(Self::Refresh),
            _ => return None,
        };
        Some(input)
    }
}

/// A whole number, as JavaScript may send it: `12` or `12.0`, rounded and clamped to `i32`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "rounded and clamped to the i32 range first"
)]
fn int_arg(args: &Value, name: &str) -> Result<i32, Fail> {
    let number = args
        .get(name)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
        .ok_or_else(|| bad(name))?;
    Ok(number
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32)
}

fn optional_int_arg(args: &Value, name: &str) -> Result<i32, Fail> {
    if args.get(name).is_none() {
        Ok(0)
    } else {
        int_arg(args, name)
    }
}

fn bool_arg(args: &Value, name: &str) -> Result<bool, Fail> {
    args.get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| bad(name))
}

fn keysym_arg(args: &Value) -> Result<u32, Fail> {
    args.get("keysym")
        .and_then(Value::as_u64)
        .and_then(|keysym| u32::try_from(keysym).ok())
        .ok_or_else(|| bad("keysym"))
}

/// The `buttons` bitmask: 1 left, 2 middle, 4 right; none when missing.
fn buttons(args: &Value) -> PointerButtons {
    let mask = args.get("buttons").and_then(Value::as_u64).unwrap_or(0);
    PointerButtons {
        left: mask & 1 != 0,
        middle: mask & 2 != 0,
        right: mask & 4 != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fired_screen_method_parses() {
        for method in INPUT_METHODS {
            assert!(Input::parse(method, &json!({})).is_some(), "{method}");
            assert!(is_input(method));
        }
        assert!(Input::parse("input", &json!({})).is_none());
        assert!(!is_input("screenOpen"));
    }

    #[test]
    fn pointer_and_scroll_arguments_are_forgiving_but_checked() {
        let pointer = Input::parse("screenPointer", &json!({"x": 10.4, "y": -3, "buttons": 5}));
        assert!(matches!(
            pointer,
            Some(Ok(Input::Pointer {
                x: 10,
                y: -3,
                buttons: PointerButtons {
                    left: true,
                    middle: false,
                    right: true
                }
            }))
        ));
        let scroll = Input::parse("screenScroll", &json!({"x": 1, "y": 2, "dy": 3}));
        assert!(matches!(
            scroll,
            Some(Ok(Input::Scroll {
                x: 1,
                y: 2,
                dx: 0,
                dy: 3
            }))
        ));
        assert!(matches!(
            Input::parse("screenPointer", &json!({"x": 1})),
            Some(Err(Fail::Bad(_)))
        ));
        assert!(matches!(
            Input::parse("screenKey", &json!({"keysym": -1, "down": true})),
            Some(Err(Fail::Bad(_)))
        ));
        assert!(matches!(
            Input::parse("screenKey", &json!({"keysym": 65, "down": true})),
            Some(Ok(Input::Key {
                keysym: 65,
                down: true
            }))
        ));
    }

    #[test]
    fn a_picture_is_converted_to_rgba_once() {
        let frame = Frame {
            id: 1,
            width: 2,
            height: 1,
            pixels: Pixels::Bgra(Arc::from(vec![1_u8, 2, 3, 255, 4, 5, 6, 255])),
        };
        assert_eq!(rgba(&frame), Some(vec![3, 2, 1, 255, 6, 5, 4, 255]));
        let short = Frame {
            id: 2,
            width: 3,
            height: 1,
            pixels: Pixels::Bgra(Arc::from(vec![0_u8; 8])),
        };
        assert_eq!(rgba(&short), None);
    }
}
