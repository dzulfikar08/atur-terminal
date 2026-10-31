use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use muxy_protocol::{
    AgentState, AttachSnapshot, ChannelId, ErrorCode, ExitReason, HistoryCursor, HistoryPage,
    InputModes, MetadataEvent, SavedScreen, ScreenFrame, SearchPage, SessionInfo, Size,
    TerminalColors,
};
use muxy_terminal::Terminal;
use muxy_terminal::pty::{ExitStatus, PtyEvent};

use crate::archive::{Archive, bound_history_page, history_range};
use crate::error::ServerError;
use crate::search::Search;
use crate::session::backing::{Backing, Fault};
use crate::session::checkpoint::Checkpoint;
use crate::session::metadata::Metadata;
use crate::session::prompt_marks::PromptMarks;
use crate::session::sizing::Sizing;
use crate::session::{
    AttachmentEvent, AttachmentId, AttachmentSink, SessionCommand, SessionHandle,
};

const TICK: Duration = Duration::from_millis(16);
const SYNC_TIMEOUT: Duration = Duration::from_secs(1);
const METADATA_POLL: Duration = Duration::from_millis(100);
/// How long an on-demand output stream stays open after the last client left.
const STREAM_GRACE: Duration = Duration::from_secs(5);
/// How long an attach waits for the first frame of a newly opened stream.
const STREAM_OPEN_WAIT: Duration = Duration::from_millis(1500);
/// How often a session without a stream checks that its process still exists.
const ALIVE_POLL: Duration = Duration::from_secs(1);

struct InputWrite {
    bytes: Vec<u8>,
    reply: Option<Sender<Result<(), ServerError>>>,
}

impl From<Vec<u8>> for InputWrite {
    fn from(bytes: Vec<u8>) -> Self {
        Self { bytes, reply: None }
    }
}

#[derive(Debug)]
pub(crate) enum OwnerEvent {
    Command(SessionCommand),
    Pty(PtyEvent),
    WriteFailed(io::Error),
}

impl From<PtyEvent> for OwnerEvent {
    fn from(event: PtyEvent) -> Self {
        Self::Pty(event)
    }
}

enum Wake {
    Event(OwnerEvent),
    Tick,
    Checkpoint,
    Metadata,
    Orphaned,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum OutputState {
    Open,
    Closed,
}

/// Whether the backing's output reaches the owner's terminal. Only on-demand
/// backings ever leave `Always`.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Stream {
    Always,
    Closed,
    /// Opened for a client that waits for the first frame.
    Opening(Instant),
    /// Open; the time the last client left, if none is attached.
    Open(Option<Instant>),
}

struct WaitingAttach {
    id: AttachmentId,
    channel: ChannelId,
    size: Option<Size>,
    sink: AttachmentSink,
}

struct Attachment {
    sink: AttachmentSink,
    seq: u64,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "Pending work flags and the application's cursor mode are independent states"
)]
struct Owner {
    info: SessionInfo,
    pty: Box<dyn Backing>,
    terminal: Terminal,
    metadata: Metadata,
    progress: super::SharedProgress,
    shared_metadata: super::SharedMetadata,
    activity: std::sync::Arc<crate::activity::Activity>,
    detector: crate::detection::Detector,
    detection_dirty: bool,
    next_detection: Instant,
    detection_screen: String,
    detection_progress: String,
    size: Size,
    events: Receiver<OwnerEvent>,
    input: Sender<InputWrite>,
    attachments: HashMap<AttachmentId, Attachment>,
    sender: Sender<OwnerEvent>,
    stream: Stream,
    waiting: Vec<WaitingAttach>,
    /// Which attached client sizes the session.
    sizing: Sizing,
    /// Whether the backing was last told that a client may size it.
    size_authority: bool,
    next_alive: Instant,
    output_pending: bool,
    resize_pending: bool,
    compress_pending: bool,
    history_total: u64,
    history_generation: u64,
    input_modes: InputModes,
    cursor_blinking: bool,
    links: Vec<muxy_protocol::LinkRow>,
    graphics: muxy_protocol::Graphics,
    prompts: Vec<u16>,
    /// Whether the backing drops OSC 133 marks, so `marks` finds the prompts instead.
    detect_prompts: bool,
    marks: PromptMarks,
    frame_state: Option<(muxy_protocol::Cursor, muxy_protocol::Modes)>,
    next_tick: Option<Instant>,
    last_tick: Option<Instant>,
    synchronized_since: Option<Instant>,
    output_state: OutputState,
    archive: Archive,
    /// False when the backing keeps terminal content, which then saves only at exit.
    checkpoints: bool,
    checkpoint: Checkpoint,
    next_metadata: Instant,
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Session startup receives owned terminal resources and shared server services"
)]
pub(crate) fn start(
    info: SessionInfo,
    mut pty: Box<dyn Backing>,
    size: Size,
    history_budget_bytes: usize,
    archive: Archive,
    colors: Option<TerminalColors>,
    activity: std::sync::Arc<crate::activity::Activity>,
    on_exit: impl FnOnce(ExitReason) + Send + 'static,
) -> Result<SessionHandle, ServerError> {
    let id = info.id.get();
    let (sender, receiver) = mpsc::channel();
    let (ready_sender, ready) = mpsc::channel();
    let on_demand = pty.on_demand();
    if !on_demand {
        pty.start_reader(sender.clone())?;
    }
    let wake = sender.clone();

    let progress = super::SharedProgress::default();
    let session_progress = progress.clone();
    let shared_metadata =
        std::sync::Arc::new(std::sync::Mutex::new(muxy_protocol::SessionMetadata {
            title: String::new(),
            directory: info.directory.clone(),
            process: None,
        }));
    let owner_metadata = shared_metadata.clone();
    let session = info.clone();
    let failed = sender.clone();
    thread::Builder::new()
        .name(format!("session-{id}"))
        .spawn(move || {
            let terminal =
                match Terminal::new(size, history_budget_bytes).and_then(|mut terminal| {
                    if let Some(colors) = colors {
                        set_colors(&mut terminal, &colors)?;
                    }
                    Ok(terminal)
                }) {
                    Ok(terminal) => terminal,
                    Err(error) => {
                        let _ = pty.kill();
                        let _ = pty.wait();
                        let _ = ready_sender.send(Err(ServerError::spawn_failed(error)));
                        return;
                    }
                };
            let input = match start_input(pty.as_mut(), failed) {
                Ok(input) => input,
                Err(error) => {
                    let _ = pty.kill();
                    let _ = pty.wait();
                    let _ = ready_sender.send(Err(error));
                    return;
                }
            };
            let _ = ready_sender.send(Ok(()));
            let checkpoints = !pty.persists_content();
            let detect_prompts = pty.drops_prompt_marks();
            let mut marks = PromptMarks::default();
            if detect_prompts {
                // Without a count of the rows herdr holds, nothing can be told to be gone.
                let total = pty.history_rows_now().map_or(u64::MAX, |history| {
                    u64::try_from(history.saturating_add(usize::from(size.rows)))
                        .unwrap_or(u64::MAX)
                });
                marks.restore(pty.saved_prompt_marks(session.id), total);
                marks.set_width(size.cols);
                log::info!(
                    "session {}: {} prompt marks restored (content rows: {total})",
                    session.id.get(),
                    marks.len()
                );
            }
            let owner = Owner {
                metadata: Metadata::new(session.directory.clone()),
                progress: session_progress,
                shared_metadata: owner_metadata,
                activity,
                detector: crate::detection::Detector::default(),
                detection_dirty: true,
                next_detection: Instant::now(),
                detection_screen: String::new(),
                detection_progress: String::new(),
                info: session,
                pty,
                terminal,
                size,
                events: receiver,
                input,
                attachments: HashMap::new(),
                sender: wake,
                stream: if on_demand {
                    Stream::Closed
                } else {
                    Stream::Always
                },
                waiting: Vec::new(),
                sizing: Sizing::default(),
                size_authority: true,
                next_alive: Instant::now() + ALIVE_POLL,
                output_pending: false,
                resize_pending: false,
                compress_pending: false,
                history_total: 0,
                history_generation: 0,
                input_modes: InputModes::default(),
                cursor_blinking: true,
                links: Vec::new(),
                graphics: muxy_protocol::Graphics::default(),
                prompts: Vec::new(),
                detect_prompts,
                marks,
                frame_state: None,
                next_tick: None,
                last_tick: None,
                synchronized_since: None,
                output_state: OutputState::Open,
                archive,
                checkpoints,
                checkpoint: if checkpoints {
                    Checkpoint::started(Instant::now())
                } else {
                    Checkpoint::idle()
                },
                next_metadata: Instant::now(),
            };
            on_exit(owner.run());
        })
        .map_err(ServerError::spawn_failed)?;

    ready
        .recv()
        .map_err(|_| ServerError::spawn_failed("session thread stopped before it was ready"))??;
    Ok(SessionHandle::new(info, sender, progress, shared_metadata))
}

