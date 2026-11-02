//! The RFB server: accepts viewers on a Unix socket and serves each one the
//! pictures a capture publishes, taking its input, clipboard and requests back.
//!
//! One thread reads a viewer's messages and one writes to it. The writer always
//! takes the newest picture, so a viewer on a slow link is simply shown fewer
//! of them and the delay never grows; the relay in front of this program is
//! what slows the writer down, by not reading.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use crate::frame::Frame;
use crate::geometry::Geometry;
use crate::rfb::encode::{Pixels, Zrle, changed_area, raw_update};
use crate::rfb::message::{self, ClientMessage, ClientParser, PixelFormat};
use crate::rfb::{
    ENC_CONTINUOUS_UPDATES, ENC_DESKTOP_SIZE, ENC_EXT_CLIPBOARD, ENC_EXT_DESKTOP_SIZE, ENC_H264,
    ENC_ZRLE, Rect, clipboard,
};

/// How long a writer sleeps when nothing happens before it looks again.
const IDLE: Duration = Duration::from_millis(250);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the Mac's clipboard is looked at.
const CLIPBOARD_POLL: Duration = Duration::from_millis(400);
/// Update requests a viewer may have open at once; more are not remembered.
const MAX_REQUESTS: u32 = 64;
const FENCE_REQUEST: u32 = 1 << 31;

/// Turns pictures into H.264, one access unit per picture.
pub trait Encoder: Send {
    /// The next access unit in Annex B form; a key frame, with the parameter
    /// sets, when `key`. Empty when the encoder produced nothing for this picture.
    ///
    /// # Errors
    /// A sentence for the log when encoding failed; the viewer then falls back
    /// to pixels.
    fn encode(&mut self, frame: &Frame, key: bool) -> Result<Vec<u8>, String>;
}

/// What the server needs from the computer it runs on.
pub trait Host: Send + Sync + 'static {
    /// The desktop's name as shown to the viewer.
    fn name(&self) -> String;
    /// A new H.264 encoder for pictures of this size, or `None` when there is none.
    fn encoder(&self, width: u32, height: u32) -> Option<Box<dyn Encoder>>;
    /// Whether mouse and keyboard events can be carried out.
    fn input_allowed(&self) -> bool;
    /// Where the pointer is, in picture pixels, and which buttons are held.
    fn pointer(&self, buttons: u8, x: u16, y: u16);
    fn key(&self, keysym: u32, down: bool);
    /// Lets go of every key and button, for a viewer that is gone.
    fn release_all(&self);
    fn clipboard_text(&self) -> Option<String>;
    /// Changes whenever the clipboard does.
    fn clipboard_changes(&self) -> u64;
    /// Replaces the clipboard text and returns the new value of
    /// [`Host::clipboard_changes`], so that the change is not announced back.
    fn set_clipboard_text(&self, text: &str) -> u64;
}

/// The newest picture and what else changed, for every viewer's writer.
pub struct Broadcast {
    inner: Mutex<Inner>,
    changed: Condvar,
    geometry: Mutex<Option<Geometry>>,
}

struct Inner {
    frame: Option<Arc<Frame>>,
    frame_seq: u64,
    /// Bumped by anything a writer must look at again: a request, a message to send.
    nudge: u64,
    clipboard: Option<Arc<String>>,
    clipboard_seq: u64,
    size: (u32, u32),
    viewers: usize,
    closed: bool,
}

impl fmt::Debug for Broadcast {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Broadcast").finish_non_exhaustive()
    }
}

/// What a writer has dealt with.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Seen {
    frame: u64,
    nudge: u64,
    clipboard: u64,
}

struct Snapshot {
    frame: Option<Arc<Frame>>,
    seen: Seen,
    clipboard: Option<Arc<String>>,
    closed: bool,
}

