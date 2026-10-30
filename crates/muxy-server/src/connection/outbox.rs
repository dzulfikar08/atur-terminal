use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use muxy_protocol::{
    AttachSnapshot, CONTROL, ChannelId, ErrorCode, ExitReason, ForegroundProcess, Message,
    MetadataEvent, ReplyBody, RequestId, ScreenFrame, SessionId, SessionProgress, Size,
    TerminalColors,
};

use crate::{AttachmentId, ServerError, SessionCommand, SessionHandle};

use super::merge::{merge, weight};

struct Attachment {
    handle: SessionHandle,
    id: AttachmentId,
    waiting: Option<RequestId>,
    resizes: VecDeque<RequestId>,
}

#[derive(Clone, Default)]
pub(crate) struct References {
    pub(crate) owner: Option<muxy_protocol::OperationId>,
    pub(crate) revision: u64,
    sessions: HashSet<SessionId>,
    released: HashSet<SessionId>,
}

impl References {
    pub(crate) fn update(
        &mut self,
        owner: Option<muxy_protocol::OperationId>,
        revision: u64,
        sessions: Vec<SessionId>,
    ) {
        let sessions: HashSet<_> = sessions.into_iter().collect();
        self.released
            .extend(self.sessions.difference(&sessions).copied());
        self.released.retain(|session| !sessions.contains(session));
        self.owner = owner;
        self.revision = revision;
        self.sessions = sessions;
    }
}

/// Invalidations this connection asked for by reading the matching state.
#[derive(Clone, Copy, Default)]
struct Watches {
    catalog: bool,
    activity: bool,
    remote_access: bool,
}

const WINDOW_FRAMES: usize = 8;
const WINDOW_BYTES: usize = 256 * 1024;

/// Most bytes of a screen stream that may be queued or on the wire unreported
/// by the client. At 20 Mbit/s this is a fifth of a second of video; a client
/// further behind makes the capture program wait instead.
const SCREEN_WINDOW: usize = 512 * 1024;

/// One screen viewer's way out: bytes of the capture program's conversation
/// waiting for the writer, and how many the client has reported handled.
struct ScreenStream {
    queue: VecDeque<Vec<u8>>,
    queued: usize,
    sent: u64,
    acked: u64,
    /// Set when the stream ended; `ScreenClosed` follows the queued bytes.
    ending: Option<String>,
    viewer: Option<crate::screen::Viewer>,
}

impl ScreenStream {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            queued: 0,
            sent: 0,
            acked: 0,
            ending: None,
            viewer: None,
        }
    }

    /// Bytes queued or sent and not yet reported handled.
    fn unhandled(&self) -> usize {
        self.queued + usize::try_from(self.sent - self.acked).unwrap_or(usize::MAX)
    }
}

/// What became of bytes a client sent to a screen stream.
pub(super) enum ScreenInput {
    Delivered,
    Unknown,
    /// The capture program is not reading; the stream was ended.
    Stalled,
}

#[derive(Default)]
struct Window {
    in_flight: VecDeque<(u64, usize)>,
    bytes: usize,
}

impl Window {
    fn admits(&self, weight: usize) -> bool {
        self.in_flight.is_empty()
            || (self.in_flight.len() < WINDOW_FRAMES && self.bytes + weight <= WINDOW_BYTES)
    }

    fn send(&mut self, seq: u64, weight: usize) {
        self.in_flight.push_back((seq, weight));
        self.bytes += weight;
    }

    fn ack(&mut self, seq: u64) -> bool {
        if self
            .in_flight
            .back()
            .is_none_or(|&(newest, _)| seq > newest)
        {
            return false;
        }
        let mut released = false;
        while let Some(&(oldest, weight)) = self.in_flight.front()
            && oldest <= seq
        {
            self.in_flight.pop_front();
            self.bytes -= weight;
            released = true;
        }
        released
    }
}

#[derive(Default)]
struct State {
    client: muxy_protocol::SessionClient,
    /// The paired device on the other end of a network connection.
    device: Option<muxy_protocol::DeviceId>,
    positions: HashMap<SessionId, u64>,
    open_sessions: HashSet<SessionId>,
    created: HashSet<SessionId>,
    changes: Arc<AtomicU64>,
    references: References,
    progress: HashMap<SessionId, SessionProgress>,
    session_metadata: HashMap<SessionId, muxy_protocol::SessionMetadata>,
    watches: Watches,
    colors: Option<TerminalColors>,
    control: VecDeque<Message>,
    pending: HashMap<ChannelId, ScreenFrame>,
    metadata: HashMap<ChannelId, Vec<MetadataEvent>>,
    window: HashMap<ChannelId, Window>,
    attachments: HashMap<ChannelId, Attachment>,
    screens: HashMap<ChannelId, ScreenStream>,
    closed: bool,
}

pub(crate) struct Outbox {
    state: Mutex<State>,
    ready: Condvar,
    /// Wakes screen relays waiting for the client to report handled bytes.
    space: Condvar,
}

