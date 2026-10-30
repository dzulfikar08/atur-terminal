//! The screen session: the public handle, its threads and the input path.
//!
//! Threads: a reader that only blocks in `ScreenTransport::recv` and hands chunks over a
//! small bounded channel (so the server window stays closed while we are busy), a
//! processor that owns the RFB state machine and decoders, and a timer that flushes
//! coalesced pointer moves and rate-limited wake-ups.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::decode::{self, FrameSink, H264Decoder};
use crate::rfb::{Codec, Event, Machine, clipboard, msg};
use crate::{Frame, Pixels};

/// Pointer moves are sent at most this often; newer ones replace the pending one.
const MOVE_INTERVAL: Duration = Duration::from_millis(8);
/// `wake` is called at most about 118 times per second.
const WAKE_INTERVAL: Duration = Duration::from_micros(8_500);
/// A framebuffer copy is published at most this often.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(4);
const STATS_WINDOW: Duration = Duration::from_secs(1);
/// Chunks the reader may hand over before it blocks (and the server window closes).
const INBOX_DEPTH: usize = 4;
const MAX_WHEEL_NOTCHES: u32 = 10;
const MAX_PRESSED_KEYS: usize = 256;
const WHEEL_UP: u8 = 1 << 3;
const WHEEL_DOWN: u8 = 1 << 4;
const WHEEL_LEFT: u8 = 1 << 5;
const WHEEL_RIGHT: u8 = 1 << 6;
const LOST_REASON: &str = "The connection to the remote screen was lost.";

/// A byte pipe to the RFB server (wayvnc) on the other side.
pub trait ScreenTransport: Send + Sync + 'static {
    /// Blocks until bytes arrive; only the session's reader thread calls it. `None` = closed.
    fn recv(&self) -> Option<Vec<u8>>;
    /// Sends raw RFB bytes; callable from any thread.
    fn send(&self, bytes: &[u8]) -> Result<(), String>;
    /// Called after the bytes of a received chunk were fully handled.
    fn processed(&self, bytes: usize);
    /// Ends the transport; must make a blocked `recv` return `None`.
    fn close(&self);
}

/// How a session starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenOptions {
    /// Ignore all input.
    pub view_only: bool,
    /// Ask for H.264 when a decoder exists.
    pub prefer_h264: bool,
}

impl Default for ScreenOptions {
    fn default() -> Self {
        Self {
            view_only: false,
            prefer_h264: true,
        }
    }
}

/// Where a session is in its life.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScreenState {
    Connecting,
    Ready(ScreenInfo),
    /// Over; the reason is a user-presentable sentence, empty when the viewer closed it.
    Closed(String),
}

/// The remote screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenInfo {
    pub width: u32,
    pub height: u32,
    pub name: String,
}

/// The mouse buttons held down.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PointerButtons {
    pub left: bool,
    pub middle: bool,
    pub right: bool,
}

impl PointerButtons {
    fn mask(self) -> u8 {
        u8::from(self.left) | (u8::from(self.middle) << 1) | (u8::from(self.right) << 2)
    }
}

/// Live numbers about the session.
#[derive(Clone, Debug, Default)]
pub struct ScreenStats {
    /// `"h264 (videotoolbox)"`, `"h264 (ffmpeg vaapi)"`, `"zrle"`, `"raw"`; empty before the
    /// first update.
    pub codec: String,
    pub fps: f32,
    pub kbps: f32,
    /// Frames that were replaced before the UI fetched them.
    pub dropped: u64,
    /// Time spent decoding the latest update.
    pub decode_ms: f32,
}

/// A handle to a running screen session; clones share it.
#[derive(Clone)]
pub struct Screen {
    guard: Arc<Guard>,
}

impl fmt::Debug for Screen {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Screen").finish_non_exhaustive()
    }
}

/// Closes the session when the last handle goes away (the threads only hold `Shared`).
struct Guard {
    shared: Arc<Shared>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.shared.close();
    }
}

impl Screen {
    /// Starts the session threads. `wake` is called (from any thread, rate-limited to
    /// about 120 per second) when there is a new frame, state, clipboard or stats to read.
    pub fn start(
        transport: impl ScreenTransport,
        options: ScreenOptions,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self::start_with_decoder(transport, options, wake, decode::new_h264)
    }