impl Broadcast {
    /// A broadcast of pictures that are `size` pixels until a frame says otherwise.
    #[must_use]
    pub fn new(size: (u32, u32)) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                frame: None,
                frame_seq: 0,
                nudge: 0,
                clipboard: None,
                clipboard_seq: 0,
                size,
                viewers: 0,
                closed: false,
            }),
            changed: Condvar::new(),
            geometry: Mutex::new(None),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes `frame` the newest picture.
    pub fn publish(&self, frame: Arc<Frame>) {
        let mut inner = self.lock();
        inner.size = (frame.width, frame.height);
        inner.frame = Some(frame);
        inner.frame_seq += 1;
        drop(inner);
        self.changed.notify_all();
    }

    /// Says what size pictures will have, before the first one arrives.
    pub fn set_size(&self, size: (u32, u32)) {
        self.lock().size = size;
    }

    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.lock().size
    }

    /// Where the shared display is, for pointer events.
    pub fn set_geometry(&self, geometry: Geometry) {
        *self.geometry.lock().unwrap_or_else(PoisonError::into_inner) = Some(geometry);
    }

    #[must_use]
    pub fn geometry(&self) -> Option<Geometry> {
        *self.geometry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Announces new clipboard text to the viewers.
    pub fn set_clipboard(&self, text: String) {
        let mut inner = self.lock();
        inner.clipboard = Some(Arc::new(text));
        inner.clipboard_seq += 1;
        drop(inner);
        self.changed.notify_all();
    }

    /// Ends every viewer.
    pub fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }

    /// How many viewers are connected.
    #[must_use]
    pub fn viewers(&self) -> usize {
        self.lock().viewers
    }

    fn wake(&self) {
        self.lock().nudge += 1;
        self.changed.notify_all();
    }

    /// Waits until something the writer has not dealt with changes: a picture
    /// when `wants_frames`, a nudge, the clipboard, or the end; or `timeout`.
    fn wait(&self, seen: Seen, wants_frames: bool, timeout: Duration) -> Snapshot {
        let guard = self.lock();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |inner| {
                !inner.closed
                    && inner.nudge == seen.nudge
                    && inner.clipboard_seq == seen.clipboard
                    && !(wants_frames && inner.frame_seq != seen.frame)
            })
            .unwrap_or_else(PoisonError::into_inner);
        Snapshot {
            frame: guard.frame.clone(),
            seen: Seen {
                frame: guard.frame_seq,
                nudge: guard.nudge,
                clipboard: guard.clipboard_seq,
            },
            clipboard: guard.clipboard.clone(),
            closed: guard.closed,
        }
    }
}

/// What the viewer asked for with `SetEncodings`.
#[derive(Clone, Copy, Debug, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per encoding a viewer may list"
)]
struct Wanted {
    h264: bool,
    zrle: bool,
    desktop_size: bool,
    ext_desktop_size: bool,
    continuous: bool,
    ext_clipboard: bool,
}

impl Wanted {
    fn from_list(list: &[i32]) -> Self {
        let has = |encoding: i32| list.contains(&encoding);
        Self {
            h264: has(ENC_H264),
            zrle: has(ENC_ZRLE),
            desktop_size: has(ENC_DESKTOP_SIZE),
            ext_desktop_size: has(ENC_EXT_DESKTOP_SIZE),
            continuous: has(ENC_CONTINUOUS_UPDATES),
            ext_clipboard: has(ENC_EXT_CLIPBOARD),
        }
    }
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent switches of one viewer's conversation"
)]
struct ViewerState {
    wanted: Wanted,
    format: PixelFormat,
    /// The viewer switched continuous updates on.
    continuous: bool,
    /// Updates the viewer has asked for and not yet got.
    requests: u32,
    /// A non-incremental request, or a change of encoding, wants the whole picture
    /// and a key frame.
    full: bool,
    /// The viewer answered our clipboard capabilities, so Notify/Request/Provide works.
    clipboard_flow: bool,
    announced_continuous: bool,
    /// Messages for the writer to send between pictures.
    outgoing: VecDeque<Vec<u8>>,
}

struct Viewer {
    state: Mutex<ViewerState>,
    broadcast: Arc<Broadcast>,
    stream: UnixStream,
    gone: AtomicBool,
}

impl Viewer {
    fn state(&self) -> MutexGuard<'_, ViewerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the state and wakes the writer.
    fn update(&self, change: impl FnOnce(&mut ViewerState)) {
        change(&mut self.state());
        self.broadcast.wake();
    }

    fn end(&self) {
        self.gone.store(true, Ordering::Release);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        self.broadcast.wake();
    }
}

/// Settings of the server that do not change.
#[derive(Clone, Copy, Debug)]
struct Config {
    view_only: bool,
}

/// A listening server.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    host: Arc<dyn Host>,
    broadcast: Arc<Broadcast>,
    config: Config,
    stop: Arc<AtomicBool>,
    ignored_clipboard: Arc<AtomicU64>,
}

impl fmt::Debug for Server {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Server")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Stops a running server.
#[derive(Clone)]
pub struct Stopper {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    broadcast: Arc<Broadcast>,
}

impl fmt::Debug for Stopper {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Stopper").finish_non_exhaustive()
    }
}