impl Outbox {
    pub(super) fn new(changes: Arc<AtomicU64>) -> Self {
        Self {
            state: Mutex::new(State {
                changes,
                ..State::default()
            }),
            ready: Condvar::default(),
            space: Condvar::default(),
        }
    }

    pub(super) fn for_device(changes: Arc<AtomicU64>, device: muxy_protocol::DeviceId) -> Self {
        let outbox = Self::new(changes);
        {
            let mut state = outbox.lock();
            state.device = Some(device);
            state.client.kind = muxy_protocol::ClientKind::Mobile;
        }
        outbox
    }

    pub(crate) fn device(&self) -> Option<muxy_protocol::DeviceId> {
        self.lock().device
    }

    pub(crate) fn referenced_sessions(&self) -> Vec<SessionId> {
        self.lock().references.sessions.iter().copied().collect()
    }

    pub(crate) fn reference_state(&self) -> Option<References> {
        let state = self.lock();
        (!state.closed).then(|| state.references.clone())
    }

    pub(crate) fn set_references(
        &self,
        references: References,
        reported: Option<&[SessionId]>,
        acknowledge_creation: bool,
    ) {
        let mut state = self.lock();
        state
            .created
            .retain(|session| !acknowledge_creation && !references.sessions.contains(session));
        if let Some(sessions) = reported {
            state.open_sessions = sessions.iter().copied().collect();
        }
        state
            .open_sessions
            .retain(|session| references.sessions.contains(session));
        state.control.retain(|message| !matches!(message, Message::Progress { session, .. } | Message::SessionMetadata { session, .. } if !references.sessions.contains(session)));
        state
            .progress
            .retain(|session, _| references.sessions.contains(session));
        state
            .session_metadata
            .retain(|session, _| references.sessions.contains(session));
        state.references = references;
        state.refresh_positions();
    }

    pub(crate) fn created_session(&self, session: SessionId) {
        let mut state = self.lock();
        if !state.closed && !state.positions.contains_key(&session) {
            state.created.insert(session);
            state.refresh_positions();
        }
    }

    pub(super) fn watch_activity(&self) {
        self.lock().watches.activity = true;
    }
    pub(super) fn watch_remote_access(&self) {
        self.lock().watches.remote_access = true;
    }
    pub(super) fn remote_access_watched(&self) -> bool {
        self.lock().watches.remote_access
    }
    pub(super) fn activity_watched(&self) -> bool {
        self.lock().watches.activity
    }

    pub(crate) fn client(&self) -> muxy_protocol::SessionClient {
        self.lock().client
    }

    pub(super) fn identify(&self, kind: muxy_protocol::ClientKind) -> muxy_protocol::SessionClient {
        let mut state = self.lock();
        if state.client.kind != kind {
            state.client.kind = kind;
            state.changes.fetch_add(1, Ordering::AcqRel);
        }
        state.client
    }

    pub(crate) fn participation(
        &self,
        session: SessionId,
    ) -> Option<(u64, muxy_protocol::SessionClient)> {
        let state = self.lock();
        if state.closed {
            return None;
        }
        state
            .positions
            .get(&session)
            .map(|position| (*position, state.client))
    }

    pub(crate) fn references(&self, session: SessionId, include_channels: bool) -> bool {
        let state = self.lock();
        !state.closed
            && (state.references.sessions.contains(&session)
                || (include_channels && state.created.contains(&session))
                || (include_channels
                    && !state.references.released.contains(&session)
                    && state
                        .attachments
                        .values()
                        .any(|attachment| attachment.handle.info().id == session)))
    }

    pub(crate) fn detach_session(&self, session: SessionId) {
        let mut state = self.lock();
        state.created.remove(&session);
        let channels: Vec<_> = state
            .attachments
            .iter()
            .filter(|(_, attachment)| attachment.handle.info().id == session)
            .map(|(channel, _)| *channel)
            .collect();
        for channel in channels {
            state.retire(
                channel,
                &ServerError::new(ErrorCode::UnknownChannel, "pane was closed"),
            );
        }
        state.refresh_positions();
        self.ready.notify_one();
    }

    pub(super) fn watch_catalog(&self) {
        self.lock().watches.catalog = true;
    }
    pub(super) fn catalog_watched(&self) -> bool {
        self.lock().watches.catalog
    }

    pub(super) fn colors(&self) -> Option<TerminalColors> {
        self.lock().colors.clone()
    }

    pub(super) fn set_colors(&self, colors: TerminalColors) {
        let mut state = self.lock();
        for attachment in state.attachments.values() {
            let _ = attachment
                .handle
                .send(SessionCommand::SetColors(colors.clone()));
        }
        state.colors = Some(colors);
    }

