//! A screen stream: raw RFB bytes of a desktop mirror, carried on one channel
//! of the connection.
//!
//! The server never has more than a window of bytes on the wire that the
//! client has not reported handled, so a viewer that falls behind slows the
//! server's source down instead of queueing video here.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use muxy_protocol::{
    CONTROL, ChannelId, ErrorCode, Feature, MAX_SCREEN_CHUNK, Message, ReplyBody, RequestBody,
};

use crate::{Client, ClientError};

/// How long the server may take to start the capture; the first viewer starts
/// the screen sharing program there.
const OPEN_TIMEOUT: Duration = Duration::from_secs(25);

/// What a screen stream delivers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScreenEvent {
    /// The next bytes of the server's side of the conversation: at most 32 KiB, in order.
    Data(Vec<u8>),
    /// The stream ended; the reason is empty when this side closed it.
    Closed(String),
}

/// Where the connection's reader delivers each channel's stream.
#[derive(Default)]
pub(crate) struct ScreenRoutes {
    state: Mutex<RouteState>,
}

#[derive(Default)]
struct RouteState {
    sinks: HashMap<ChannelId, Sender<ScreenEvent>>,
    /// What arrived before its channel was claimed. The server's window bounds it.
    unclaimed: HashMap<ChannelId, Vec<ScreenEvent>>,
    /// Channel numbers are never reused, so late data for these is dropped.
    closed: HashSet<ChannelId>,
}

impl ScreenRoutes {
    fn lock(&self) -> MutexGuard<'_, RouteState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn deliver(&self, channel: ChannelId, event: ScreenEvent) {
        let mut state = self.lock();
        if state.closed.contains(&channel) {
            return;
        }
        let ends = matches!(event, ScreenEvent::Closed(_));
        if let Some(sink) = state.sinks.get(&channel) {
            let _ = sink.send(event);
            if ends {
                state.sinks.remove(&channel);
                state.closed.insert(channel);
            }
        } else {
            state.unclaimed.entry(channel).or_default().push(event);
        }
    }

    fn claim(&self, channel: ChannelId) -> Receiver<ScreenEvent> {
        let (sink, events) = mpsc::channel();
        let mut state = self.lock();
        let mut ended = false;
        for event in state.unclaimed.remove(&channel).unwrap_or_default() {
            ended |= matches!(event, ScreenEvent::Closed(_));
            let _ = sink.send(event);
        }
        if ended {
            state.closed.insert(channel);
        } else {
            state.sinks.insert(channel, sink);
        }
        events
    }

    pub(crate) fn is_released(&self, channel: ChannelId) -> bool {
        self.lock().closed.contains(&channel)
    }

    fn release(&self, channel: ChannelId) {
        let mut state = self.lock();
        state.sinks.remove(&channel);
        state.unclaimed.remove(&channel);
        state.closed.insert(channel);
    }

    /// The connection is gone: every stream ends.
    pub(crate) fn disconnected(&self) {
        let mut state = self.lock();
        let reason = "The connection to the server was lost.";
        for (channel, sink) in std::mem::take(&mut state.sinks) {
            let _ = sink.send(ScreenEvent::Closed(reason.into()));
            state.closed.insert(channel);
        }
        state.unclaimed.clear();
    }
}

/// One open screen stream. Closing it, or dropping it, ends the viewer on the server.
#[derive(Debug)]
pub struct ScreenLink {
    client: Client,
    channel: ChannelId,
    /// Only `recv` locks it, so the other methods work while it waits.
    events: Mutex<Receiver<ScreenEvent>>,
    processed: AtomicU64,
}