    /// Like [`Screen::start`] with the H.264 decoder factory injected; for tests.
    #[doc(hidden)]
    pub fn start_with_decoder(
        transport: impl ScreenTransport,
        options: ScreenOptions,
        wake: impl Fn() + Send + Sync + 'static,
        make_decoder: impl FnOnce(FrameSink) -> Option<Box<dyn H264Decoder>>,
    ) -> Self {
        let shared = Arc::new(Shared::new(Box::new(transport), Box::new(wake), options));
        let decoder = if options.prefer_h264 {
            let target = Arc::clone(&shared);
            let sink: FrameSink = Arc::new(move |frame| target.publish(frame));
            make_decoder(sink)
        } else {
            None
        };
        let machine = Machine::new(decoder);
        let (sender, receiver) = mpsc::sync_channel(INBOX_DEPTH);
        let started = spawn(&shared, "screen-reader", move |shared| {
            read_loop(shared, &sender);
        }) && spawn(&shared, "screen-processor", move |shared| {
            process_loop(shared, &receiver, machine);
        }) && spawn(&shared, "screen-timer", timer_loop);
        if !started {
            shared.fail("The screen viewer could not start its threads.");
        }
        Self {
            guard: Arc::new(Guard { shared }),
        }
    }

    pub fn state(&self) -> ScreenState {
        lock(&self.guard.shared.state).clone()
    }

    /// The newest published frame, `None` before the first.
    pub fn frame(&self) -> Option<Frame> {
        let mut slot = lock(&self.guard.shared.slot);
        slot.taken = true;
        slot.frame.clone()
    }

    /// Moves the pointer to framebuffer pixel `(x, y)` (clamped) with `buttons` held.
    /// No-op in view-only mode.
    pub fn pointer(&self, x: i32, y: i32, buttons: PointerButtons) {
        let shared = &self.guard.shared;
        if !shared.view_only.load(Ordering::Relaxed) {
            shared.pointer(x, y, buttons.mask());
        }
    }

    /// Wheel notches at `(x, y)`: positive `dy` scrolls up (away from the user), positive
    /// `dx` scrolls right. No-op in view-only mode.
    pub fn scroll(&self, x: i32, y: i32, dx: i32, dy: i32) {
        let shared = &self.guard.shared;
        if !shared.view_only.load(Ordering::Relaxed) {
            shared.scroll(x, y, dx, dy);
        }
    }

    /// Presses or releases the key with X11 keysym `keysym`. No-op in view-only mode.
    pub fn key(&self, keysym: u32, down: bool) {
        let shared = &self.guard.shared;
        if !shared.view_only.load(Ordering::Relaxed) {
            shared.key(keysym, down);
        }
    }

    /// Releases every pressed key and mouse button (focus lost).
    pub fn release_all(&self) {
        self.guard.shared.release_all();
    }

    pub fn set_view_only(&self, view_only: bool) {
        let shared = &self.guard.shared;
        shared.view_only.store(view_only, Ordering::Relaxed);
        if view_only {
            shared.release_all();
        }
    }

    /// Local clipboard to remote: through the extended clipboard (UTF-8) when the server
    /// has it, else as Latin-1 `ClientCutText`. No-op in view-only mode.
    pub fn send_clipboard(&self, text: &str) {
        let shared = &self.guard.shared;
        if !shared.view_only.load(Ordering::Relaxed) {
            shared.send_clipboard(text);
        }
    }

    /// Remote clipboard text that arrived since the last call.
    pub fn take_clipboard(&self) -> Option<String> {
        lock(&self.guard.shared.clipboard_in).take()
    }

    pub fn stats(&self) -> ScreenStats {
        let shared = &self.guard.shared;
        let dropped = lock(&shared.slot).dropped;
        let mut stats = lock(&shared.stats);
        stats.roll(Instant::now());
        ScreenStats {
            codec: stats.codec.clone(),
            fps: stats.fps,
            kbps: stats.kbps,
            dropped,
            decode_ms: stats.decode_ms,
        }
    }