impl Stopper {
    /// Ends every viewer and the accept loop.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.broadcast.close();
        // Wakes the accept call.
        let _ = UnixStream::connect(&self.path);
    }
}

impl Server {
    /// Listens on `path` (replacing a stale socket there), readable only by this user.
    ///
    /// # Errors
    /// When the socket cannot be created.
    pub fn bind(
        path: &Path,
        host: Arc<dyn Host>,
        broadcast: Arc<Broadcast>,
        view_only: bool,
    ) -> io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            path: path.to_owned(),
            host,
            broadcast,
            config: Config { view_only },
            stop: Arc::new(AtomicBool::new(false)),
            ignored_clipboard: Arc::new(AtomicU64::new(0)),
        })
    }

    #[must_use]
    pub fn stopper(&self) -> Stopper {
        Stopper {
            path: self.path.clone(),
            stop: Arc::clone(&self.stop),
            broadcast: Arc::clone(&self.broadcast),
        }
    }

    /// Accepts viewers until stopped.
    pub fn run(self) {
        self.watch_clipboard();
        for connection in self.listener.incoming() {
            if self.stop.load(Ordering::Acquire) {
                break;
            }
            let Ok(stream) = connection else {
                continue;
            };
            let host = Arc::clone(&self.host);
            let broadcast = Arc::clone(&self.broadcast);
            let ignored = Arc::clone(&self.ignored_clipboard);
            let config = self.config;
            let spawned = thread::Builder::new()
                .name("atur-screen-viewer".into())
                .spawn(move || {
                    if let Err(error) = serve_viewer(stream, &host, &broadcast, &ignored, config) {
                        log::info!("a viewer ended: {error}");
                    }
                });
            if let Err(error) = spawned {
                log::warn!("cannot start a viewer thread: {error}");
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }

    /// Announces changes of the Mac's clipboard that did not come from a viewer.
    fn watch_clipboard(&self) {
        let host = Arc::clone(&self.host);
        let broadcast = Arc::clone(&self.broadcast);
        let ignored = Arc::clone(&self.ignored_clipboard);
        let stop = Arc::clone(&self.stop);
        let spawned = thread::Builder::new()
            .name("atur-screen-clipboard".into())
            .spawn(move || {
                let mut last = host.clipboard_changes();
                while !stop.load(Ordering::Acquire) {
                    thread::sleep(CLIPBOARD_POLL);
                    let now = host.clipboard_changes();
                    if now == last {
                        continue;
                    }
                    last = now;
                    if now == ignored.load(Ordering::Acquire) || broadcast.viewers() == 0 {
                        continue;
                    }
                    if let Some(text) = host.clipboard_text() {
                        broadcast.set_clipboard(text);
                    }
                }
            });
        if let Err(error) = spawned {
            log::warn!("clipboard sharing from this Mac is off: {error}");
        }
    }
}

fn serve_viewer(
    mut stream: UnixStream,
    host: &Arc<dyn Host>,
    broadcast: &Arc<Broadcast>,
    ignored: &Arc<AtomicU64>,
    config: Config,
) -> io::Result<()> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let size = broadcast.size();
    handshake(&mut stream, size, &host.name())?;
    stream.set_read_timeout(None)?;
    let viewer = Arc::new(Viewer {
        state: Mutex::new(ViewerState {
            wanted: Wanted::default(),
            format: PixelFormat::NATIVE,
            continuous: false,
            requests: 0,
            full: true,
            clipboard_flow: false,
            announced_continuous: false,
            outgoing: VecDeque::new(),
        }),
        broadcast: Arc::clone(broadcast),
        stream: stream.try_clone()?,
        gone: AtomicBool::new(false),
    });
    broadcast.lock().viewers += 1;
    let writer = {
        let viewer = Arc::clone(&viewer);
        let host = Arc::clone(host);
        let output = stream.try_clone()?;
        thread::Builder::new()
            .name("atur-screen-writer".into())
            .spawn(move || {
                if let Err(error) = write_loop(&viewer, &host, output, size) {
                    log::info!("writing to a viewer ended: {error}");
                }
                viewer.end();
            })?
    };
    let result = read_loop(&viewer, host, ignored, config, &mut stream);
    viewer.end();
    let _ = writer.join();
    host.release_all();
    broadcast.lock().viewers -= 1;
    result
}