    pub(crate) fn push_control(&self, message: Message) {
        let mut state = self.lock();
        if !state.closed {
            if let Message::Changed { topic, revision } = &message
                && let Some(Message::Changed {
                    revision: pending, ..
                }) = state.control.iter_mut().find(
                    |message| matches!(message, Message::Changed { topic: pending, .. } if pending == topic),
                )
            {
                *pending = (*pending).max(*revision);
                return;
            }
            if let Message::SessionMetadata { session, metadata } = &message {
                if !state.references.sessions.contains(session)
                    || state.session_metadata.get(session) == Some(metadata)
                {
                    return;
                }
                state.session_metadata.insert(*session, metadata.clone());
                if let Some(pending) = state.control.iter_mut().find(|pending| matches!(pending, Message::SessionMetadata { session: id, .. } if id == session)) {
                    *pending = message;
                    return;
                }
            }
            if let Message::Progress { session, progress } = &message {
                if !state.references.sessions.contains(session)
                    || *progress == state.progress.get(session).copied().unwrap_or_default()
                {
                    return;
                }
                state.progress.insert(*session, *progress);
            }
            if let Message::Progress { session, .. } = &message
                && let Some(pending) = state.control.iter_mut().find(|pending| matches!(pending, Message::Progress { session: id, .. } if id == session)) {
                *pending = message;
                return;
            }
            if let Message::FilesChanged { project, changes } = &message
                && let Some(Message::FilesChanged { changes: pending, .. }) = state.control.iter_mut().find(|pending| matches!(pending, Message::FilesChanged { project: id, .. } if id == project)) {
                pending.merge(changes);
                return;
            }
            if let Message::GitChanged { project } = &message
                && state.control.iter().any(|pending| matches!(pending, Message::GitChanged { project: id } if id == project)) {
                return;
            }
            state.control.push_back(message);
            self.ready.notify_one();
        }
    }

    pub(super) fn attach(
        &self,
        channel: ChannelId,
        id: AttachmentId,
        request: RequestId,
        handle: SessionHandle,
        command: SessionCommand,
    ) -> Result<(), ServerError> {
        let mut state = self.lock();
        if state.closed {
            return Err(ServerError::new(
                ErrorCode::BadRequest,
                "connection is closed",
            ));
        }
        let session = handle.info().id;
        if state.references.owner.is_some() && state.references.released.contains(&session) {
            return Err(ServerError::unknown_session(session));
        }
        if let Some(colors) = &state.colors {
            handle.send(SessionCommand::SetColors(colors.clone()))?;
        }
        if let Err(undelivered) = handle.try_send(command) {
            // Its sink reports back to this outbox when dropped, which takes the lock.
            drop(state);
            drop(undelivered);
            return Err(ServerError::unknown_session(session));
        }
        state.references.released.remove(&session);
        state.attachments.insert(
            channel,
            Attachment {
                handle,
                id,
                waiting: Some(request),
                resizes: VecDeque::new(),
            },
        );
        state.window.insert(channel, Window::default());
        state.created.remove(&session);
        state.refresh_positions();
        Ok(())
    }

    pub(super) fn snapshot(
        &self,
        channel: ChannelId,
        snapshot: AttachSnapshot,
        process: Option<ForegroundProcess>,
    ) -> bool {
        let mut state = self.lock();
        let Some(attachment) = state.attachments.get_mut(&channel) else {
            return false;
        };
        if let Some(id) = attachment.waiting.take() {
            state.control.push_back(Message::Reply {
                id,
                body: ReplyBody::Attached {
                    snapshot: Box::new(snapshot),
                    process,
                },
            });
            self.ready.notify_one();
        }
        true
    }

    pub(super) fn attach_failed(&self, channel: ChannelId, error: &ServerError) {
        let mut state = self.lock();
        state.retire(channel, error);
        self.ready.notify_one();
    }

    pub(super) fn handle(&self, channel: ChannelId) -> Option<SessionHandle> {
        self.lock()
            .attachments
            .get(&channel)
            .map(|attachment| attachment.handle.clone())
    }

    pub(super) fn resize(
        &self,
        channel: ChannelId,
        request: RequestId,
        size: Size,
    ) -> Result<(), ServerError> {
        let mut state = self.lock();
        let attachment = state
            .attachments
            .get_mut(&channel)
            .ok_or_else(|| ServerError::new(ErrorCode::UnknownChannel, "unknown channel"))?;
        attachment.handle.send(SessionCommand::ResizeAttachment {
            id: attachment.id,
            size,
        })?;
        attachment.resizes.push_back(request);
        Ok(())
    }

    pub(super) fn resized(&self, channel: ChannelId, frame: ScreenFrame) -> bool {
        let mut state = self.lock();
        let Some(attachment) = state.attachments.get_mut(&channel) else {
            return false;
        };
        if let Some(id) = attachment.resizes.pop_front() {
            state.pending.insert(channel, frame);
            state.control.push_back(Message::Reply {
                id,
                body: ReplyBody::Resized,
            });
            self.ready.notify_one();
        }
        true
    }

    pub(super) fn detach(&self, channel: ChannelId) -> Result<(), ServerError> {
        let mut state = self.lock();
        if let Some(stream) = state.screens.remove(&channel) {
            drop(state);
            drop(stream);
            self.space.notify_all();
            return Ok(());
        }
        if !state.attachments.contains_key(&channel) {
            return Err(ServerError::new(
                ErrorCode::UnknownChannel,
                "unknown channel",
            ));
        }
        state.retire(
            channel,
            &ServerError::new(ErrorCode::UnknownChannel, "channel detached"),
        );
        self.ready.notify_one();
        Ok(())
    }