    /// Ends the session; the state becomes `Closed("")`.
    pub fn close(&self) {
        self.guard.shared.close();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Input bookkeeping and everything that must be ordered with writes. Writes to the
/// transport happen with this locked, so events can never be reordered.
struct Io {
    buttons: u8,
    /// Newest pointer position; sentinel until the first event.
    pos: (u16, u16),
    /// A move to `pos` has not been sent yet.
    pending_move: bool,
    last_move: Option<Instant>,
    pressed: Vec<u32>,
    last_wake: Option<Instant>,
    wake_pending: bool,
}

struct FrameSlot {
    frame: Option<Frame>,
    /// `frame()` returned the current frame.
    taken: bool,
    dropped: u64,
}

struct StatsInner {
    window_start: Instant,
    frames: u32,
    bytes: u32,
    fps: f32,
    kbps: f32,
    codec: String,
    decode_ms: f32,
}

impl StatsInner {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "display numbers over a one second window"
    )]
    fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed < STATS_WINDOW {
            return;
        }
        let seconds = elapsed.as_secs_f64();
        self.fps = (f64::from(self.frames) / seconds) as f32;
        self.kbps = (f64::from(self.bytes) * 8.0 / 1000.0 / seconds) as f32;
        self.frames = 0;
        self.bytes = 0;
        self.window_start = now;
    }
}

struct Shared {
    transport: Box<dyn ScreenTransport>,
    wake: Box<dyn Fn() + Send + Sync>,
    io: Mutex<Io>,
    io_cv: Condvar,
    view_only: AtomicBool,
    closed: AtomicBool,
    fb_width: AtomicU32,
    fb_height: AtomicU32,
    ext_clipboard: AtomicBool,
    state: Mutex<ScreenState>,
    slot: Mutex<FrameSlot>,
    next_frame_id: AtomicU64,
    clipboard_in: Mutex<Option<String>>,
    clipboard_out: Mutex<Option<String>>,
    stats: Mutex<StatsInner>,
}

impl Shared {
    fn new(
        transport: Box<dyn ScreenTransport>,
        wake: Box<dyn Fn() + Send + Sync>,
        options: ScreenOptions,
    ) -> Self {
        Self {
            transport,
            wake,
            io: Mutex::new(Io {
                buttons: 0,
                pos: (u16::MAX, u16::MAX),
                pending_move: false,
                last_move: None,
                pressed: Vec::new(),
                last_wake: None,
                wake_pending: false,
            }),
            io_cv: Condvar::new(),
            view_only: AtomicBool::new(options.view_only),
            closed: AtomicBool::new(false),
            fb_width: AtomicU32::new(0),
            fb_height: AtomicU32::new(0),
            ext_clipboard: AtomicBool::new(false),
            state: Mutex::new(ScreenState::Connecting),
            slot: Mutex::new(FrameSlot {
                frame: None,
                taken: true,
                dropped: 0,
            }),
            next_frame_id: AtomicU64::new(1),
            clipboard_in: Mutex::new(None),
            clipboard_out: Mutex::new(None),
            stats: Mutex::new(StatsInner {
                window_start: Instant::now(),
                frames: 0,
                bytes: 0,
                fps: 0.0,
                kbps: 0.0,
                codec: String::new(),
                decode_ms: 0.0,
            }),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Marks the session closed. `false` when it already was.
    fn shutdown(&self, reason: String) -> bool {
        if self.closed.swap(true, Ordering::SeqCst) {
            return false;
        }
        *lock(&self.state) = ScreenState::Closed(reason);
        self.transport.close();
        // Taking the lock orders this with a timer thread that is about to wait.
        drop(lock(&self.io));
        self.io_cv.notify_all();
        true
    }

    fn close(&self) {
        self.shutdown(String::new());
    }

    /// Closes with a reason and tells the UI right away.
    fn fail(&self, reason: impl Into<String>) {
        if self.shutdown(reason.into()) {
            (self.wake)();
        }
    }

    /// Runs `action` with the write lock held; a failed write closes the session.
    fn with_io(&self, action: impl FnOnce(&mut Io) -> Result<(), String>) {
        if self.is_closed() {
            return;
        }
        let result = action(&mut lock(&self.io));
        if let Err(reason) = result {
            self.fail(reason);
        }
    }

    /// Sends bytes in order with everything else.
    fn send(&self, bytes: &[u8]) -> bool {
        self.with_io(|_| self.transport.send(bytes));
        !self.is_closed()
    }

    fn request_wake(&self) {
        let now = Instant::now();
        let call = {
            let mut io = lock(&self.io);
            if io
                .last_wake
                .is_none_or(|last| now.saturating_duration_since(last) >= WAKE_INTERVAL)
            {
                io.last_wake = Some(now);
                io.wake_pending = false;
                true
            } else {
                if !io.wake_pending {
                    io.wake_pending = true;
                    self.io_cv.notify_all();
                }
                false
            }
        };
        if call {
            (self.wake)();
        }
    }

    fn publish(&self, mut frame: Frame) {
        frame.id = self.next_frame_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut slot = lock(&self.slot);
            if slot.frame.is_some() && !slot.taken {
                slot.dropped += 1;
            }
            slot.frame = Some(frame);
            slot.taken = false;
        }
        {
            let mut stats = lock(&self.stats);
            stats.roll(Instant::now());
            stats.frames = stats.frames.saturating_add(1);
        }
        self.request_wake();
    }

    fn publish_pixels(&self, width: usize, height: usize, pixels: &[u8]) {
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            return;
        };
        self.publish(Frame {
            id: 0,
            width,
            height,
            pixels: Pixels::Bgra(Arc::from(pixels)),
        });
    }