/// RFB 3.7 or 3.8 with security type None, then `ServerInit`.
fn handshake(stream: &mut UnixStream, size: (u32, u32), name: &str) -> io::Result<()> {
    stream.write_all(b"RFB 003.008\n")?;
    let mut version = [0_u8; 12];
    stream.read_exact(&mut version)?;
    let minor = match version.as_slice() {
        b"RFB 003.008\n" => 8,
        b"RFB 003.007\n" => 7,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the viewer does not speak RFB 3.7 or 3.8",
            ));
        }
    };
    // One security type: None.
    stream.write_all(&[1, 1])?;
    let mut choice = [0_u8; 1];
    stream.read_exact(&mut choice)?;
    if choice[0] != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the viewer chose a security type that was not offered",
        ));
    }
    if minor >= 8 {
        stream.write_all(&0_u32.to_be_bytes())?;
    }
    let mut shared = [0_u8; 1];
    stream.read_exact(&mut shared)?;
    let width = u16::try_from(size.0).unwrap_or(u16::MAX);
    let height = u16::try_from(size.1).unwrap_or(u16::MAX);
    stream.write_all(&message::server_init(
        width,
        height,
        PixelFormat::NATIVE,
        name,
    ))
}

fn read_loop(
    viewer: &Arc<Viewer>,
    host: &Arc<dyn Host>,
    ignored: &Arc<AtomicU64>,
    config: Config,
    stream: &mut UnixStream,
) -> io::Result<()> {
    let mut parser = ClientParser::default();
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let messages = parser
            .feed(&buffer[..count])
            .map_err(|reason| io::Error::new(io::ErrorKind::InvalidData, reason))?;
        for message in messages {
            handle(viewer, host, ignored, config, message);
        }
    }
}

fn handle(
    viewer: &Arc<Viewer>,
    host: &Arc<dyn Host>,
    ignored: &Arc<AtomicU64>,
    config: Config,
    received: ClientMessage,
) {
    let input = host.input_allowed() && !config.view_only;
    match received {
        ClientMessage::SetPixelFormat(Some(format)) => viewer.update(|state| {
            state.format = format;
            state.full = true;
        }),
        ClientMessage::SetPixelFormat(None) => {
            log::warn!("a viewer asked for a pixel format this server does not produce");
            viewer.end();
        }
        ClientMessage::SetEncodings(list) => {
            let wanted = Wanted::from_list(&list);
            viewer.update(|state| {
                if wanted.ext_clipboard && !state.wanted.ext_clipboard {
                    state.outgoing.push_back(clipboard::caps());
                }
                if wanted.continuous && !state.announced_continuous {
                    state.announced_continuous = true;
                    state
                        .outgoing
                        .push_back(message::END_OF_CONTINUOUS_UPDATES.to_vec());
                }
                state.wanted = wanted;
                state.full = true;
            });
        }
        ClientMessage::UpdateRequest { incremental } => viewer.update(|state| {
            state.requests = (state.requests + 1).min(MAX_REQUESTS);
            if !incremental {
                state.full = true;
            }
        }),
        ClientMessage::EnableContinuousUpdates { enable } => viewer.update(|state| {
            state.continuous = enable;
            if !enable {
                state
                    .outgoing
                    .push_back(message::END_OF_CONTINUOUS_UPDATES.to_vec());
            }
        }),
        ClientMessage::Key { down, keysym } => {
            if input {
                host.key(keysym, down);
            }
        }
        ClientMessage::Pointer { buttons, x, y } => {
            if input {
                host.pointer(buttons, x, y);
            }
        }
        ClientMessage::CutText(text) => {
            if !config.view_only && !text.is_empty() {
                ignored.store(host.set_clipboard_text(&text), Ordering::Release);
            }
        }
        ClientMessage::ExtClipboard(body) => {
            handle_clipboard(viewer, host, ignored, config, &body);
        }
        ClientMessage::Fence { flags, payload } => {
            if flags & FENCE_REQUEST != 0 {
                viewer.update(|state| {
                    state
                        .outgoing
                        .push_back(message::fence_reply(flags, &payload));
                });
            }
        }
        ClientMessage::Ignored => {}
    }
}