impl Client {
    /// Starts mirroring the server's desktop. Needs `Feature::SCREEN`; a server
    /// that cannot capture a desktop answers with an explanation in the error.
    pub fn open_screen(&self) -> Result<ScreenLink, ClientError> {
        if !self.supports(Feature::SCREEN) {
            return Err(ClientError::Invalid(ErrorCode::Unsupported));
        }
        let channel = match self.request_with_timeout(RequestBody::OpenScreen, OPEN_TIMEOUT)? {
            ReplyBody::ScreenOpened(channel) => channel,
            other => return Err(ClientError::UnexpectedReply(Box::new(other))),
        };
        Ok(ScreenLink {
            events: Mutex::new(self.screen_routes().claim(channel)),
            client: self.clone(),
            channel,
            processed: AtomicU64::new(0),
        })
    }
}

impl ScreenLink {
    pub fn channel(&self) -> ChannelId {
        self.channel
    }

    /// Blocks for the next event; `None` when the connection is gone.
    pub fn recv(&self) -> Option<ScreenEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv()
            .ok()
    }

    /// Sends bytes of the client's side of the conversation.
    pub fn send(&self, bytes: &[u8]) -> Result<(), ClientError> {
        for chunk in bytes.chunks(MAX_SCREEN_CHUNK / 2) {
            self.client
                .send_message(self.channel, &Message::ScreenData(chunk.to_vec()))?;
        }
        Ok(())
    }

    /// Reports that `bytes` of received data were handled, which lets the
    /// server send more.
    pub fn processed(&self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let total = self
            .processed
            .fetch_add(bytes, Ordering::AcqRel)
            .saturating_add(bytes);
        let _ = self.client.send_message(
            CONTROL,
            &Message::FrameAck {
                channel: self.channel,
                seq: total,
            },
        );
    }

    /// Ends the viewer. Later calls and the drop do nothing more.
    pub fn close(&self) {
        let routes = self.client.screen_routes();
        if routes.is_released(self.channel) {
            return;
        }
        routes.release(self.channel);
        self.client
            .request_async(
                RequestBody::Detach(self.channel),
                Duration::from_secs(5),
                |body| match body {
                    ReplyBody::Detached => Ok(()),
                    body => Err(ClientError::UnexpectedReply(Box::new(body))),
                },
            )
            .on_complete(|_| ());
    }
}

impl Drop for ScreenLink {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(bytes: &[u8]) -> ScreenEvent {
        ScreenEvent::Data(bytes.to_vec())
    }

    #[test]
    fn bytes_that_arrive_before_the_channel_is_claimed_are_kept_in_order() {
        let routes = ScreenRoutes::default();
        let channel = ChannelId(3);
        routes.deliver(channel, data(b"one"));
        routes.deliver(channel, data(b"two"));
        let events = routes.claim(channel);
        routes.deliver(channel, data(b"three"));
        assert_eq!(events.try_recv(), Ok(data(b"one")));
        assert_eq!(events.try_recv(), Ok(data(b"two")));
        assert_eq!(events.try_recv(), Ok(data(b"three")));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn a_stream_that_ended_before_it_was_claimed_stays_ended() {
        let routes = ScreenRoutes::default();
        let channel = ChannelId(4);
        routes.deliver(channel, data(b"last"));
        routes.deliver(channel, ScreenEvent::Closed("gone".into()));
        let events = routes.claim(channel);
        assert_eq!(events.try_recv(), Ok(data(b"last")));
        assert_eq!(events.try_recv(), Ok(ScreenEvent::Closed("gone".into())));
        routes.deliver(channel, data(b"late"));
        assert!(events.try_recv().is_err());
        assert!(routes.is_released(channel));
    }

    #[test]
    fn a_released_channel_drops_what_arrives_later() {
        let routes = ScreenRoutes::default();
        let channel = ChannelId(5);
        let events = routes.claim(channel);
        routes.release(channel);
        routes.deliver(channel, data(b"late"));
        assert!(events.try_recv().is_err());
        assert!(routes.is_released(channel));
    }

    #[test]
    fn losing_the_connection_ends_every_stream() {
        let routes = ScreenRoutes::default();
        let events = routes.claim(ChannelId(6));
        routes.disconnected();
        assert!(matches!(events.try_recv(), Ok(ScreenEvent::Closed(_))));
    }
}