    fn set_size(&self, width: usize, height: usize) -> (u32, u32) {
        let width = u32::try_from(width).unwrap_or(u32::MAX);
        let height = u32::try_from(height).unwrap_or(u32::MAX);
        self.fb_width.store(width, Ordering::Relaxed);
        self.fb_height.store(height, Ordering::Relaxed);
        (width, height)
    }

    fn set_ready(&self, width: usize, height: usize, name: String) {
        let (width, height) = self.set_size(width, height);
        let mut state = lock(&self.state);
        if !matches!(*state, ScreenState::Closed(_)) {
            *state = ScreenState::Ready(ScreenInfo {
                width,
                height,
                name,
            });
        }
    }

    fn set_resized(&self, width: usize, height: usize) {
        let (width, height) = self.set_size(width, height);
        if let ScreenState::Ready(info) = &mut *lock(&self.state) {
            info.width = width;
            info.height = height;
        }
    }

    fn set_codec(&self, codec: &str) {
        let mut stats = lock(&self.stats);
        if stats.codec != codec {
            codec.clone_into(&mut stats.codec);
        }
    }

    fn add_received(&self, bytes: usize) {
        let mut stats = lock(&self.stats);
        stats.roll(Instant::now());
        stats.bytes = stats
            .bytes
            .saturating_add(u32::try_from(bytes).unwrap_or(u32::MAX));
    }

    fn set_decode_time(&self, elapsed: Duration) {
        lock(&self.stats).decode_ms = elapsed.as_secs_f32() * 1000.0;
    }

    /// Clamps to the framebuffer; `None` before the screen is known.
    fn clamp(&self, x: i32, y: i32) -> Option<(u16, u16)> {
        let width = self.fb_width.load(Ordering::Relaxed);
        let height = self.fb_height.load(Ordering::Relaxed);
        if width == 0 || height == 0 {
            return None;
        }
        let max_x = i32::try_from(width - 1).unwrap_or(i32::MAX);
        let max_y = i32::try_from(height - 1).unwrap_or(i32::MAX);
        Some((
            u16::try_from(x.clamp(0, max_x)).unwrap_or(u16::MAX),
            u16::try_from(y.clamp(0, max_y)).unwrap_or(u16::MAX),
        ))
    }

    /// Appends the unsent pointer move, if any, to `bytes`.
    fn take_pending_move(io: &mut Io, bytes: &mut Vec<u8>) {
        if io.pending_move {
            io.pending_move = false;
            io.last_move = Some(Instant::now());
            msg::pointer_event(bytes, io.buttons, io.pos.0, io.pos.1);
        }
    }

    fn pointer(&self, x: i32, y: i32, mask: u8) {
        let Some((x, y)) = self.clamp(x, y) else {
            return;
        };
        self.with_io(|io| {
            let moved = io.pos != (x, y);
            io.pos = (x, y);
            let now = Instant::now();
            if mask != io.buttons {
                io.buttons = mask;
                io.pending_move = false;
                io.last_move = Some(now);
                let mut bytes = Vec::with_capacity(6);
                msg::pointer_event(&mut bytes, mask, x, y);
                return self.transport.send(&bytes);
            }
            if !moved {
                return Ok(());
            }
            let due = io
                .last_move
                .is_none_or(|last| now.saturating_duration_since(last) >= MOVE_INTERVAL);
            if due && !io.pending_move {
                io.last_move = Some(now);
                let mut bytes = Vec::with_capacity(6);
                msg::pointer_event(&mut bytes, mask, x, y);
                self.transport.send(&bytes)
            } else {
                io.pending_move = true;
                self.io_cv.notify_all();
                Ok(())
            }
        });
    }

