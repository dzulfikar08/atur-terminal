//! Opening a screen stream on a connection.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use muxy_protocol::{ChannelId, ErrorCode};

use crate::ServerError;
use crate::screen::{self, Downstream};

use super::outbox::Outbox;

/// Hands the capture's bytes to a connection's outbox.
struct ToOutbox {
    outbox: Arc<Outbox>,
    channel: ChannelId,
}

impl Downstream for ToOutbox {
    fn push(&self, bytes: Vec<u8>) -> bool {
        self.outbox.push_screen(self.channel, bytes)
    }

    fn finish(&self, reason: String) {
        self.outbox.finish_screen(self.channel, reason);
    }
}

/// Starts mirroring the desktop on a new channel of `outbox`.
pub(super) fn open(
    outbox: &Arc<Outbox>,
    last_channel: &AtomicU32,
) -> Result<ChannelId, ServerError> {
    if !screen::offered() {
        return Err(ServerError::new(
            ErrorCode::Unsupported,
            "Screen mirroring needs a Linux computer with a Wayland desktop.",
        ));
    }
    let channel = ChannelId(
        last_channel
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |last| {
                last.checked_add(1)
            })
            .map_err(|_| ServerError::new(ErrorCode::BadRequest, "channel IDs exhausted"))?
            + 1,
    );
    if !outbox.reserve_screen(channel) {
        return Err(ServerError::new(
            ErrorCode::BadRequest,
            "the connection is closing",
        ));
    }
    let downstream = Arc::new(ToOutbox {
        outbox: Arc::clone(outbox),
        channel,
    });
    match screen::hub().connect(downstream) {
        Ok(viewer) => {
            outbox.attach_viewer(channel, viewer);
            log::info!(
                "a client started mirroring this desktop on channel {}",
                channel.0
            );
            Ok(channel)
        }
        Err(error) => {
            outbox.release_screen(channel);
            Err(error)
        }
    }
}