fn set_colors(
    terminal: &mut Terminal,
    colors: &TerminalColors,
) -> Result<(), muxy_terminal::TerminalError> {
    terminal.set_defaults(colors)
}

fn start_input(
    pty: &mut dyn Backing,
    events: Sender<OwnerEvent>,
) -> Result<Sender<InputWrite>, ServerError> {
    let mut writer = pty.take_writer()?;
    let (input, pending) = mpsc::channel::<InputWrite>();
    thread::Builder::new()
        .name(format!("session-{}-input", pty.child_pid()))
        .spawn(move || {
            for input in pending {
                let result = writer.write_all(&input.bytes);
                if let Some(reply) = input.reply {
                    let acknowledgement = result.as_ref().copied().map_err(|error| {
                        ServerError::new(
                            ErrorCode::BadRequest,
                            format!("terminal input write failed: {error}"),
                        )
                    });
                    let _ = reply.send(acknowledgement);
                }
                if let Err(error) = result {
                    let _ = events.send(OwnerEvent::WriteFailed(error));
                    break;
                }
            }
        })
        .map_err(ServerError::spawn_failed)?;
    Ok(input)
}

impl Owner {
    fn run(mut self) -> ExitReason {
        let reason = self.serve().unwrap_or_else(|error| {
            log::error!("session {} stopped: {error}", self.info.id.get());
            self.pty
                .release(ExitReason::Ended)
                .map_or(ExitReason::Ended, exit_reason)
        });
        self.drain_output();
        self.update_metadata();
        self.activity.remove(self.info.id);
        self.checkpoint(Some(reason));
        for attachment in self.attachments.values_mut() {
            attachment.sink.send(AttachmentEvent::Ended(reason));
        }
        reason
    }