    fn scroll(&self, x: i32, y: i32, dx: i32, dy: i32) {
        let Some((x, y)) = self.clamp(x, y) else {
            return;
        };
        let vertical = (dy.unsigned_abs().min(MAX_WHEEL_NOTCHES), dy > 0);
        let horizontal = (dx.unsigned_abs().min(MAX_WHEEL_NOTCHES), dx > 0);
        if vertical.0 == 0 && horizontal.0 == 0 {
            return;
        }
        self.with_io(|io| {
            io.pos = (x, y);
            io.pending_move = false;
            io.last_move = Some(Instant::now());
            let mut bytes = Vec::new();
            let wheels = [
                (vertical, WHEEL_UP, WHEEL_DOWN),
                (horizontal, WHEEL_RIGHT, WHEEL_LEFT),
            ];
            for ((notches, positive), up, down) in wheels {
                let wheel = if positive { up } else { down };
                for _ in 0..notches {
                    msg::pointer_event(&mut bytes, io.buttons | wheel, x, y);
                    msg::pointer_event(&mut bytes, io.buttons, x, y);
                }
            }
            self.transport.send(&bytes)
        });
    }

    fn key(&self, keysym: u32, down: bool) {
        self.with_io(|io| {
            let mut bytes = Vec::with_capacity(16);
            Self::take_pending_move(io, &mut bytes);
            msg::key_event(&mut bytes, down, keysym);
            if down {
                if !io.pressed.contains(&keysym) && io.pressed.len() < MAX_PRESSED_KEYS {
                    io.pressed.push(keysym);
                }
            } else {
                io.pressed.retain(|&pressed| pressed != keysym);
            }
            self.transport.send(&bytes)
        });
    }

    fn release_all(&self) {
        self.with_io(|io| {
            let mut bytes = Vec::new();
            Self::take_pending_move(io, &mut bytes);
            for keysym in io.pressed.drain(..) {
                msg::key_event(&mut bytes, false, keysym);
            }
            if io.buttons != 0 {
                io.buttons = 0;
                msg::pointer_event(&mut bytes, 0, io.pos.0, io.pos.1);
            }
            if bytes.is_empty() {
                Ok(())
            } else {
                self.transport.send(&bytes)
            }
        });
    }

    fn send_clipboard(&self, text: &str) {
        if text.is_empty() || text.len() > crate::rfb::MAX_CLIPBOARD {
            return;
        }
        *lock(&self.clipboard_out) = Some(text.to_owned());
        let mut bytes = Vec::new();
        if self.ext_clipboard.load(Ordering::SeqCst) {
            clipboard::notify(&mut bytes, true);
        } else if !msg::cut_text_latin1(&mut bytes, text) {
            return;
        }
        self.send(&bytes);
    }
}

/// Runs `body` on a named thread that holds the shared state.
fn spawn(
    shared: &Arc<Shared>,
    name: &str,
    body: impl FnOnce(&Arc<Shared>) + Send + 'static,
) -> bool {
    let shared = Arc::clone(shared);
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || body(&shared))
        .is_ok()
}

enum Inbox {
    Data(Vec<u8>),
    End,
}

fn read_loop(shared: &Arc<Shared>, sender: &SyncSender<Inbox>) {
    while !shared.is_closed() {
        let Some(chunk) = shared.transport.recv() else {
            // The processor may already be gone.
            let _ = sender.send(Inbox::End);
            return;
        };
        if sender.send(Inbox::Data(chunk)).is_err() {
            return;
        }
    }
}

/// Deferred framebuffer publishing.
#[derive(Default)]
struct Publisher {
    last: Option<Instant>,
    dirty: bool,
}

impl Publisher {
    /// How long until a pending publish is due; `None` when nothing is pending.
    fn wait(&self) -> Option<Duration> {
        if !self.dirty {
            return None;
        }
        Some(self.last.map_or(Duration::ZERO, |last| {
            PUBLISH_INTERVAL.saturating_sub(last.elapsed())
        }))
    }

    fn due(&self) -> bool {
        self.last
            .is_none_or(|last| last.elapsed() >= PUBLISH_INTERVAL)
    }