    pub(super) fn session_ended(&self, session: SessionId, reason: ExitReason) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        state.created.remove(&session);
        state.control.retain(
            |message| !matches!(message, Message::Progress { session: id, .. } | Message::SessionMetadata { session: id, .. } if *id == session),
        );
        state.references.sessions.remove(&session);
        state.progress.remove(&session);
        state.session_metadata.remove(&session);
        state.open_sessions.remove(&session);
        let channels: Vec<_> = state
            .attachments
            .iter()
            .filter_map(|(&channel, attachment)| {
                (attachment.handle.id() == session).then_some(channel)
            })
            .collect();
        let error = ServerError::unknown_session(session);
        for channel in channels {
            state.retire(channel, &error);
        }
        state.refresh_positions();
        state
            .control
            .push_back(Message::SessionEnded { session, reason });
        self.ready.notify_one();
    }

    pub(super) fn push_frame(&self, channel: ChannelId, frame: ScreenFrame) -> bool {
        let mut state = self.lock();
        if !state.window.contains_key(&channel) || state.closed {
            return false;
        }
        match state.pending.entry(channel) {
            std::collections::hash_map::Entry::Occupied(mut entry) => merge(entry.get_mut(), frame),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(frame);
            }
        }
        self.ready.notify_one();
        true
    }

    pub(super) fn push_metadata(&self, channel: ChannelId, event: MetadataEvent) -> bool {
        let mut state = self.lock();
        if !state.window.contains_key(&channel) || state.closed {
            return false;
        }
        let pending = state.metadata.entry(channel).or_default();
        if let Some(previous) = pending
            .iter_mut()
            .find(|previous| std::mem::discriminant(*previous) == std::mem::discriminant(&event))
        {
            *previous = event;
        } else {
            pending.push(event);
        }
        self.ready.notify_one();
        true
    }

    pub(super) fn ack(&self, channel: ChannelId, seq: u64) {
        let mut state = self.lock();
        if let Some(stream) = state.screens.get_mut(&channel) {
            // A client cannot have handled more than was sent.
            stream.acked = stream.acked.max(seq.min(stream.sent));
            self.space.notify_all();
            return;
        }
        if let Some(window) = state.window.get_mut(&channel)
            && window.ack(seq)
        {
            self.ready.notify_one();
        }
    }

    /// Makes `channel` a screen stream, before any of its bytes can arrive.
    pub(super) fn reserve_screen(&self, channel: ChannelId) -> bool {
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        state.screens.insert(channel, ScreenStream::new());
        true
    }

    /// Gives a reserved stream its capture link; a stream that ended meanwhile
    /// just drops it, which ends the viewer.
    pub(super) fn attach_viewer(&self, channel: ChannelId, viewer: crate::screen::Viewer) {
        let mut state = self.lock();
        if let Some(stream) = state.screens.get_mut(&channel) {
            stream.viewer = Some(viewer);
        }
    }

    /// Forgets a stream that never started.
    pub(super) fn release_screen(&self, channel: ChannelId) {
        let removed = self.lock().screens.remove(&channel);
        drop(removed);
    }

    /// Queues bytes for the client, waiting while it has not handled the
    /// window already sent. `false` once the stream or the connection is gone.
    pub(super) fn push_screen(&self, channel: ChannelId, bytes: Vec<u8>) -> bool {
        let mut state = self.lock();
        loop {
            if state.closed {
                return false;
            }
            let Some(stream) = state.screens.get_mut(&channel) else {
                return false;
            };
            if stream.ending.is_some() {
                return false;
            }
            let unhandled = stream.unhandled();
            if unhandled == 0 || unhandled + bytes.len() <= SCREEN_WINDOW {
                stream.queued += bytes.len();
                stream.queue.push_back(bytes);
                self.ready.notify_one();
                return true;
            }
            state = self
                .space
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The capture ended: after the queued bytes the client is told, and the
    /// stream is gone.
    pub(super) fn finish_screen(&self, channel: ChannelId, reason: String) {
        let mut state = self.lock();
        if let Some(stream) = state.screens.get_mut(&channel)
            && stream.ending.is_none()
        {
            stream.ending = Some(reason);
            self.ready.notify_one();
        }
    }

    /// Passes the client's bytes on to the capture program.
    pub(super) fn screen_input(&self, channel: ChannelId, bytes: Vec<u8>) -> ScreenInput {
        let mut state = self.lock();
        let Some(stream) = state.screens.get_mut(&channel) else {
            return ScreenInput::Unknown;
        };
        let Some(viewer) = &stream.viewer else {
            return ScreenInput::Delivered;
        };
        if viewer.write(bytes) {
            return ScreenInput::Delivered;
        }
        stream.viewer = None;
        stream.ending = Some("The screen sharing program stopped reading input.".into());
        self.ready.notify_one();
        self.space.notify_all();
        ScreenInput::Stalled
    }

    pub(super) fn next(&self) -> Option<(ChannelId, Message)> {
        let mut state = self.lock();
        loop {
            if let Some(message) = state.control.pop_front() {
                return Some((CONTROL, message));
            }
            if state.closed {
                return None;
            }
            if let Some(channel) = state.metadata.keys().next().copied()
                && let Some(pending) = state.metadata.get_mut(&channel)
            {
                let event = pending.remove(0);
                if pending.is_empty() {
                    state.metadata.remove(&channel);
                }
                return Some((channel, Message::Metadata(event)));
            }
            let channel = state
                .pending
                .iter()
                .find(|(channel, frame)| {
                    state
                        .window
                        .get(channel)
                        .is_some_and(|window| window.admits(weight(frame)))
                })
                .map(|(channel, _)| *channel);
            if let Some(channel) = channel
                && let Some(frame) = state.pending.remove(&channel)
                && let Some(window) = state.window.get_mut(&channel)
            {
                window.send(frame.seq, weight(&frame));
                return Some((channel, Message::Frame(frame)));
            }
            let screen = state
                .screens
                .iter()
                .find(|(_, stream)| !stream.queue.is_empty() || stream.ending.is_some())
                .map(|(channel, _)| *channel);
            if let Some(channel) = screen
                && let Some(stream) = state.screens.get_mut(&channel)
            {
                if let Some(chunk) = stream.queue.pop_front() {
                    stream.queued -= chunk.len();
                    stream.sent += chunk.len() as u64;
                    return Some((channel, Message::ScreenData(chunk)));
                }
                if let Some(reason) = stream.ending.take() {
                    state.screens.remove(&channel);
                    return Some((channel, Message::ScreenClosed(reason)));
                }
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    pub(super) fn close_with(&self, message: Message) {
        let mut state = self.lock();
        if !state.closed {
            state.control.push_back(message);
            state.close();
            self.ready.notify_all();
            self.space.notify_all();
        }
    }

    pub(crate) fn close(&self) {
        self.lock().close();
        self.ready.notify_all();
        self.space.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn refresh_positions(&mut self) {
        if self.closed {
            return;
        }
        let mut sessions = self.open_sessions.clone();
        sessions.extend(&self.created);
        sessions.extend(
            self.attachments
                .values()
                .map(|attachment| attachment.handle.id())
                .filter(|session| !self.references.released.contains(session)),
        );
        let previous = self.positions.len();
        self.positions
            .retain(|session, _| sessions.contains(session));
        if previous != self.positions.len() {
            self.changes.fetch_add(1, Ordering::AcqRel);
        }
        for session in sessions {
            self.positions
                .entry(session)
                .or_insert_with(|| self.changes.fetch_add(1, Ordering::AcqRel));
        }
    }

    fn close(&mut self) {
        if !self.positions.is_empty() {
            self.positions.clear();
            self.changes.fetch_add(1, Ordering::AcqRel);
        }
        self.closed = true;
        for (_, attachment) in self.attachments.drain() {
            let _ = attachment
                .handle
                .send(SessionCommand::Detach(attachment.id));
        }
        self.pending.clear();
        self.progress.clear();
        self.metadata.clear();
        self.window.clear();
        self.screens.clear();
    }

    fn retire(&mut self, channel: ChannelId, error: &ServerError) {
        self.pending.remove(&channel);
        self.metadata.remove(&channel);
        self.window.remove(&channel);
        if let Some(attachment) = self.attachments.remove(&channel) {
            let _ = attachment
                .handle
                .send(SessionCommand::Detach(attachment.id));
            for id in attachment.waiting.into_iter().chain(attachment.resizes) {
                self.control.push_back(Message::Reply {
                    id,
                    body: ReplyBody::Error(error.to_reply()),
                });
            }
        }
        self.refresh_positions();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration;

    use muxy_protocol::{Cursor, Modes, Row};

    use super::*;

    #[test]
    fn a_screen_stream_waits_for_the_client_beyond_its_window() {
        let outbox = Arc::new(Outbox::new(Arc::default()));
        let channel = ChannelId(3);
        assert!(outbox.reserve_screen(channel));
        let chunk = 16 * 1024;
        for _ in 0..SCREEN_WINDOW / chunk {
            assert!(outbox.push_screen(channel, vec![7; chunk]));
        }
        let (done, finished) = mpsc::channel();
        let pusher = Arc::clone(&outbox);
        let blocked = thread::spawn(move || {
            let accepted = pusher.push_screen(channel, vec![8; chunk]);
            let _ = done.send(accepted);
        });
        assert!(finished.recv_timeout(Duration::from_millis(150)).is_err());
        // Sending is not handling: the window opens when the client reports it.
        assert_eq!(
            outbox.next(),
            Some((channel, Message::ScreenData(vec![7; chunk])))
        );
        assert!(finished.recv_timeout(Duration::from_millis(150)).is_err());
        outbox.ack(channel, chunk as u64);
        assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(true));
        let _ = blocked.join();
        // An acknowledgement beyond what was sent counts only what was sent.
        outbox.ack(channel, u64::MAX);
        assert_eq!(outbox.lock().screens[&channel].acked, chunk as u64);
    }

    #[test]
    fn a_finished_screen_stream_closes_after_its_queued_bytes() {
        let outbox = Outbox::new(Arc::default());
        let channel = ChannelId(2);
        assert!(outbox.reserve_screen(channel));
        assert!(outbox.push_screen(channel, b"last".to_vec()));
        outbox.finish_screen(channel, "gone".into());
        assert!(!outbox.push_screen(channel, b"late".to_vec()));
        assert_eq!(
            outbox.next(),
            Some((channel, Message::ScreenData(b"last".to_vec())))
        );
        assert_eq!(
            outbox.next(),
            Some((channel, Message::ScreenClosed("gone".into())))
        );
        assert!(outbox.detach(channel).is_err());
    }

    #[test]
    fn detaching_a_screen_stream_ends_it_and_frees_a_waiting_relay() {
        let outbox = Arc::new(Outbox::new(Arc::default()));
        let channel = ChannelId(4);
        assert!(outbox.reserve_screen(channel));
        assert!(outbox.push_screen(channel, vec![1; SCREEN_WINDOW]));
        let pusher = Arc::clone(&outbox);
        let waiting = thread::spawn(move || pusher.push_screen(channel, vec![2; 1024]));
        thread::sleep(Duration::from_millis(100));
        assert!(outbox.detach(channel).is_ok());
        assert_eq!(waiting.join().ok(), Some(false));
    }

    #[test]
    fn file_changes_coalesce_and_rescan_for_slow_consumers() {
        let outbox = Outbox::new(Arc::default());
        let project = muxy_protocol::ProjectId::new();
        for index in 0..1000 {
            outbox.push_control(Message::FilesChanged {
                project,
                changes: muxy_protocol::FileChanges {
                    paths: vec![muxy_protocol::ServerPath(index.to_string().into_bytes())],
                    rescan: false,
                },
            });
        }
        assert_eq!(outbox.lock().control.len(), 1);
        assert_eq!(
            outbox.next(),
            Some((
                CONTROL,
                Message::FilesChanged {
                    project,
                    changes: muxy_protocol::FileChanges {
                        paths: Vec::new(),
                        rescan: true
                    }
                }
            ))
        );
    }

    fn frame(seq: u64, index: u16) -> ScreenFrame {
        ScreenFrame {
            size: Size { cols: 80, rows: 24 },
            graphics: None,
            seq,
            reset: false,
            rows: vec![Row {
                index,
                runs: vec![],
            }],
            cursor: Cursor {
                shape: muxy_protocol::CursorShape::default(),
                row: 0,
                col: 0,
                visible: true,
            },
            modes: Modes::default(),
        }
    }

    #[test]
    fn title_updates_coalesce_and_resubscribe_without_screen_window() {
        let outbox = Outbox::new(Arc::default());
        let session = SessionId::from(std::num::NonZeroU64::MIN);
        let message = |title: String| Message::SessionMetadata {
            session,
            metadata: muxy_protocol::SessionMetadata {
                title,
                directory: muxy_protocol::ServerPath(b"/tmp".to_vec()),
                process: None,
            },
        };
        outbox.lock().references.sessions.insert(session);
        for i in 0..1000 {
            outbox.push_control(message(i.to_string()));
        }
        assert_eq!(outbox.lock().control.len(), 1);
        assert_eq!(outbox.next(), Some((CONTROL, message("999".into()))));
        outbox.push_control(message("999".into()));
        assert!(outbox.lock().control.is_empty());
        outbox.push_control(message("pending".into()));
        outbox.set_references(References::default(), Some(&[]), false);
        outbox.push_control(message("ignored".into()));
        assert!(outbox.lock().control.is_empty());
        outbox.lock().references.sessions.insert(session);
        outbox.push_control(message("pending".into()));
        assert_eq!(outbox.next(), Some((CONTROL, message("pending".into()))));
    }

    #[test]
    fn progress_resubscription_restores_unchanged_state_between_polls() {
        for delivered in [false, true] {
            let outbox = Outbox::new(Arc::default());
            let session = SessionId::from(std::num::NonZeroU64::MIN);
            let message = Message::Progress {
                session,
                progress: SessionProgress {
                    progress: Some(muxy_protocol::TerminalProgress {
                        state: muxy_protocol::ProgressState::Indeterminate,
                        percent: None,
                    }),
                    completed: 0,
                },
            };
            let mut references = References::default();
            references.update(None, 1, vec![session]);
            outbox.set_references(references.clone(), Some(&[session]), false);
            outbox.push_control(message.clone());
            if delivered {
                assert_eq!(outbox.next(), Some((CONTROL, message.clone())));
            }
            outbox.set_references(References::default(), Some(&[]), false);
            outbox.push_control(message.clone());
            assert!(outbox.lock().control.is_empty());
            outbox.set_references(references.clone(), Some(&[session]), false);
            outbox.push_control(message.clone());
            assert_eq!(outbox.lock().control.len(), 1);
            assert_eq!(outbox.next(), Some((CONTROL, message.clone())));
            outbox.set_references(references, Some(&[session]), false);
            outbox.push_control(message);
            assert!(outbox.lock().control.is_empty());
        }
    }

    #[test]
    fn negotiated_development_protocol_preserves_every_control_message() {
        for message in Message::samples()
            .into_iter()
            .filter(|message| message.channel_kind() == muxy_protocol::ChannelKind::Control)
        {
            let outbox = Outbox::new(Arc::default());
            if let Message::Progress { session, .. } | Message::SessionMetadata { session, .. } =
                &message
            {
                outbox.lock().references.sessions.insert(*session);
            }
            outbox.push_control(message.clone());
            outbox.close();
            assert_eq!(outbox.next(), Some((CONTROL, message)));
            assert_eq!(outbox.next(), None);
        }
    }

    #[test]
    fn control_first_and_independent_windows_per_channel() {
        let outbox = Outbox::new(Arc::default());
        outbox.lock().window.extend([
            (ChannelId(1), Window::default()),
            (ChannelId(2), Window::default()),
        ]);
        outbox.push_frame(ChannelId(1), frame(1, 0));
        outbox.push_control(Message::HelloReply {
            versions: muxy_protocol::SUPPORTED.to_vec(),
            server: muxy_protocol::ServerInfo::current(),
            features: Vec::new(),
        });
        assert!(matches!(
            outbox.next(),
            Some((CONTROL, Message::HelloReply { .. }))
        ));
        assert_eq!(
            outbox.next(),
            Some((ChannelId(1), Message::Frame(frame(1, 0))))
        );
        outbox.push_frame(ChannelId(2), frame(1, 0));
        assert_eq!(
            outbox.next(),
            Some((ChannelId(2), Message::Frame(frame(1, 0))))
        );
    }

    fn open_channel(outbox: &Outbox, channel: ChannelId) {
        outbox.lock().window.insert(channel, Window::default());
    }

    fn send_frame(outbox: &Outbox, channel: ChannelId, seq: u64) {
        outbox.push_frame(channel, frame(seq, 0));
        assert_eq!(
            outbox.next(),
            Some((channel, Message::Frame(frame(seq, 0))))
        );
    }

    fn in_flight(outbox: &Outbox, channel: ChannelId) -> Vec<u64> {
        outbox.lock().window[&channel]
            .in_flight
            .iter()
            .map(|&(seq, _)| seq)
            .collect()
    }

    #[test]
    fn window_holds_frames_in_flight_then_merges_and_cumulative_ack_releases_the_oldest() {
        let outbox = Outbox::new(Arc::default());
        let channel = ChannelId(1);
        open_channel(&outbox, channel);
        for seq in 1..=WINDOW_FRAMES as u64 {
            send_frame(&outbox, channel, seq);
        }
        outbox.push_frame(channel, frame(9, 1));
        outbox.push_frame(channel, frame(10, 2));
        let full = outbox.lock();
        assert_eq!(full.window[&channel].in_flight.len(), WINDOW_FRAMES);
        assert!(!full.window[&channel].admits(weight(&full.pending[&channel])));
        assert_eq!(full.pending.len(), 1);
        drop(full);
        outbox.ack(channel, 3);
        assert_eq!(in_flight(&outbox, channel), vec![4, 5, 6, 7, 8]);
        let mut merged = frame(10, 1);
        merged.rows.push(Row {
            index: 2,
            runs: vec![],
        });
        assert_eq!(outbox.next(), Some((channel, Message::Frame(merged))));
        assert_eq!(in_flight(&outbox, channel), vec![4, 5, 6, 7, 8, 10]);
        outbox.ack(channel, 10);
        assert!(in_flight(&outbox, channel).is_empty());
        assert_eq!(outbox.lock().window[&channel].bytes, 0);
    }

    #[test]
    fn acking_every_frame_in_order_behaves_like_the_original_client() {
        let outbox = Outbox::new(Arc::default());
        let channel = ChannelId(1);
        open_channel(&outbox, channel);
        for seq in [1, 2, 5, 6, 9] {
            send_frame(&outbox, channel, seq);
            assert_eq!(in_flight(&outbox, channel), vec![seq]);
            outbox.ack(channel, seq);
            assert!(in_flight(&outbox, channel).is_empty());
        }
    }

    #[test]
    fn stale_repeated_and_unsent_acks_release_nothing() {
        let outbox = Outbox::new(Arc::default());
        let channel = ChannelId(1);
        open_channel(&outbox, channel);
        send_frame(&outbox, channel, 3);
        send_frame(&outbox, channel, 4);
        outbox.ack(channel, 0);
        outbox.ack(channel, 2);
        outbox.ack(channel, 5);
        outbox.ack(ChannelId(9), 4);
        assert_eq!(in_flight(&outbox, channel), vec![3, 4]);
        outbox.ack(channel, 3);
        outbox.ack(channel, 3);
        assert_eq!(in_flight(&outbox, channel), vec![4]);
    }

    #[test]
    fn byte_bound_stops_heavy_frames_but_never_starves_an_oversized_one() {
        let outbox = Outbox::new(Arc::default());
        let channel = ChannelId(1);
        open_channel(&outbox, channel);
        let heavy = |seq: u64, bytes: usize| {
            let mut heavy = frame(seq, 0);
            heavy.rows[0].runs.push(muxy_protocol::Run {
                text: "x".repeat(bytes),
                width: 1,
                style: muxy_protocol::Style::default(),
            });
            heavy
        };
        outbox.push_frame(channel, heavy(1, WINDOW_BYTES * 2));
        assert!(matches!(
            outbox.next(),
            Some((_, Message::Frame(sent))) if sent.seq == 1
        ));
        outbox.push_frame(channel, frame(2, 0));
        {
            let state = outbox.lock();
            assert!(!state.window[&channel].admits(weight(&state.pending[&channel])));
        }
        outbox.ack(channel, 1);
        assert_eq!(outbox.next(), Some((channel, Message::Frame(frame(2, 0)))));
        outbox.push_frame(channel, heavy(3, WINDOW_BYTES / 2));
        assert!(matches!(
            outbox.next(),
            Some((_, Message::Frame(sent))) if sent.seq == 3
        ));
        outbox.push_frame(channel, heavy(4, WINDOW_BYTES / 2));
        let state = outbox.lock();
        assert!(!state.window[&channel].admits(weight(&state.pending[&channel])));
    }

    #[test]
    fn metadata_coalesces_without_frame_window_and_is_retired_with_its_channel() {
        let outbox = Outbox::new(Arc::default());
        let channel = ChannelId(1);
        open_channel(&outbox, channel);
        for index in 0..1000 {
            outbox.push_metadata(channel, MetadataEvent::Title(index.to_string()));
            outbox.push_metadata(channel, MetadataEvent::Bell);
        }
        assert_eq!(outbox.lock().metadata[&channel].len(), 2);
        outbox.push_control(Message::VersionUnsupported);
        assert_eq!(outbox.next(), Some((CONTROL, Message::VersionUnsupported)));
        assert_eq!(
            outbox.next(),
            Some((
                channel,
                Message::Metadata(MetadataEvent::Title("999".into()))
            ))
        );
        assert_eq!(
            outbox.next(),
            Some((channel, Message::Metadata(MetadataEvent::Bell)))
        );
        outbox.push_metadata(channel, MetadataEvent::Bell);
        outbox.lock().retire(
            channel,
            &ServerError::new(ErrorCode::UnknownChannel, "detached"),
        );
        outbox.push_metadata(channel, MetadataEvent::Bell);
        assert!(outbox.lock().metadata.is_empty());
        outbox.close();
        assert_eq!(outbox.next(), None);
    }

    #[test]
    fn clipboard_metadata_keeps_only_the_latest_write_and_is_retired() {
        let outbox = Outbox::new(Arc::default());
        let channel = ChannelId(1);
        open_channel(&outbox, channel);
        outbox.push_metadata(channel, MetadataEvent::ClipboardWrite("first".into()));
        outbox.push_metadata(channel, MetadataEvent::ClipboardWrite("last".into()));
        assert_eq!(outbox.lock().metadata[&channel].len(), 1);
        assert_eq!(
            outbox.next(),
            Some((
                channel,
                Message::Metadata(MetadataEvent::ClipboardWrite("last".into()))
            ))
        );
        outbox.push_metadata(channel, MetadataEvent::ClipboardWrite("pending".into()));
        outbox.lock().retire(
            channel,
            &ServerError::new(ErrorCode::UnknownChannel, "detached"),
        );
        assert!(outbox.lock().metadata.is_empty());
    }

    #[test]
    fn closing_wakes_writer_and_discards_pending_but_drains_control()
    -> Result<(), Box<dyn std::error::Error>> {
        let outbox = Arc::new(Outbox::new(Arc::default()));
        let mut saturated = Window::default();
        for seq in 0..WINDOW_FRAMES as u64 {
            saturated.send(seq, 0);
        }
        outbox.lock().window.insert(ChannelId(1), saturated);
        outbox.push_frame(ChannelId(1), frame(1, 0));
        let (sender, receiver) = mpsc::channel();
        let waiting = Arc::clone(&outbox);
        let writer = thread::spawn(move || {
            let _ = sender.send(waiting.next());
        });
        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
        outbox.push_control(Message::VersionUnsupported);
        outbox.close();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2))?,
            Some((CONTROL, Message::VersionUnsupported))
        );
        writer.join().map_err(|_| "writer panicked")?;
        assert_eq!(outbox.next(), None);
        outbox.push_frame(ChannelId(1), frame(2, 0));
        outbox.push_control(Message::VersionUnsupported);
        assert_eq!(outbox.next(), None);
        Ok(())
    }
}