fn handle_clipboard(
    viewer: &Arc<Viewer>,
    host: &Arc<dyn Host>,
    ignored: &Arc<AtomicU64>,
    config: Config,
    body: &[u8],
) {
    match clipboard::parse_incoming(body) {
        clipboard::Incoming::Caps => viewer.update(|state| state.clipboard_flow = true),
        clipboard::Incoming::Request => {
            if let Some(reply) = host
                .clipboard_text()
                .and_then(|text| clipboard::provide(&text))
            {
                viewer.update(|state| state.outgoing.push_back(reply));
            }
        }
        clipboard::Incoming::Peek => {
            let has_text = host.clipboard_text().is_some();
            viewer.update(|state| state.outgoing.push_back(clipboard::notify(has_text)));
        }
        clipboard::Incoming::Notify { text: true } if !config.view_only => {
            viewer.update(|state| state.outgoing.push_back(clipboard::request()));
        }
        clipboard::Incoming::Provide(text) if !config.view_only && !text.is_empty() => {
            ignored.store(host.set_clipboard_text(&text), Ordering::Release);
        }
        clipboard::Incoming::Notify { .. }
        | clipboard::Incoming::Provide(_)
        | clipboard::Incoming::Ignored => {}
    }
}

/// What the writer decided to send for the newest picture.
struct Plan {
    wanted: Wanted,
    format: PixelFormat,
    full: bool,
}

/// One viewer's side of the conversation after the handshake: what it has been
/// sent and the encoder state that goes with it.
struct Writer {
    viewer: Arc<Viewer>,
    host: Arc<dyn Host>,
    stream: UnixStream,
    seen: Seen,
    size: (u32, u32),
    encoder: Option<Box<dyn Encoder>>,
    h264_failed: bool,
    zrle: Zrle,
    /// The last picture sent as pixels, to send only what changed since.
    previous: Option<Arc<Frame>>,
    key_pending: bool,
}

fn write_loop(
    viewer: &Arc<Viewer>,
    host: &Arc<dyn Host>,
    stream: UnixStream,
    size: (u32, u32),
) -> io::Result<()> {
    let mut writer = Writer {
        viewer: Arc::clone(viewer),
        host: Arc::clone(host),
        stream,
        seen: Seen::default(),
        size,
        encoder: None,
        h264_failed: false,
        zrle: Zrle::new(),
        previous: None,
        key_pending: true,
    };
    while !viewer.gone.load(Ordering::Acquire) {
        if !writer.step()? {
            break;
        }
    }
    Ok(())
}

impl Writer {
    /// Waits for something to do and does it. `false` when the server is closing.
    fn step(&mut self) -> io::Result<bool> {
        let wants = {
            let state = self.viewer.state();
            state.continuous || state.requests > 0 || state.full
        };
        let snapshot = self.viewer.broadcast.wait(self.seen, wants, IDLE);
        if snapshot.closed {
            return Ok(false);
        }
        let messages: Vec<Vec<u8>> = self.viewer.state().outgoing.drain(..).collect();
        for bytes in messages {
            self.stream.write_all(&bytes)?;
        }
        if snapshot.seen.clipboard != self.seen.clipboard {
            send_clipboard(&self.viewer, &mut self.stream, snapshot.clipboard.as_ref())?;
        }
        let new_frame = snapshot.seen.frame != self.seen.frame;
        self.seen = snapshot.seen;
        let Some(frame) = snapshot.frame else {
            return Ok(true);
        };
        let Some(plan) = self.plan(new_frame) else {
            return Ok(true);
        };
        self.follow_size(&frame, &plan)?;
        if plan.wanted.h264 && !self.h264_failed && self.encoder.is_none() {
            self.encoder = self.host.encoder(frame.width, frame.height);
            self.h264_failed = self.encoder.is_none();
            if self.h264_failed {
                log::info!("no H.264 encoder: serving pixels instead");
            }
        }
        if plan.wanted.h264 && self.encoder.is_some() {
            self.send_video(&frame, &plan)?;
        } else {
            self.send_pixels(frame, &plan)?;
        }
        Ok(true)
    }

    /// Takes one of the viewer's open requests when a picture is due.
    fn plan(&self, new_frame: bool) -> Option<Plan> {
        let mut state = self.viewer.state();
        let ready = state.continuous || state.requests > 0;
        let full = state.full;
        if !(full || (ready && new_frame)) {
            return None;
        }
        if !state.continuous {
            state.requests = state.requests.saturating_sub(1);
        }
        state.full = false;
        Some(Plan {
            wanted: state.wanted,
            format: state.format,
            full,
        })
    }