    fn publish(&mut self, shared: &Shared, machine: &Machine) {
        let (width, height) = machine.size();
        shared.publish_pixels(width, height, machine.pixels());
        self.last = Some(Instant::now());
        self.dirty = false;
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the processor thread owns the machine and its decoder until it ends"
)]
fn process_loop(shared: &Arc<Shared>, receiver: &Receiver<Inbox>, mut machine: Machine) {
    let h264_codec = machine
        .h264_name()
        .map_or_else(|| "h264".to_owned(), |name| format!("h264 ({name})"));
    let mut publisher = Publisher::default();
    loop {
        let next = match publisher.wait() {
            Some(wait) => receiver.recv_timeout(wait),
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        if shared.is_closed() {
            return;
        }
        match next {
            Ok(Inbox::Data(chunk)) => {
                if !handle_chunk(shared, &mut machine, &mut publisher, &h264_codec, &chunk) {
                    return;
                }
            }
            Ok(Inbox::End) | Err(RecvTimeoutError::Disconnected) => {
                shared.fail(LOST_REASON);
                return;
            }
            Err(RecvTimeoutError::Timeout) => publisher.publish(shared, &machine),
        }
    }
}

/// Feeds one received chunk through the state machine and acts on the result.
/// `false` ends the session.
fn handle_chunk(
    shared: &Shared,
    machine: &mut Machine,
    publisher: &mut Publisher,
    h264_codec: &str,
    chunk: &[u8],
) -> bool {
    shared.add_received(chunk.len());
    let started = Instant::now();
    let result = machine.feed(chunk);
    let decode_time = started.elapsed();
    let mut reply = machine.take_out();
    let mut changed = false;
    for event in machine.take_events() {
        match event {
            Event::Ready {
                width,
                height,
                name,
            } => {
                shared.set_ready(width, height, name);
                changed = true;
            }
            Event::Resized { width, height } => {
                shared.set_resized(width, height);
                changed = true;
            }
            Event::Update { codec, dirty } => {
                shared.set_decode_time(decode_time);
                match codec {
                    Some(Codec::Raw) => shared.set_codec("raw"),
                    Some(Codec::Zrle) => shared.set_codec("zrle"),
                    Some(Codec::H264) => shared.set_codec(h264_codec),
                    None => {}
                }
                publisher.dirty |= dirty;
            }
            Event::Clipboard(text) => {
                *lock(&shared.clipboard_in) = Some(text);
                changed = true;
            }
            Event::ExtClipboard(usable) => shared.ext_clipboard.store(usable, Ordering::SeqCst),
            Event::ClipboardRequest => {
                let text = lock(&shared.clipboard_out).clone();
                if let Some(text) = text {
                    clipboard::provide(&mut reply, &text);
                }
            }
            Event::ClipboardPeek => {
                let has_text = lock(&shared.clipboard_out).is_some();
                clipboard::notify(&mut reply, has_text);
            }
        }
    }
    if publisher.dirty && publisher.due() {
        publisher.publish(shared, machine);
    }
    if !reply.is_empty() && !shared.send(&reply) {
        return false;
    }
    if let Err(error) = result {
        shared.fail(error.to_string());
        return false;
    }
    shared.transport.processed(chunk.len());
    if changed {
        shared.request_wake();
    }
    true
}

/// Flushes coalesced pointer moves and delayed wake-ups when they fall due.
fn timer_loop(shared: &Arc<Shared>) {
    let mut io = lock(&shared.io);
    loop {
        if shared.is_closed() {
            return;
        }
        let now = Instant::now();
        let move_due = io
            .pending_move
            .then(|| io.last_move.map_or(now, |last| last + MOVE_INTERVAL));
        let wake_due = io
            .wake_pending
            .then(|| io.last_wake.map_or(now, |last| last + WAKE_INTERVAL));
        let due = match (move_due, wake_due) {
            (Some(first), Some(second)) => Some(first.min(second)),
            (first, None) => first,
            (None, second) => second,
        };
        let Some(due) = due else {
            io = shared
                .io_cv
                .wait(io)
                .unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        if due > now {
            io = shared
                .io_cv
                .wait_timeout(io, due - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        }
        let mut failure = None;
        if move_due.is_some_and(|time| time <= now) {
            let mut bytes = Vec::with_capacity(6);
            Shared::take_pending_move(&mut io, &mut bytes);
            if !bytes.is_empty() {
                failure = shared.transport.send(&bytes).err();
            }
        }
        let call_wake = wake_due.is_some_and(|time| time <= now);
        if call_wake {
            io.wake_pending = false;
            io.last_wake = Some(now);
        }
        drop(io);
        if let Some(reason) = failure {
            shared.fail(reason);
        }
        if call_wake {
            (shared.wake)();
        }
        io = lock(&shared.io);
    }
}
