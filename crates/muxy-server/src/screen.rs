//! Desktop mirroring: relays the RFB (VNC) conversation between a viewer on a
//! client's connection and a capture program that this server owns.
//!
//! The capture program is `wayvnc` on a Wayland desktop. It is started when
//! the first viewer opens a screen and stopped [`GRACE`] after the last one
//! leaves. It only ever listens on a socket inside a private folder of the
//! user's runtime directory, or, for an older `wayvnc` that cannot, on
//! 127.0.0.1; never on another address. No VNC password is used: the viewer
//! reached this server through an authenticated connection (a local socket or
//! ssh), and that is the whole access control.
//!
//! Bounded buffering lives in the connection's outbox: a viewer's bytes wait
//! there only up to a window the client has not yet reported handled, and the
//! relay thread blocks beyond it, so `wayvnc` backs up into its own socket and
//! sends only the newest picture once the client catches up.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use muxy_protocol::ErrorCode;

use crate::ServerError;

mod wayvnc;

/// How long the capture program keeps running with nobody watching, so that a
/// viewer who reconnects does not pay for starting it again.
const GRACE: Duration = Duration::from_secs(15);

/// The size of one read from the capture program, and so of one `ScreenData` message.
pub(crate) const CHUNK: usize = 16 * 1024;

/// Writes the client sent that the capture program has not yet read. A viewer
/// that fills this is not reading its input events at all and is closed.
const INPUT_QUEUE: usize = 256;

/// Whether this server offers screen mirroring at all.
pub(crate) fn offered() -> bool {
    cfg!(target_os = "linux") && !disabled()
}

fn disabled() -> bool {
    std::env::var_os("ATUR_TERM_SCREEN").is_some_and(|value| value == "0")
}

/// Where a capture program accepts viewers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Endpoint {
    Unix(std::path::PathBuf),
    Loopback(u16),
}

/// A capture program the hub starts and stops.
pub(crate) trait Capture: Send + Sync + 'static {
    /// Starts it when it is not running and says where it listens.
    fn start(&self) -> Result<Endpoint, ServerError>;
    /// Whether it is still running.
    fn alive(&self) -> bool;
    /// Stops it and removes what it left behind.
    fn stop(&self);
    /// A viewer saw the stream end before any data came: the way it was
    /// started does not work here, so the next start should try another.
    fn failed_to_serve(&self);
}

/// Where a viewer's bytes go. The outbox of a connection implements it.
pub(crate) trait Downstream: Send + Sync + 'static {
    /// Hands bytes to the client; blocks while the client is behind. `false`
    /// means the stream is gone and no more should be pushed.
    fn push(&self, bytes: Vec<u8>) -> bool;
    /// The stream ended after everything pushed. Empty `reason` is a normal end.
    fn finish(&self, reason: String);
}

enum Stream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        match endpoint {
            Endpoint::Unix(path) => UnixStream::connect(path).map(Self::Unix),
            Endpoint::Loopback(port) => {
                let stream = TcpStream::connect(("127.0.0.1", *port))?;
                stream.set_nodelay(true)?;
                Ok(Self::Tcp(stream))
            }
        }
    }

    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
        }
    }

    fn shutdown(&self) {
        let _ = match self {
            Self::Unix(stream) => stream.shutdown(Shutdown::Both),
            Self::Tcp(stream) => stream.shutdown(Shutdown::Both),
        };
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buffer),
            Self::Tcp(stream) => stream.read(buffer),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buffer),
            Self::Tcp(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

#[derive(Default)]
struct State {
    viewers: usize,
    /// Changes whenever a viewer arrives, so a pending stop knows it is stale.
    generation: u64,
}

/// Starts the capture for the first viewer and stops it after the last.
pub(crate) struct Hub {
    capture: Box<dyn Capture>,
    grace: Duration,
    state: Mutex<State>,
    /// Serialises starting, which can take seconds.
    starting: Mutex<()>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Hub").finish_non_exhaustive()
    }
}

/// The server's one hub, driving `wayvnc`.
pub(crate) fn hub() -> &'static Arc<Hub> {
    static HUB: LazyLock<Arc<Hub>> = LazyLock::new(|| {
        Arc::new(Hub::new(
            Box::new(wayvnc::Wayvnc::from_environment()),
            GRACE,
        ))
    });
    &HUB
}

/// Stops the capture program. The server calls it as it shuts down.
pub(crate) fn stop_all() {
    hub().capture.stop();
}

/// A viewer's link to the capture program. Dropping it ends the viewer.
pub(crate) struct Viewer {
    input: SyncSender<Vec<u8>>,
    socket: Stream,
    /// Tells the relay that this side ended the viewer, so it has nothing to report.
    local_close: Arc<AtomicBool>,
}

impl std::fmt::Debug for Viewer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Viewer").finish_non_exhaustive()
    }
}