    /// Tells the viewer when the picture changed size, and starts over for it.
    fn follow_size(&mut self, frame: &Frame, plan: &Plan) -> io::Result<()> {
        if (frame.width, frame.height) == self.size {
            return Ok(());
        }
        let extended = plan.wanted.ext_desktop_size;
        if !extended && !plan.wanted.desktop_size {
            return Err(io::Error::other(
                "the screen changed size and the viewer cannot follow",
            ));
        }
        let width = usize::try_from(frame.width).unwrap_or(usize::MAX);
        let height = usize::try_from(frame.height).unwrap_or(usize::MAX);
        self.stream
            .write_all(&message::desktop_size(width, height, extended))?;
        self.size = (frame.width, frame.height);
        self.encoder = None;
        self.h264_failed = false;
        self.previous = None;
        self.key_pending = true;
        Ok(())
    }

    fn send_video(&mut self, frame: &Frame, plan: &Plan) -> io::Result<()> {
        let reset = self.key_pending || plan.full;
        let Some(encoder) = self.encoder.as_mut() else {
            return Ok(());
        };
        match encoder.encode(frame, reset) {
            Ok(unit) if unit.is_empty() => {}
            Ok(unit) => {
                let width = usize::try_from(frame.width).unwrap_or(usize::MAX);
                let height = usize::try_from(frame.height).unwrap_or(usize::MAX);
                self.stream
                    .write_all(&message::h264_update(width, height, &unit, reset))?;
                self.key_pending = false;
            }
            Err(reason) => {
                log::warn!("H.264 encoding failed: {reason}");
                self.encoder = None;
                self.h264_failed = true;
                self.key_pending = true;
                // Pixels from here on; the next pass sends the whole picture.
                self.viewer.update(|state| state.full = true);
            }
        }
        Ok(())
    }

    fn send_pixels(&mut self, frame: Arc<Frame>, plan: &Plan) -> io::Result<()> {
        let area = match (&self.previous, plan.full) {
            (Some(old), false) => diff(old, &frame),
            _ => Some(Rect {
                x: 0,
                y: 0,
                w: usize::try_from(frame.width).unwrap_or(0),
                h: usize::try_from(frame.height).unwrap_or(0),
            }),
        };
        let Some(area) = area else {
            return Ok(());
        };
        let bytes = pixel_update(&frame, area, plan, &mut self.zrle)?;
        self.stream.write_all(&bytes)?;
        self.previous = Some(frame);
        Ok(())
    }
}

/// Where two pictures of one size differ; `None` when they do not.
fn diff(old: &Frame, new: &Frame) -> Option<Rect> {
    let width = usize::try_from(new.width).ok()?;
    let height = usize::try_from(new.height).ok()?;
    new.with_bgra(|new_stride, new_bytes| {
        old.with_bgra(|old_stride, old_bytes| {
            changed_area(
                Some(Pixels {
                    stride: old_stride,
                    bytes: old_bytes,
                    width,
                    height,
                }),
                Pixels {
                    stride: new_stride,
                    bytes: new_bytes,
                    width,
                    height,
                },
            )
        })
    })
    .flatten()
    .flatten()
}

/// An update of `area` of `frame` as ZRLE or raw pixels.
fn pixel_update(frame: &Frame, area: Rect, plan: &Plan, zrle: &mut Zrle) -> io::Result<Vec<u8>> {
    let width = usize::try_from(frame.width).unwrap_or(0);
    let height = usize::try_from(frame.height).unwrap_or(0);
    let use_zrle = plan.wanted.zrle && plan.format == PixelFormat::NATIVE;
    frame
        .with_bgra(|stride, bytes| {
            let pixels = Pixels {
                stride,
                bytes,
                width,
                height,
            };
            if use_zrle {
                zrle.update(pixels, area)
            } else {
                raw_update(plan.format, pixels, area)
                    .ok_or_else(|| "the update lies outside the picture".to_owned())
            }
        })
        .unwrap_or_else(|| Err("the picture cannot be read".to_owned()))
        .map_err(io::Error::other)
}

fn send_clipboard(
    viewer: &Viewer,
    stream: &mut UnixStream,
    text: Option<&Arc<String>>,
) -> io::Result<()> {
    let Some(text) = text else {
        return Ok(());
    };
    let (flow, extended) = {
        let state = viewer.state();
        (state.clipboard_flow, state.wanted.ext_clipboard)
    };
    if flow {
        stream.write_all(&clipboard::notify(true))
    } else if !extended {
        match message::server_cut_text(text) {
            Some(bytes) => stream.write_all(&bytes),
            None => Ok(()),
        }
    } else {
        Ok(())
    }
}
