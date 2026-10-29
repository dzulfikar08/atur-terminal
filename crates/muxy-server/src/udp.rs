//! UDP streams for paired devices, offered over their authenticated connection.
//!
//! A phone has no ssh to carry a secret the way the stdio bridge does, but it
//! already holds a pinned TLS connection that authenticated it. `OfferUdp` on
//! that connection binds a port from the configured range, answers with the
//! port and a fresh secret, and waits for the device to contact it. The stream
//! that results is served exactly like an accepted TLS connection: Hello, then
//! the device's credential, with the same admission limit and the same
//! revocation, because it goes through the same `serve_remote`. The secret is
//! an extra layer; it never replaces the credential.

use std::io;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use muxy_protocol::transport::ByteStream;
use muxy_protocol::transport::udp::{
    Options, Secret, UdpServer, UdpStatus, UdpStream, parse_ports,
};
use muxy_protocol::{DeviceId, ErrorCode, UdpOffer};

use crate::connection::Outbox;
use crate::{Admission, Registry, ServerError};

/// An offer nobody contacts within this long is closed.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(30);
/// How often an unclaimed offer checks whether the connection that asked is gone.
const POLL: Duration = Duration::from_millis(250);
/// A device that stops sending for this long loses its UDP connection. The
/// phone sends a heartbeat every two seconds, and closes its connection itself
/// when the app leaves the foreground.
const DEVICE_IDLE: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING: usize = 16;
const MAX_PENDING_PER_DEVICE: usize = 2;
const DEFAULT_PORTS: RangeInclusive<u16> = 60001..=61000;

/// Serves a claimed stream as a network connection, the way the executable
/// serves a stream its TLS listener accepted.
pub type ServeStream =
    dyn Fn(Box<dyn ByteStream>, Arc<Registry>, Admission) -> io::Result<()> + Send + Sync;

/// What a server needs to answer `OfferUdp`.
pub struct UdpOffers {
    ports: RangeInclusive<u16>,
    idle: Duration,
    serve: Arc<ServeStream>,
    pending: Arc<Mutex<Vec<DeviceId>>>,
}

impl std::fmt::Debug for UdpOffers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UdpOffers")
            .field("ports", &self.ports)
            .finish_non_exhaustive()
    }
}

impl UdpOffers {
    pub fn new(
        ports: RangeInclusive<u16>,
        serve: impl Fn(Box<dyn ByteStream>, Arc<Registry>, Admission) -> io::Result<()>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            ports,
            idle: DEVICE_IDLE,
            serve: Arc::new(serve),
            pending: Arc::default(),
        }
    }

    /// Offers on `ATUR_TERM_UDP_PORTS` (`LOW-HIGH`, default 60001-61000) and
    /// keeps a quiet device for `ATUR_TERM_UDP_IDLE` seconds; `None` when
    /// `ATUR_TERM_UDP` is `0`, `off`, `false` or `no`.
    pub fn from_environment(
        serve: impl Fn(Box<dyn ByteStream>, Arc<Registry>, Admission) -> io::Result<()>
        + Send
        + Sync
        + 'static,
    ) -> Option<Self> {
        let off = muxy_core::env::var("ATUR_TERM_UDP").is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no"
            )
        });
        if off {
            return None;
        }
        let ports = muxy_core::env::var("ATUR_TERM_UDP_PORTS")
            .and_then(|text| parse_ports(&text))
            .unwrap_or(DEFAULT_PORTS);
        let mut offers = Self::new(ports, serve);
        if let Some(seconds) = muxy_core::env::var("ATUR_TERM_UDP_IDLE")
            .and_then(|text| text.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
        {
            offers.idle = Duration::from_secs(seconds);
        }
        Some(offers)
    }

    /// Binds a port for the device behind `requester` and waits for it on a
    /// thread of its own.
    pub(crate) fn offer(
        &self,
        registry: &Arc<Registry>,
        requester: &Arc<Outbox>,
    ) -> Result<UdpOffer, ServerError> {
        let device = requester.device().ok_or_else(|| {
            ServerError::new(ErrorCode::Unauthorized, "only paired devices get UDP")
        })?;
        let slot = Slot::take(&self.pending, device)?;
        let secret = Secret::generate().map_err(ServerError::spawn_failed)?;
        let server = UdpServer::bind(
            self.ports.clone(),
            &secret,
            Options {
                idle_timeout: self.idle,
                claim_timeout: CLAIM_TIMEOUT,
            },
        )
        .map_err(|error| {
            ServerError::new(
                ErrorCode::BadRequest,
                format!(
                    "no UDP port available in {}-{}: {error}",
                    self.ports.start(),
                    self.ports.end()
                ),
            )
        })?;
        let port = server.port();
        let waiting = Waiting {
            status: server.status(),
            stream: server.into_stream(),
            requester: Arc::clone(requester),
            registry: Arc::clone(registry),
            serve: Arc::clone(&self.serve),
            slot,
        };
        thread::Builder::new()
            .name("udp-offer".into())
            .spawn(move || waiting.run())
            .map_err(ServerError::spawn_failed)?;
        Ok(UdpOffer {
            port,
            key: *secret.as_bytes(),
        })
    }
}

/// One of the few unclaimed offers a device or the server may have at a time.
struct Slot {
    pending: Arc<Mutex<Vec<DeviceId>>>,
    device: DeviceId,
}

impl Slot {
    fn take(pending: &Arc<Mutex<Vec<DeviceId>>>, device: DeviceId) -> Result<Self, ServerError> {
        let mut taken = pending.lock().unwrap_or_else(PoisonError::into_inner);
        if taken.len() >= MAX_PENDING
            || taken.iter().filter(|other| **other == device).count() >= MAX_PENDING_PER_DEVICE
        {
            return Err(ServerError::new(
                ErrorCode::BadRequest,
                "too many UDP offers are waiting",
            ));
        }
        taken.push(device);
        Ok(Self {
            pending: Arc::clone(pending),
            device,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut taken = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(index) = taken.iter().position(|device| *device == self.device) {
            taken.swap_remove(index);
        }
    }
}

struct Waiting {
    status: UdpStatus,
    stream: UdpStream,
    requester: Arc<Outbox>,
    registry: Arc<Registry>,
    serve: Arc<ServeStream>,
    slot: Slot,
}

impl Waiting {
    fn run(self) {
        let Self {
            status,
            stream,
            requester,
            registry,
            serve,
            slot,
        } = self;
        let deadline = Instant::now() + CLAIM_TIMEOUT;
        while !status.claimed() {
            let left = deadline.saturating_duration_since(Instant::now());
            // Dropping the stream closes the port.
            if left.is_zero() || status.closed() || requester.is_closed() {
                return;
            }
            status.wait_claimed(left.min(POLL));
        }
        drop(slot);
        // Too many unauthenticated peers: dropping the stream refuses this one.
        let Some(admission) = registry.admit_remote() else {
            return;
        };
        if let Err(error) = serve(Box::new(stream), registry, admission) {
            log::error!("mobile access UDP connection failed: {error}");
        }
    }
}