impl Viewer {
    /// Queues the client's bytes for the capture program. `false` when it is
    /// not reading them, in which case the viewer should be closed.
    pub(crate) fn write(&self, bytes: Vec<u8>) -> bool {
        match self.input.try_send(bytes) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.local_close.store(true, Ordering::Release);
        self.socket.shutdown();
    }
}

/// Counts a viewer in; counts it out, and schedules the stop, when dropped.
struct Lease {
    hub: Arc<Hub>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.hub.leave();
    }
}

impl Hub {
    pub(crate) fn new(capture: Box<dyn Capture>, grace: Duration) -> Self {
        Self {
            capture,
            grace,
            state: Mutex::new(State::default()),
            starting: Mutex::new(()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Connects a viewer, starting the capture when it is not running.
    /// Everything the capture sends goes to `downstream` from a thread of its own.
    pub(crate) fn connect(
        self: &Arc<Self>,
        downstream: Arc<dyn Downstream>,
    ) -> Result<Viewer, ServerError> {
        if disabled() {
            return Err(unsupported(
                "Screen mirroring is turned off on this computer.",
            ));
        }
        // Counted before starting, so a stop that is already pending cannot
        // take the capture away from this viewer.
        let lease = self.join();
        let stream = {
            let _starting = self.starting.lock().unwrap_or_else(PoisonError::into_inner);
            self.dial()?
        };
        let reader = stream.try_clone().map_err(ServerError::spawn_failed)?;
        let mut writer = stream.try_clone().map_err(ServerError::spawn_failed)?;
        let (input, queued) = mpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE);
        thread::Builder::new()
            .name("atur-screen-in".into())
            .spawn(move || {
                for bytes in queued {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                }
                writer.shutdown();
            })
            .map_err(ServerError::spawn_failed)?;
        let local_close = Arc::new(AtomicBool::new(false));
        let relayed = Relay {
            socket: reader,
            downstream,
            hub: Arc::clone(self),
            lease,
            local_close: Arc::clone(&local_close),
        };
        thread::Builder::new()
            .name("atur-screen-out".into())
            .spawn(move || relayed.run())
            .map_err(ServerError::spawn_failed)?;
        Ok(Viewer {
            input,
            socket: stream,
            local_close,
        })
    }

    /// Connects to the capture, starting it, or starting it again, when needed.
    fn dial(&self) -> Result<Stream, ServerError> {
        let endpoint = self.capture.start()?;
        if let Ok(stream) = Stream::connect(&endpoint) {
            return Ok(stream);
        }
        // It stopped between the check and the connection, or never listened.
        self.capture.stop();
        let endpoint = self.capture.start()?;
        Stream::connect(&endpoint).map_err(|error| {
            self.capture.stop();
            ServerError::new(
                ErrorCode::SpawnFailed,
                format!("The screen sharing program did not accept the connection: {error}"),
            )
        })
    }

    fn join(self: &Arc<Self>) -> Lease {
        let mut state = self.state();
        state.viewers += 1;
        state.generation += 1;
        Lease {
            hub: Arc::clone(self),
        }
    }

    fn leave(self: &Arc<Self>) {
        let generation = {
            let mut state = self.state();
            state.viewers = state.viewers.saturating_sub(1);
            if state.viewers > 0 {
                return;
            }
            state.generation += 1;
            state.generation
        };
        let hub = Arc::clone(self);
        let grace = self.grace;
        let scheduled = thread::Builder::new()
            .name("atur-screen-stop".into())
            .spawn(move || {
                thread::sleep(grace);
                let stale = {
                    let state = hub.state();
                    state.viewers > 0 || state.generation != generation
                };
                if !stale {
                    log::info!("no screen viewers left; stopping the screen capture");
                    hub.capture.stop();
                }
            });
        if scheduled.is_err() {
            self.capture.stop();
        }
    }
}

fn unsupported(message: &str) -> ServerError {
    ServerError::new(ErrorCode::Unsupported, message)
}

struct Relay {
    socket: Stream,
    downstream: Arc<dyn Downstream>,
    hub: Arc<Hub>,
    lease: Lease,
    local_close: Arc<AtomicBool>,
}

impl Relay {
    /// Carries the capture's bytes to the client until either side ends.
    fn run(mut self) {
        let mut buffer = vec![0; CHUNK];
        let mut received = 0_usize;
        loop {
            match self.socket.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    received += count;
                    if !self.downstream.push(buffer[..count].to_vec()) {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    log::debug!("screen capture read ended: {error}");
                    break;
                }
            }
        }
        self.socket.shutdown();
        if !self.local_close.load(Ordering::Acquire) {
            if received == 0 {
                self.hub.capture.failed_to_serve();
                self.downstream.finish(
                    "The screen sharing program closed the connection before sending anything. \
                     Try again."
                        .into(),
                );
            } else {
                self.downstream
                    .finish("The screen sharing program stopped.".into());
            }
        }
        drop(self.lease);
    }
}

#[cfg(test)]
mod tests;