    #[allow(
        clippy::too_many_lines,
        reason = "One exhaustive dispatch preserves ordering between terminal input and PTY output"
    )]
    fn serve(&mut self) -> Result<ExitReason, Fault> {
        loop {
            match self.next_wake() {
                Wake::Event(OwnerEvent::Pty(PtyEvent::Output(bytes))) => {
                    self.feed(&bytes)?;
                    self.stream_output()?;
                }
                Wake::Event(OwnerEvent::Pty(PtyEvent::Closed)) => {
                    self.output_state = OutputState::Closed;
                }
                Wake::Event(OwnerEvent::WriteFailed(error)) => return Err(error.into()),
                Wake::Event(OwnerEvent::Command(SessionCommand::Input(bytes))) => {
                    self.observe_input(&bytes);
                    self.input.send(bytes.into())?;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::TerminalInput(input))) => {
                    self.terminal_input(input)?;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::WriteInput { bytes, reply })) => {
                    self.observe_input(&bytes);
                    self.input.send(InputWrite {
                        bytes,
                        reply: Some(reply),
                    })?;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::ClearScreen { reply })) => {
                    let result = self.clear_screen().map_err(|error| {
                        ServerError::new(ErrorCode::BadRequest, format!("terminal clear: {error}"))
                    });
                    let _ = reply.send(result);
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::CellSize(cell))) => {
                    self.pty.set_cell_size(cell)?;
                    self.terminal.set_cell_size(cell)?;
                    self.output_pending = true;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::Mouse(event))) => {
                    if let Some(sent) = self.pty.mouse(&event) {
                        sent?;
                    } else {
                        let bytes = self.terminal.encode_mouse(&event)?;
                        if !bytes.is_empty() {
                            self.input.send(bytes.into())?;
                        }
                    }
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::SetColors(colors))) => {
                    set_colors(&mut self.terminal, &colors)?;
                    let answers = self.terminal.take_pty_output();
                    if !answers.is_empty() {
                        self.input.send(answers.into())?;
                    }
                    self.output_pending = true;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::Resize(size))) => {
                    self.resize(size)?;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::ResizeAttachment { id, size })) => {
                    self.resize_attachment(id, size)?;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::Typed(id))) => {
                    self.typed(id, Instant::now())?;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::Attach {
                    id,
                    channel,
                    size,
                    watching,
                    sink,
                })) => {
                    self.sizing.join(id, watching, Some(size));
                    self.request_attach(id, channel, Some(size), sink)?;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::AttachWithoutResize {
                    id,
                    channel,
                    watching,
                    sink,
                })) => {
                    self.sizing.join(id, watching, None);
                    self.request_attach(id, channel, None, sink)?;
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::HistoryPage {
                    before,
                    max_rows,
                    reply,
                })) => {
                    let _ = reply.send(self.history_page(before, max_rows));
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::Detach(id))) => {
                    self.attachments.remove(&id);
                    self.waiting.retain(|waiting| waiting.id != id);
                    self.sizing.leave(id);
                    self.sync_size_authority();
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::Search {
                    query,
                    ignore_case,
                    before,
                    max_results,
                    reply,
                })) => {
                    let _ = reply.send(self.search(&query, ignore_case, before, max_results));
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::End)) => {
                    return self.terminate(ExitReason::Ended);
                }
                Wake::Event(OwnerEvent::Command(SessionCommand::Stop)) => {
                    return self.terminate(ExitReason::ServerStopped);
                }
                Wake::Tick => self.tick()?,
                Wake::Checkpoint => self.checkpoint(None),
                Wake::Metadata => self.update_metadata(),
                Wake::Orphaned => return self.terminate(ExitReason::ServerStopped),
            }
            self.service_stream()?;
            if self.output_state == OutputState::Closed
                && let Some(status) = self.pty.try_wait()
            {
                return Ok(exit_reason(status));
            }
            if self.next_tick.is_none() && self.has_pending_work() {
                self.next_tick = Some(tick_deadline(self.last_tick, Instant::now()));
            }
        }
    }

    /// Attaches a client. An on-demand backing first opens its output stream and
    /// the client is attached when the first frame has painted the terminal.
    fn request_attach(
        &mut self,
        id: AttachmentId,
        channel: ChannelId,
        size: Option<Size>,
        sink: AttachmentSink,
    ) -> Result<(), Fault> {
        self.sync_size_authority();
        match self.stream {
            Stream::Always => self.attach(id, channel, size.unwrap_or(self.size), sink),
            Stream::Open(_) => {
                self.stream = Stream::Open(None);
                self.attach(id, channel, size.unwrap_or(self.size), sink)
            }
            Stream::Opening(_) => {
                self.waiting.push(WaitingAttach {
                    id,
                    channel,
                    size,
                    sink,
                });
                Ok(())
            }
            Stream::Closed => {
                if let Some(size) = size
                    && size != self.size
                    && !self.sizing.watching(id)
                {
                    self.resize(size)?;
                }
                self.pty.set_streaming(true, &self.sender)?;
                self.stream = Stream::Opening(Instant::now());
                self.waiting.push(WaitingAttach {
                    id,
                    channel,
                    size,
                    sink,
                });
                Ok(())
            }
        }
    }

    fn attach_waiting(&mut self) -> Result<(), Fault> {
        for waiting in std::mem::take(&mut self.waiting) {
            let size = waiting.size.unwrap_or(self.size);
            self.attach(waiting.id, waiting.channel, size, waiting.sink)?;
        }
        Ok(())
    }

    /// The first output of a newly opened stream has arrived.
    fn stream_output(&mut self) -> Result<(), Fault> {
        if matches!(self.stream, Stream::Opening(_)) {
            self.stream = Stream::Open(None);
            self.attach_waiting()?;
        }
        Ok(())
    }

    /// Closes an unwatched stream after a grace period, gives up waiting for a
    /// slow first frame, and notices a process that ended while nobody watched.
    fn service_stream(&mut self) -> Result<(), Fault> {
        let now = Instant::now();
        match self.stream {
            Stream::Opening(since) => {
                if now.duration_since(since) >= STREAM_OPEN_WAIT {
                    log::warn!(
                        "session {}: no first frame from the backing",
                        self.info.id.get()
                    );
                    self.stream = Stream::Open(None);
                    self.attach_waiting()?;
                } else {
                    self.next_metadata = self.next_metadata.min(since + STREAM_OPEN_WAIT);
                }
            }
            Stream::Open(None) if self.attachments.is_empty() && self.waiting.is_empty() => {
                self.stream = Stream::Open(Some(now));
                self.next_metadata = self.next_metadata.min(now + STREAM_GRACE);
            }
            Stream::Open(Some(_)) if !self.attachments.is_empty() => {
                self.stream = Stream::Open(None);
            }
            Stream::Open(Some(since)) => {
                if now.duration_since(since) >= STREAM_GRACE {
                    self.pty.set_streaming(false, &self.sender)?;
                    self.stream = Stream::Closed;
                    self.next_alive = now + ALIVE_POLL;
                } else {
                    self.next_metadata = self.next_metadata.min(since + STREAM_GRACE);
                }
            }
            Stream::Always | Stream::Open(None) => {}
            Stream::Closed => {
                if now >= self.next_alive {
                    self.next_alive = now + ALIVE_POLL;
                    if self.output_state == OutputState::Open && self.pty.try_wait().is_some() {
                        self.output_state = OutputState::Closed;
                    }
                }
            }
        }
        Ok(())
    }

    /// Replaces the terminal's content with the process's visible screen, read
    /// from the backing, while no stream keeps it current.
    fn seed_mirror(&mut self) {
        if self.stream != Stream::Closed {
            return;
        }
        let Some((size, bytes)) = self.pty.screen() else {
            return;
        };
        let _ = self.terminal.resize(size);
        self.terminal.feed(b"\x1bc");
        self.terminal.feed(&bytes);
        let _ = self.terminal.take_pty_output();
        let _ = self.terminal.resize(self.size);
    }

    fn next_wake(&self) -> Wake {
        let now = Instant::now();
        if self.next_tick.is_some_and(|tick| tick <= now) {
            return Wake::Tick;
        }
        if self.checkpoint.due().is_some_and(|due| due <= now) {
            return Wake::Checkpoint;
        }
        if self.next_metadata <= now {
            return Wake::Metadata;
        }
        let deadline = self
            .next_tick
            .unwrap_or(self.next_metadata)
            .min(self.checkpoint.due().unwrap_or(self.next_metadata))
            .min(self.next_metadata);
        let received = self
            .events
            .recv_timeout(deadline.saturating_duration_since(now));
        match received {
            Ok(event) => Wake::Event(event),
            Err(RecvTimeoutError::Timeout) if Some(deadline) == self.checkpoint.due() => {
                Wake::Checkpoint
            }
            Err(RecvTimeoutError::Timeout) if deadline == self.next_metadata => Wake::Metadata,
            Err(RecvTimeoutError::Timeout) => Wake::Tick,
            Err(RecvTimeoutError::Disconnected) => Wake::Orphaned,
        }
    }

    fn has_pending_work(&self) -> bool {
        self.output_pending
            || self.resize_pending
            || self.compress_pending
            || self.output_state == OutputState::Closed
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<(), Fault> {
        self.detection_dirty = true;
        self.terminal.feed(bytes);
        if self.detect_prompts {
            self.marks.screen_changed(Instant::now());
        }
        if self.terminal.synchronized_output()? {
            self.synchronized_since.get_or_insert_with(Instant::now);
        } else {
            self.synchronized_since = None;
        }
        let answers = self.terminal.take_pty_output();
        if !answers.is_empty() {
            self.input.send(answers.into())?;
        }
        self.output_pending = true;
        self.compress_pending = true;
        self.arm_checkpoint(false);
        Ok(())
    }

    fn arm_checkpoint(&mut self, immediately: bool) {
        if !self.checkpoints {
            return;
        }
        let now = Instant::now();
        if immediately {
            self.checkpoint.immediately(now);
        } else {
            self.checkpoint.changed(now);
        }
    }

    fn terminal_input(&mut self, input: muxy_protocol::TerminalInput) -> Result<(), Fault> {
        use muxy_protocol::TerminalInput;
        let bytes = match input {
            TerminalInput::Key(event) => self.terminal.encode_key(event)?,
            TerminalInput::Paste(bytes) if self.pty.brackets_paste() => {
                self.terminal.encode_paste_bracketed(bytes)?
            }
            TerminalInput::Paste(bytes) => self.terminal.encode_paste(bytes)?,
            TerminalInput::Focus(focused) => self.terminal.encode_focus(focused)?,
            TerminalInput::ClearScreen => return self.clear_screen(),
        };
        if !bytes.is_empty() {
            self.observe_input(&bytes);
            self.input.send(bytes.into())?;
        }
        Ok(())
    }

    fn clear_screen(&mut self) -> Result<(), Fault> {
        if self.terminal.clear_screen()? {
            self.marks.clear();
            self.persist_marks();
            self.resize_pending = true;
            self.output_pending = true;
            self.detection_dirty = true;
            self.arm_checkpoint(true);
            let redraw = self.terminal.take_pty_output();
            if !redraw.is_empty() {
                self.input.send(redraw.into())?;
            }
        }
        Ok(())
    }

    /// Follows what the user types into a backing that drops prompt marks, and
    /// starts looking for the message an Enter submits.
    fn observe_input(&mut self, bytes: &[u8]) {
        if !self.detect_prompts {
            return;
        }
        let Some(typed) = self.marks.type_in(bytes) else {
            return;
        };
        // A closed stream leaves the mirror stale: nothing on it can be found.
        if !matches!(self.stream, Stream::Always | Stream::Open(_)) {
            return;
        }
        let agent = self.pty.agent();
        // While an agent works or waits, Enter answers a dialog or queues a message.
        if matches!(agent, Some((_, AgentState::Working | AgentState::Blocked))) {
            return;
        }
        let text = if typed.trim().is_empty() {
            // A shell's recalled command is on the cursor's row. An agent's Enter
            // without text confirms something.
            if agent.is_some() {
                return;
            }
            self.cursor_line()
        } else {
            typed
        };
        self.marks.expect(&text, agent.is_some(), Instant::now());
    }

    fn cursor_line(&mut self) -> String {
        let (Ok(screen), Ok(cursor)) = (self.terminal.screen(), self.terminal.cursor()) else {
            return String::new();
        };
        screen
            .get(usize::from(cursor.row))
            .map(|row| row.runs.iter().map(|run| run.text.as_str()).collect())
            .unwrap_or_default()
    }

    /// Marks the row of a submitted message once the screen shows it.
    fn resolve_prompt_marks(&mut self) {
        if !self.detect_prompts || !self.marks.due(Instant::now()) {
            return;
        }
        let (Ok(screen), Ok(cursor)) = (self.terminal.screen(), self.terminal.cursor()) else {
            self.marks.abandon();
            return;
        };
        let Some(base) = self.pty.history_rows_now() else {
            self.marks.abandon();
            return;
        };
        if self
            .marks
            .resolve(Instant::now(), &screen, cursor.row, base)
        {
            self.output_pending = true;
        }
        self.persist_marks();
    }

    /// Hands changed marks to the backing, which keeps them across a restart.
    fn persist_marks(&mut self) {
        if let Some(marks) = self.marks.take_changed() {
            self.pty.save_prompt_marks(self.info.id, &marks);
        }
    }

    /// The screen rows where a prompt starts: those the terminal saw marked and,
    /// for a backing that drops the marks, the messages found by watching input.
    fn screen_prompts(&mut self, history: usize) -> Result<Vec<u16>, muxy_terminal::TerminalError> {
        let mut rows = self.terminal.screen_prompts()?;
        if self.detect_prompts {
            self.verify_prompt_marks(history);
            rows.extend(self.marks.screen_rows(history, self.size.rows));
            rows.sort_unstable();
            rows.dedup();
        }
        Ok(rows)
    }

    /// Moves the marks on screen to where their messages are now, and drops those
    /// whose message is gone: an application that redraws its screen (a banner that
    /// goes away) moves rows that herdr has not scrolled into scrollback yet.
    fn verify_prompt_marks(&mut self, history: usize) {
        if !matches!(self.stream, Stream::Always | Stream::Open(_))
            || !self
                .marks
                .check_due(history, self.size.rows, Instant::now())
        {
            return;
        }
        let Ok(screen) = self.terminal.screen() else {
            return;
        };
        let texts = PromptMarks::texts(&screen);
        if self.marks.stale(history, &texts) {
            // The rows moved under scrollback counted a moment ago: count it afresh.
            let base = self.pty.history_rows_now().unwrap_or(history);
            self.marks.reanchor(base, &texts);
            self.persist_marks();
        }
    }

    /// A client reports the grid of its view. The session takes that size when the
    /// client may size it; either way the client is answered with the session's screen.
    fn resize_attachment(&mut self, id: AttachmentId, size: Size) -> Result<(), Fault> {
        self.prune_viewers();
        if self.sizing.told(id, size) && size != self.size {
            self.resize(size)?;
            self.broadcast_frame(Some(id))?;
            self.output_pending = false;
            self.resize_pending = false;
        } else {
            self.repaint(id)?;
        }
        Ok(())
    }

    /// A client typed: the keyboard may move to it, and the session with it.
    fn typed(&mut self, id: AttachmentId, now: Instant) -> Result<(), Fault> {
        self.prune_viewers();
        let size = self.sizing.typed(id, now);
        self.sync_size_authority();
        if let Some(size) = size
            && size != self.size
        {
            self.resize(size)?;
        }
        Ok(())
    }

    /// Tells the backing whether a client may size the session, once that changes.
    fn sync_size_authority(&mut self) {
        if self.sizing.is_empty() {
            return;
        }
        let sizes = self.sizing.sizes();
        if sizes != self.size_authority {
            self.size_authority = sizes;
            self.pty.set_size_authority(sizes);
        }
    }

    /// Forgets clients that left without a detach, such as a closed connection.
    fn prune_viewers(&mut self) {
        let present: std::collections::HashSet<AttachmentId> = self
            .attachments
            .keys()
            .copied()
            .chain(self.waiting.iter().map(|waiting| waiting.id))
            .collect();
        self.sizing.retain(&present);
    }

    /// Answers a resize that left the session as it was: the whole screen goes to
    /// the asking client, whose view may have assumed another size.
    fn repaint(&mut self, id: AttachmentId) -> Result<(), Fault> {
        if !self.attachments.contains_key(&id) {
            return Ok(());
        }
        let rows = self.terminal.screen()?;
        let graphics = self.terminal.graphics()?;
        let cursor = self.terminal.cursor()?;
        let modes = self.terminal.modes()?;
        let Some(attachment) = self.attachments.get_mut(&id) else {
            return Ok(());
        };
        let seq = attachment.seq;
        attachment.seq += 1;
        let sent = [
            AttachmentEvent::Metadata(MetadataEvent::ScreenPrompts {
                seq,
                rows: self.prompts.clone(),
            }),
            AttachmentEvent::Metadata(MetadataEvent::Links {
                seq,
                rows: self.links.clone(),
            }),
            AttachmentEvent::Resized(ScreenFrame {
                size: self.size,
                graphics: Some(graphics),
                seq,
                reset: true,
                rows,
                cursor,
                modes,
            }),
        ]
        .into_iter()
        .all(|event| attachment.sink.send(event));
        if !sent {
            self.attachments.remove(&id);
        }
        Ok(())
    }

    fn resize(&mut self, size: Size) -> Result<(), Fault> {
        self.pty.resize(size)?;
        self.terminal.resize(size)?;
        if self.detect_prompts {
            self.marks.resized(size.cols);
            self.persist_marks();
        }
        self.size = size;
        self.resize_pending = true;
        self.arm_checkpoint(false);
        Ok(())
    }

    fn attach(
        &mut self,
        id: AttachmentId,
        channel: ChannelId,
        size: Size,
        mut sink: AttachmentSink,
    ) -> Result<(), Fault> {
        if self.attachments.is_empty() && size != self.size && !self.sizing.watching(id) {
            self.resize(size)?;
        }
        self.update_metadata();
        self.update_cursor_blinking()?;
        // Establish one common baseline before adding an attachment. Existing
        // clients receive pending changes before the new client's snapshot.
        if self.attachments.is_empty() {
            self.terminal.take_changed_rows()?;
            self.links = self.terminal.screen_links();
            let (_, history) = self.history_state()?;
            self.prompts = self.screen_prompts(history)?;
            self.frame_state = Some((self.terminal.cursor()?, self.terminal.modes()?));
        } else {
            self.broadcast_frame(None)?;
        }
        self.output_pending = false;
        self.resize_pending = false;
        let history = self.history_page(HistoryCursor(0), 200)?;
        let screen = history
            .screen
            .ok_or_else(|| io::Error::other("fresh history has no screen"))?;
        let snapshot = AttachSnapshot {
            graphics: screen.graphics,
            prompts: history.prompts,
            channel,
            size: self.size,
            rows: screen.rows,
            cursor: screen.cursor,
            modes: self.terminal.modes()?,
            title: self.metadata.title.clone(),
            directory: self.metadata.directory.clone(),
            history: history.rows,
            history_cursor: history.next,
            history_total: history.total_rows,
        };
        if sink.send(AttachmentEvent::Snapshot {
            snapshot,
            process: self.metadata.process.clone(),
        }) {
            sink.send(AttachmentEvent::Metadata(MetadataEvent::InputModes(
                self.terminal.input_modes()?,
            )));
            sink.send(AttachmentEvent::Metadata(MetadataEvent::CursorBlinking(
                self.cursor_blinking,
            )));
            sink.send(AttachmentEvent::Metadata(MetadataEvent::Links {
                seq: 0,
                rows: self.links.clone(),
            }));
            self.attachments.insert(id, Attachment { sink, seq: 1 });
        }
        Ok(())
    }

    /// The scrollback's generation and row count, from the backing when it keeps
    /// the scrollback, else from the owner's terminal.
    fn history_state(&mut self) -> Result<(u64, usize), muxy_terminal::TerminalError> {
        match self.pty.history_rows() {
            Some(rows) => Ok((1, rows)),
            None => Ok((
                self.terminal.history_generation()?,
                self.terminal.history_rows()?,
            )),
        }
    }

    fn history_page(
        &mut self,
        before: HistoryCursor,
        max_rows: u16,
    ) -> Result<HistoryPage, ServerError> {
        fn terminal_error(error: impl std::fmt::Display) -> ServerError {
            ServerError::new(
                ErrorCode::HistoryUnavailable,
                format!("terminal history: {error}"),
            )
        }
        if before.0 == 0 {
            self.seed_mirror();
        }
        let (generation, total) = self.history_state().map_err(terminal_error)?;
        let (range, next) = history_range(self.info.id, generation, before, max_rows, total)?;
        self.compress_pending = true;
        let (history, mut prompts) = if self.pty.history_rows().is_some() {
            let marks = self.marks.history_rows(&range);
            let rows = self
                .pty
                .history(range, self.size.cols)
                .map_err(terminal_error)?;
            (rows, marks)
        } else {
            self.terminal
                .history_with_prompts(range)
                .map_err(terminal_error)?
        };
        let screen = if before.0 == 0 {
            prompts.extend(
                self.screen_prompts(total)
                    .map_err(terminal_error)?
                    .into_iter()
                    .filter_map(|row| u16::try_from(history.len() + usize::from(row)).ok()),
            );
            Some(SavedScreen {
                graphics: self.terminal.graphics().map_err(terminal_error)?,
                size: self.size,
                rows: self.terminal.screen().map_err(terminal_error)?,
                cursor: self.terminal.cursor().map_err(terminal_error)?,
                reason: None,
            })
        } else {
            None
        };
        bound_history_page(
            self.info.id,
            generation,
            before,
            HistoryPage {
                prompts,
                rows: history,
                next,
                total_rows: total as u64,
                screen,
            },
        )
    }

    fn update_cursor_blinking(&mut self) -> Result<(), Fault> {
        let blinking = self.terminal.cursor_blinking()?;
        if blinking != self.cursor_blinking {
            self.cursor_blinking = blinking;
            self.attachments.retain(|_, attachment| {
                attachment
                    .sink
                    .send(AttachmentEvent::Metadata(MetadataEvent::CursorBlinking(
                        blinking,
                    )))
            });
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<(), Fault> {
        self.next_tick = None;
        self.last_tick = Some(Instant::now());
        if self.terminal.synchronized_output()? {
            let since = self.synchronized_since.get_or_insert_with(Instant::now);
            if since.elapsed() < SYNC_TIMEOUT {
                return Ok(());
            }
            self.terminal.end_synchronized_output()?;
        }
        self.synchronized_since = None;
        self.update_metadata();
        let modes = self.terminal.input_modes()?;
        if modes != self.input_modes {
            self.input_modes = modes;
            self.attachments.retain(|_, attachment| {
                attachment
                    .sink
                    .send(AttachmentEvent::Metadata(MetadataEvent::InputModes(modes)))
            });
        }
        self.update_cursor_blinking()?;
        if self.output_pending || self.resize_pending {
            self.broadcast_frame(None)?;
            self.output_pending = false;
            self.resize_pending = false;
        } else if self.compress_pending {
            self.terminal.compress_idle()?;
            self.compress_pending = false;
        }
        Ok(())
    }

    fn search(
        &mut self,
        query: &str,
        ignore_case: bool,
        before: HistoryCursor,
        max_results: u16,
    ) -> Result<SearchPage, ServerError> {
        fn failed(error: impl std::fmt::Display) -> ServerError {
            ServerError::new(ErrorCode::HistoryUnavailable, format!("search: {error}"))
        }
        let (generation, history_rows) = self.history_state().map_err(failed)?;
        let search = Search {
            session: self.info.id,
            generation,
            query,
            ignore_case,
            before,
            max_results,
            history_rows,
            screen_rows: usize::from(self.size.rows),
        };
        let range = search.range()?;
        if range.end > history_rows {
            self.seed_mirror();
        }
        self.compress_pending = true;
        let rows = range.start.min(history_rows)..range.end.min(history_rows);
        let history = if self.pty.history_rows().is_some() {
            self.pty.history(rows, self.size.cols).map_err(failed)?
        } else {
            self.terminal.history(rows).map_err(failed)?
        };
        let screen = if range.end > history_rows {
            self.terminal.screen().map_err(failed)?
        } else {
            Vec::new()
        };
        search.scan(|index| {
            Ok(if index < history_rows {
                &history[index - range.start].runs
            } else {
                &screen[index - history_rows].runs
            })
        })
    }

    fn update_metadata(&mut self) {
        let terminal = self.terminal.take_events();
        for event in &terminal {
            if let muxy_terminal::TerminalEvent::Progress(progress) = event {
                use muxy_protocol::ProgressState;
                self.detection_progress = progress.progress.map_or_else(
                    || "4;0;0".into(),
                    |p| match p.state {
                        ProgressState::Running => {
                            format!("4;1;{}", p.percent.map_or(-1, i16::from))
                        }
                        ProgressState::Indeterminate | ProgressState::Unrecognized(_) => {
                            "4;1;-1".into()
                        }
                        ProgressState::Error => "4;2".into(),
                        ProgressState::Paused => "4;4".into(),
                    },
                );
                *self
                    .progress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = *progress;
            }
        }
        let events = self.metadata.update(self.pty.as_ref(), terminal);
        if events.iter().any(|event| {
            matches!(
                event,
                MetadataEvent::Title(_)
                    | MetadataEvent::Directory(_)
                    | MetadataEvent::ForegroundProcess { .. }
            )
        }) {
            *self
                .shared_metadata
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                muxy_protocol::SessionMetadata {
                    title: self.metadata.title.clone(),
                    directory: self.metadata.directory.clone(),
                    process: self.metadata.process.clone(),
                };
        }
        for event in events {
            self.attachments.retain(|_, attachment| {
                attachment
                    .sink
                    .send(AttachmentEvent::Metadata(event.clone()))
            });
        }
        if self.metadata.agent != self.detector.provider {
            self.detection_dirty = true;
            self.detection_progress.clear();
        }
        let now = Instant::now();
        if now >= self.next_detection {
            self.next_detection = now + METADATA_POLL;
            if self.metadata.agent.is_some() {
                let reported = self.pty.agent().map(|(_, state)| state);
                let completed = if let Some(state) = reported {
                    self.detector.adopt(self.metadata.agent, state)
                } else {
                    if self.detection_dirty && !self.terminal.synchronized_output().unwrap_or(true)
                    {
                        if let Ok(screen) = self.terminal.detection_text() {
                            self.detection_screen = screen;
                        }
                        self.detection_dirty = false;
                    }
                    self.detector.update(
                        self.metadata.agent,
                        self.detection_screen.clone(),
                        &self.metadata.title,
                        &self.detection_progress,
                        now,
                    )
                };
                if let Some(provider) = self.detector.provider {
                    self.activity.update(
                        muxy_protocol::AgentActivity {
                            session: self.info.id,
                            project: self.info.project,
                            provider,
                            state: self.detector.state,
                        },
                        completed,
                    );
                }
            } else {
                self.detector = crate::detection::Detector::default();
                self.detection_screen.clear();
                self.activity.clear_agent(self.info.id);
            }
        }
        self.schedule_metadata();
    }

    /// Looks for a submitted message that is due, then sets when this runs next.
    fn schedule_metadata(&mut self) {
        self.resolve_prompt_marks();
        let mut next = Instant::now()
            + if self.metadata.agent.is_some() {
                METADATA_POLL
            } else {
                Duration::from_secs(1)
            };
        if let Some(check) = self.marks.next_check() {
            next = next.min(check);
        }
        self.next_metadata = next;
    }

    fn broadcast_frame(&mut self, resized: Option<AttachmentId>) -> Result<(), Fault> {
        if self.attachments.is_empty() {
            return Ok(());
        }
        let (generation, total_rows) = self.history_state()?;
        let history_rows = total_rows;
        let total_rows = total_rows as u64;
        if total_rows != self.history_total || generation != self.history_generation {
            self.history_total = total_rows;
            self.history_generation = generation;
            self.attachments.retain(|_, attachment| {
                attachment
                    .sink
                    .send(AttachmentEvent::Metadata(MetadataEvent::History {
                        total_rows,
                    }))
            });
        }
        let graphics = self.terminal.graphics()?;
        let graphics_changed = graphics != self.graphics || self.resize_pending;
        self.graphics = graphics;
        let frame = ScreenFrame {
            size: self.size,
            graphics: graphics_changed.then(|| self.graphics.clone()),
            seq: 0,
            reset: self.resize_pending,
            rows: self.terminal.take_changed_rows()?,
            cursor: self.terminal.cursor()?,
            modes: self.terminal.modes()?,
        };
        let links = self.terminal.screen_links();
        let prompts = self.screen_prompts(history_rows)?;
        let prompts_changed = self.prompts != prompts;
        self.prompts = prompts;
        let links_changed = self.links != links;
        self.links = links;
        let state = (frame.cursor, frame.modes);
        if !graphics_changed
            && !links_changed
            && !prompts_changed
            && !frame.reset
            && frame.rows.is_empty()
            && self.frame_state == Some(state)
        {
            return Ok(());
        }
        self.frame_state = Some(state);
        self.attachments.retain(|id, attachment| {
            let frame = ScreenFrame {
                seq: attachment.seq,
                ..frame.clone()
            };
            if prompts_changed || frame.reset {
                attachment
                    .sink
                    .send(AttachmentEvent::Metadata(MetadataEvent::ScreenPrompts {
                        seq: frame.seq,
                        rows: self.prompts.clone(),
                    }));
            }
            if links_changed || frame.reset {
                attachment
                    .sink
                    .send(AttachmentEvent::Metadata(MetadataEvent::Links {
                        seq: frame.seq,
                        rows: self.links.clone(),
                    }));
            }
            attachment.seq += 1;
            let event = if resized == Some(*id) {
                AttachmentEvent::Resized(frame)
            } else {
                AttachmentEvent::Frame(frame)
            };
            attachment.sink.send(event)
        });
        Ok(())
    }

    fn terminate(&mut self, reason: ExitReason) -> Result<ExitReason, Fault> {
        self.seed_mirror();
        self.pty.release(reason)?;
        Ok(reason)
    }

    fn drain_output(&mut self) {
        if self.stream == Stream::Closed {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.output_state == OutputState::Open {
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(OwnerEvent::Pty(PtyEvent::Output(bytes))) => {
                    self.terminal.feed(&bytes);
                    self.terminal.take_pty_output();
                }
                Ok(OwnerEvent::Pty(PtyEvent::Closed)) => self.output_state = OutputState::Closed,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    fn checkpoint(&mut self, reason: Option<ExitReason>) {
        let started = Instant::now();
        self.compress_pending = true;
        let result = self
            .terminal
            .archive()
            .map_err(io::Error::other)
            .and_then(|terminal| self.archive.save(self.info.id, terminal, reason));
        if let Err(error) = result {
            log::error!(
                "session {} could not save terminal content: {error}",
                self.info.id.get()
            );
        }
        self.checkpoint.saved(started.elapsed());
    }
}

fn exit_reason(status: ExitStatus) -> ExitReason {
    match (status.code, status.signal) {
        (Some(code), _) => ExitReason::Exited(code),
        (None, Some(signal)) => ExitReason::Signaled(signal),
        (None, None) => ExitReason::Ended,
    }
}

fn tick_deadline(last_tick: Option<Instant>, now: Instant) -> Instant {
    last_tick.map_or(now, |last| (last + TICK).max(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxy_protocol::{ServerPath, SessionId};
    use muxy_terminal::pty::{Pty, SpawnRequest};
    use std::num::NonZeroU64;
    use std::os::unix::ffi::OsStrExt;

    fn owner() -> Result<Owner, Fault> {
        let size = Size { cols: 20, rows: 3 };
        let cwd = std::env::temp_dir();
        let mut pty = Pty::spawn(SpawnRequest {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: cwd.clone(),
            env: vec![],
            size: size.into(),
        })?;
        pty.wait()?;
        owner_with(Box::new(pty))
    }

    fn owner_with(pty: Box<dyn Backing>) -> Result<Owner, Fault> {
        let size = Size { cols: 20, rows: 3 };
        let cwd = std::env::temp_dir();
        let directory = ServerPath(cwd.as_os_str().as_bytes().to_vec());
        let detect_prompts = pty.drops_prompt_marks();
        Ok(Owner {
            graphics: muxy_protocol::Graphics::default(),
            info: SessionInfo {
                project: muxy_protocol::ProjectId::from_u128(1),
                id: SessionId::from(NonZeroU64::MIN),
                directory: directory.clone(),
            },
            pty,
            terminal: Terminal::new(size, 1024)?,
            metadata: Metadata::new(directory.clone()),
            progress: crate::session::SharedProgress::default(),
            shared_metadata: std::sync::Arc::new(std::sync::Mutex::new(
                muxy_protocol::SessionMetadata {
                    title: String::new(),
                    directory,
                    process: None,
                },
            )),
            activity: std::sync::Arc::default(),
            detector: crate::detection::Detector::default(),
            detection_dirty: true,
            next_detection: Instant::now(),
            detection_screen: String::new(),
            detection_progress: String::new(),
            size,
            events: mpsc::channel().1,
            input: mpsc::channel().0,
            attachments: HashMap::new(),
            sender: mpsc::channel().0,
            stream: Stream::Always,
            waiting: Vec::new(),
            next_alive: Instant::now(),
            output_pending: false,
            resize_pending: false,
            sizing: Sizing::default(),
            size_authority: true,
            compress_pending: false,
            history_total: 0,
            history_generation: 0,
            input_modes: InputModes::default(),
            cursor_blinking: true,
            links: Vec::new(),
            prompts: Vec::new(),
            detect_prompts,
            marks: PromptMarks::default(),
            frame_state: None,
            next_tick: None,
            last_tick: None,
            synchronized_since: None,
            output_state: OutputState::Closed,
            archive: Archive::memory(1024),
            checkpoints: true,
            checkpoint: Checkpoint::idle(),
            next_metadata: Instant::now(),
        })
    }

    #[test]
    fn first_output_after_idle_renders_immediately_and_bursts_stay_on_the_tick_grid() {
        let last = Instant::now();
        let after = |millis| last + Duration::from_millis(millis);
        assert_eq!(tick_deadline(None, last), last);
        assert_eq!(tick_deadline(Some(last), last), last + TICK);
        assert_eq!(tick_deadline(Some(last), after(5)), last + TICK);
        assert_eq!(tick_deadline(Some(last), last + TICK), last + TICK);
        assert_eq!(tick_deadline(Some(last), after(17)), after(17));
        assert_eq!(tick_deadline(Some(last), after(500)), after(500));
    }

    #[test]
    fn rendering_records_the_tick_that_paces_the_next_one() -> Result<(), Fault> {
        let mut owner = owner()?;
        assert!(owner.last_tick.is_none());
        let before = Instant::now();
        owner.tick()?;
        assert!(owner.last_tick.is_some_and(|tick| tick >= before));
        Ok(())
    }

    #[test]
    fn synchronized_frames_wait_for_completion_and_recover_after_timeout() -> Result<(), Fault> {
        let mut owner = owner()?;
        let (sink, events) = mpsc::channel();
        owner.attach(AttachmentId(1), ChannelId(1), owner.size, sink.into())?;
        events.try_iter().for_each(drop);
        owner.feed(b"\x1b[?2026hpartial")?;
        owner.tick()?;
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event, AttachmentEvent::Frame(_)))
        );
        assert!(owner.output_pending);
        owner.feed(b" complete\x1b[?2026l")?;
        owner.tick()?;
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, AttachmentEvent::Frame(_)))
        );
        owner.feed(b"\x1b[?2026hstalled")?;
        owner.synchronized_since = Some(Instant::now().checked_sub(SYNC_TIMEOUT).unwrap());
        owner.tick()?;
        assert!(!owner.terminal.synchronized_output()?);
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, AttachmentEvent::Frame(_)))
        );
        Ok(())
    }

    #[test]
    fn prompt_marks_are_atomic_on_attach_and_history_reads_schedule_recompression()
    -> Result<(), Fault> {
        let mut owner = owner()?;
        for _ in 0..10 {
            owner.terminal.feed(
                b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\r\n\x1b]133;C\x07hi\r\n\x1b]133;D;0\x07",
            );
        }
        owner.terminal.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        let (sink, events) = mpsc::channel();
        owner.attach(AttachmentId(1), ChannelId(1), owner.size, sink.into())?;
        let AttachmentEvent::Snapshot { snapshot, .. } = events.recv()? else {
            return Err("expected snapshot".into());
        };
        assert_eq!(snapshot.prompts.len(), 11);
        assert_eq!(
            snapshot.prompts.last().copied().map(usize::from),
            Some(snapshot.history.len() + 2)
        );
        owner.compress_pending = false;
        let page = owner.history_page(HistoryCursor(0), 5)?;
        assert!(owner.compress_pending);
        assert_eq!(page.prompts, [1, 3, 5, 7]);
        owner.tick()?;
        assert!(!owner.compress_pending);
        owner.terminal.feed(b"\x1b[2J\x1b[H");
        owner.broadcast_frame(None)?;
        let events: Vec<_> = events.try_iter().collect();
        assert!(events.iter().any(|event| matches!(event, AttachmentEvent::Metadata(MetadataEvent::ScreenPrompts { seq: 1, rows }) if rows.is_empty())));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AttachmentEvent::Frame(frame) if frame.seq == 1))
        );
        Ok(())
    }

    /// A backing like herdr's: it keeps the scrollback, drops OSC 133 marks, and
    /// says what its agent is doing.
    struct Scrollback {
        history: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        agent: std::sync::Arc<std::sync::Mutex<Option<AgentState>>>,
    }

    impl Backing for Scrollback {
        fn start_reader(&self, _sink: Sender<OwnerEvent>) -> Result<(), ServerError> {
            Ok(())
        }

        fn take_writer(&mut self) -> Result<Box<dyn Write + Send>, ServerError> {
            Ok(Box::new(io::sink()))
        }

        fn resize(&mut self, _size: Size) -> Result<(), Fault> {
            Ok(())
        }

        fn set_cell_size(&mut self, _cell: muxy_terminal::CellSize) -> Result<(), Fault> {
            Ok(())
        }

        fn try_wait(&mut self) -> Option<ExitStatus> {
            None
        }

        fn wait(&mut self) -> Result<ExitStatus, Fault> {
            Ok(ExitStatus {
                code: None,
                signal: None,
            })
        }

        fn kill(&mut self) -> Result<(), Fault> {
            Ok(())
        }

        fn child_pid(&self) -> u32 {
            0
        }

        fn foreground_pid(&self) -> Option<u32> {
            None
        }

        fn history_rows(&self) -> Option<usize> {
            Some(self.history.load(std::sync::atomic::Ordering::Relaxed))
        }

        fn history(
            &self,
            range: std::ops::Range<usize>,
            _cols: u16,
        ) -> Result<Vec<muxy_terminal::Row>, Fault> {
            Ok(range
                .map(|_| muxy_terminal::Row {
                    index: 0,
                    runs: Vec::new(),
                })
                .collect())
        }

        fn drops_prompt_marks(&self) -> bool {
            true
        }

        fn agent(&self) -> Option<(muxy_protocol::AgentProvider, AgentState)> {
            let state = *self
                .agent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.map(|state| (muxy_protocol::AgentProvider::OhMyPi, state))
        }
    }

    type Agent = std::sync::Arc<std::sync::Mutex<Option<AgentState>>>;
    type History = std::sync::Arc<std::sync::atomic::AtomicUsize>;

    /// An owner over a herdr-like backing with `history` rows of scrollback and a
    /// watched session.
    fn herdr_owner(history: usize) -> Result<(Owner, History, Agent), Fault> {
        let history = History::new(std::sync::atomic::AtomicUsize::new(history));
        let agent = Agent::default();
        let mut owner = owner_with(Box::new(Scrollback {
            history: History::clone(&history),
            agent: Agent::clone(&agent),
        }))?;
        owner.stream = Stream::Open(None);
        Ok((owner, history, agent))
    }

    fn settle() {
        thread::sleep(Duration::from_millis(150));
    }

    #[test]
    fn a_message_submitted_through_a_backing_that_drops_marks_is_marked_where_it_shows()
    -> Result<(), Fault> {
        use std::sync::atomic::Ordering;
        let (mut owner, history, _) = herdr_owner(40)?;
        let (sink, events) = mpsc::channel();
        owner.attach(AttachmentId(1), ChannelId(1), owner.size, sink.into())?;
        events.try_iter().for_each(drop);
        owner.feed(b"answer\r\n\r\n> hello there")?;
        owner.observe_input(b"hello there\r");
        assert!(owner.marks.next_check().is_some());
        // herdr redraws: the message is on the first row and the editor is empty.
        owner.feed(b"\x1b[2J\x1b[Hhello there\r\n\r\n> ")?;
        history.store(42, Ordering::Relaxed);
        settle();
        owner.resolve_prompt_marks();
        assert!(owner.marks.next_check().is_none());
        assert!(owner.output_pending);
        // The mark is row 42 of the whole content: the first screen row.
        let page = owner.history_page(HistoryCursor(0), 5)?;
        assert_eq!(page.prompts, [5]);
        owner.broadcast_frame(None)?;
        assert!(events.try_iter().any(|event| matches!(
            event,
            AttachmentEvent::Metadata(MetadataEvent::ScreenPrompts { rows, .. }) if rows == [0]
        )));
        // Output pushes it into scrollback: it is now a history row.
        history.store(44, Ordering::Relaxed);
        let page = owner.history_page(HistoryCursor(0), 5)?;
        assert_eq!(page.prompts, [3]);
        let older = owner.history_page(page.next.ok_or("older rows")?, 5)?;
        assert!(older.prompts.is_empty());
        // A new width reflows the content, so the marks are dropped.
        owner.resize(Size { cols: 30, rows: 3 })?;
        assert!(owner.history_page(HistoryCursor(0), 5)?.prompts.is_empty());
        Ok(())
    }

    #[test]
    fn an_agent_message_is_marked_on_its_padding_row_but_not_while_the_agent_works()
    -> Result<(), Fault> {
        use std::sync::atomic::Ordering;
        let (mut owner, history, agent) = herdr_owner(10)?;
        *agent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(AgentState::Working);
        owner.observe_input(b"steer this way\r");
        assert!(owner.marks.next_check().is_none());
        // Enter without text answers a dialog.
        *agent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(AgentState::Idle);
        owner.observe_input(b"\r");
        assert!(owner.marks.next_check().is_none());

        owner.observe_input(b"fix the login redirect\r");
        owner.feed(b"\x1b[2J\x1b[H\r\nfix the login redirect\r\n> ")?;
        history.store(12, Ordering::Relaxed);
        settle();
        owner.resolve_prompt_marks();
        let page = owner.history_page(HistoryCursor(0), 5)?;
        // Absolute row 12 is the blank row above the message, the screen's first.
        assert_eq!(page.prompts, [5]);
        Ok(())
    }

    #[test]
    fn input_to_an_unwatched_session_or_a_local_pty_is_not_followed() -> Result<(), Fault> {
        let (mut unwatched, _, _) = herdr_owner(10)?;
        unwatched.stream = Stream::Closed;
        unwatched.observe_input(b"hello there\r");
        assert!(unwatched.marks.next_check().is_none());

        let mut local = owner()?;
        local.observe_input(b"hello there\r");
        assert!(local.marks.next_check().is_none());
        assert!(!local.detect_prompts);
        Ok(())
    }

    #[test]
    fn clipboard_writes_reach_current_attachments_without_replaying_on_attach() -> Result<(), Fault>
    {
        let mut owner = owner()?;
        let (first, first_events) = mpsc::channel();
        owner.attach(AttachmentId(1), ChannelId(1), owner.size, first.into())?;
        first_events.try_iter().for_each(drop);

        owner.feed(b"\x1b]52;c;b2xk\x07")?;
        owner.tick()?;
        assert!(first_events.try_iter().any(|event| matches!(
            event,
            AttachmentEvent::Metadata(MetadataEvent::ClipboardWrite(text)) if text == "old"
        )));

        owner.feed(b"\x1b]52;c;cGVuZGluZw==\x07")?;
        let (second, second_events) = mpsc::channel();
        owner.attach(AttachmentId(2), ChannelId(2), owner.size, second.into())?;
        assert!(first_events.try_iter().any(|event| matches!(
            event,
            AttachmentEvent::Metadata(MetadataEvent::ClipboardWrite(text)) if text == "pending"
        )));
        assert!(!second_events.try_iter().any(|event| matches!(
            event,
            AttachmentEvent::Metadata(MetadataEvent::ClipboardWrite(_))
        )));

        owner.feed(b"\x1b]52;c;bGl2ZQ==\x1b\\")?;
        owner.tick()?;
        for events in [&first_events, &second_events] {
            assert!(events.try_iter().any(|event| matches!(
                event,
                AttachmentEvent::Metadata(MetadataEvent::ClipboardWrite(text)) if text == "live"
            )));
        }
        Ok(())
    }

    #[test]
    fn new_attachment_receives_its_own_baseline_correction() -> Result<(), Fault> {
        let mut owner = owner()?;
        owner.feed(b"\x1b[1;1HA")?;
        let (first, first_events) = mpsc::channel();
        owner.attach(AttachmentId(1), ChannelId(1), owner.size, first.into())?;
        owner.tick()?;
        first_events.try_iter().for_each(drop);
        owner.feed(b"\x1b[1;1HB")?;
        let (second, second_events) = mpsc::channel();
        owner.attach(AttachmentId(2), ChannelId(2), owner.size, second.into())?;
        let mut displayed = match second_events.recv()? {
            AttachmentEvent::Snapshot { snapshot, .. } => snapshot.rows,
            other => return Err(format!("expected snapshot, got {other:?}").into()),
        };
        owner.feed(b"\x1b[1;1HA")?;
        owner.tick()?;
        for event in second_events.try_iter() {
            if let AttachmentEvent::Frame(frame) = event {
                for row in frame.rows {
                    let index = usize::from(row.index);
                    displayed[index] = row;
                }
            }
        }
        let current = owner.terminal.screen()?;
        assert_eq!(
            displayed, current,
            "each attachment must converge to the server screen"
        );
        Ok(())
    }

    #[test]
    fn replaced_history_invalidates_attached_caches_without_a_screen_frame() -> Result<(), Fault> {
        let mut owner = owner()?;
        let burst = |label: &str| format!("{}\x1b[2J\x1b[H", format!("{label}\r\n").repeat(10));
        owner.feed(burst("old").as_bytes())?;
        let (first, _first_events) = mpsc::channel();
        owner.attach(AttachmentId(1), ChannelId(1), owner.size, first.into())?;
        let (second, second_events) = mpsc::channel();
        owner.attach(AttachmentId(2), ChannelId(2), owner.size, second.into())?;
        let previous = match second_events.recv()? {
            AttachmentEvent::Snapshot { snapshot, .. } => snapshot,
            other => return Err(format!("expected snapshot, got {other:?}").into()),
        };
        second_events.try_iter().for_each(drop);

        owner.feed(b"\x1b[3J")?;
        owner.feed(burst("new").as_bytes())?;
        owner.tick()?;
        let current = owner.history_page(HistoryCursor(0), 200)?;
        assert_eq!(previous.history_total, current.total_rows);
        assert_ne!(previous.history, current.rows);
        let events: Vec<_> = second_events.try_iter().collect();
        assert!(matches!(
            events.as_slice(),
            [AttachmentEvent::Metadata(MetadataEvent::History { total_rows })]
                if *total_rows == current.total_rows
        ));
        Ok(())
    }

    #[test]
    fn cursor_only_mode_only_and_reset_frames_are_not_suppressed() -> Result<(), Fault> {
        let mut owner = owner()?;
        let (sink, events) = mpsc::channel();
        owner.attach(AttachmentId(1), ChannelId(1), owner.size, sink.into())?;
        events.try_iter().for_each(drop);
        for sequence in [b"\x1b[1;2H".as_slice(), b"\x1b[?2004h", b"\x1b[?1h"] {
            owner.feed(sequence)?;
            owner.tick()?;
            let frames: Vec<_> = events
                .try_iter()
                .filter_map(|event| match event {
                    AttachmentEvent::Frame(frame) => Some(frame),
                    _ => None,
                })
                .collect();
            assert_eq!(frames.len(), 1);
            assert!(frames[0].rows.is_empty());
        }
        owner.resize(Size {
            cols: owner.size.cols + 1,
            ..owner.size
        })?;
        owner.tick()?;
        assert!(events.try_iter().any(|event| matches!(event, AttachmentEvent::Frame(frame) if frame.reset && !frame.rows.is_empty())));
        Ok(())
    }

    #[test]
    fn repeated_identical_output_has_no_frame_cost() -> Result<(), Fault> {
        let mut owner = owner()?;
        let (sink, events) = mpsc::channel();
        owner.feed(b"\x1b[1;1HA")?;
        owner.attach(AttachmentId(1), ChannelId(1), owner.size, sink.into())?;
        events.try_iter().for_each(drop);
        let mut frames = 0;
        for _ in 0..10_000 {
            owner.feed(b"\x1b[1;1HA")?;
            owner.tick()?;
            frames += events
                .try_iter()
                .filter(|event| matches!(event, AttachmentEvent::Frame(_)))
                .count();
        }
        assert_eq!(frames, 0);
        Ok(())
    }

    #[test]
    fn output_and_resizes_schedule_a_checkpoint_that_saving_clears() -> Result<(), Fault> {
        let mut owner = owner()?;
        assert_eq!(owner.checkpoint.due(), None);
        let before = Instant::now();
        owner.feed(b"output")?;
        let due = owner.checkpoint.due();
        assert!(due.is_some_and(|due| due > before));
        owner.checkpoint(None);
        assert_eq!(owner.checkpoint.due(), None);
        assert!(owner.compress_pending);
        owner.resize(Size {
            cols: owner.size.cols + 1,
            ..owner.size
        })?;
        assert!(owner.checkpoint.due().is_some());
        owner.checkpoint(Some(ExitReason::Ended));
        assert_eq!(owner.checkpoint.due(), None);
        Ok(())
    }

    type Log = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// A backing that records the sizes and the size authority it is given.
    struct Recording(Log);

    impl Recording {
        fn note(&self, line: String) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(line);
        }
    }

    fn lines(log: &Log) -> Vec<String> {
        log.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    impl Backing for Recording {
        fn start_reader(&self, _sink: Sender<OwnerEvent>) -> Result<(), ServerError> {
            Ok(())
        }

        fn take_writer(&mut self) -> Result<Box<dyn Write + Send>, ServerError> {
            Ok(Box::new(io::sink()))
        }

        fn resize(&mut self, size: Size) -> Result<(), Fault> {
            self.note(format!("resize {}x{}", size.cols, size.rows));
            Ok(())
        }

        fn set_size_authority(&mut self, authority: bool) {
            self.note(format!("authority {authority}"));
        }

        fn set_cell_size(&mut self, _cell: muxy_terminal::CellSize) -> Result<(), Fault> {
            Ok(())
        }

        fn try_wait(&mut self) -> Option<ExitStatus> {
            None
        }

        fn wait(&mut self) -> Result<ExitStatus, Fault> {
            Ok(ExitStatus {
                code: None,
                signal: None,
            })
        }

        fn kill(&mut self) -> Result<(), Fault> {
            Ok(())
        }

        fn child_pid(&self) -> u32 {
            0
        }

        fn foreground_pid(&self) -> Option<u32> {
            None
        }
    }

    fn join(
        owner: &mut Owner,
        id: u64,
        watching: bool,
        told: Option<Size>,
    ) -> Result<Receiver<AttachmentEvent>, Fault> {
        let (sink, events) = mpsc::channel();
        owner.sizing.join(AttachmentId(id), watching, told);
        owner.sync_size_authority();
        let size = owner.size;
        owner.attach(
            AttachmentId(id),
            ChannelId(u32::try_from(id)?),
            size,
            sink.into(),
        )?;
        Ok(events)
    }

    #[test]
    fn a_watcher_sizes_the_session_only_once_it_types() -> Result<(), Fault> {
        let log = Log::default();
        let mut owner = owner_with(Box::new(Recording(Log::clone(&log))))?;
        let start = owner.size;
        let wide = Size { cols: 60, rows: 10 };
        let small = Size { cols: 30, rows: 8 };
        let (desktop, phone) = (AttachmentId(1), AttachmentId(2));
        let _desktop_events = join(&mut owner, 1, false, Some(wide))?;
        let phone_events = join(&mut owner, 2, true, Some(small))?;
        phone_events.try_iter().for_each(drop);

        // The phone only tells its grid: the session keeps its size, and the phone gets
        // the screen it has, whole, as the answer.
        owner.resize_attachment(phone, small)?;
        assert_eq!(owner.size, start);
        assert!(phone_events.try_iter().any(|event| matches!(
            event,
            AttachmentEvent::Resized(frame) if frame.size == start && frame.reset
        )));
        assert!(lines(&log).is_empty());

        // Nobody typed yet: a client that is not watching may size it.
        owner.resize_attachment(desktop, wide)?;
        assert_eq!(owner.size, wide);

        // The phone types: the session takes the size the phone told, and the desktop
        // is the one that only watches now.
        owner.typed(phone, Instant::now())?;
        assert_eq!(owner.size, small);
        owner.resize_attachment(desktop, Size { cols: 70, rows: 12 })?;
        assert_eq!(owner.size, small);
        // Saying the size it has is no resize either.
        owner.resize_attachment(phone, small)?;
        assert_eq!(lines(&log), ["resize 60x10", "resize 30x8"]);
        Ok(())
    }

    #[test]
    fn the_backing_follows_whether_any_client_may_size_the_session() -> Result<(), Fault> {
        let log = Log::default();
        let mut owner = owner_with(Box::new(Recording(Log::clone(&log))))?;
        let phone_events = join(&mut owner, 1, true, None)?;
        join(&mut owner, 2, false, None)?;
        owner.sync_size_authority();
        owner.sync_size_authority();
        owner.attachments.remove(&AttachmentId(2));
        owner.sizing.leave(AttachmentId(2));
        owner.sync_size_authority();
        owner.typed(AttachmentId(1), Instant::now())?;
        drop(phone_events);
        assert_eq!(
            lines(&log),
            [
                "authority false",
                "authority true",
                "authority false",
                "authority true"
            ]
        );
        Ok(())
    }
}
