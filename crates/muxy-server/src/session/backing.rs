//! What a session owner needs from whatever runs the terminal's process.
//!
//! The default backing is a PTY this server spawned. A herdr backing drives a
//! terminal that a herdr server owns, so the process survives this server.

use std::error::Error;
use std::io::Write;
use std::ops::Range;
use std::sync::mpsc::Sender;

use muxy_protocol::{AgentProvider, AgentState, ExitReason, MouseEvent, Size};
use muxy_terminal::pty::{ExitStatus, Pty};
use muxy_terminal::{CellSize, Row};

use super::owner::OwnerEvent;
use crate::error::ServerError;

pub(crate) type Fault = Box<dyn Error + Send + Sync>;

/// A terminal's process and its byte stream, as the session owner sees it.
pub(crate) trait Backing: Send {
    /// Starts delivering the terminal's output to `sink`; ends with `PtyEvent::Closed`.
    fn start_reader(&self, sink: Sender<OwnerEvent>) -> Result<(), ServerError>;

    fn take_writer(&mut self) -> Result<Box<dyn Write + Send>, ServerError>;

    fn resize(&mut self, size: Size) -> Result<(), Fault>;

    fn set_cell_size(&mut self, cell: CellSize) -> Result<(), Fault>;

    fn try_wait(&mut self) -> Option<ExitStatus>;

    fn wait(&mut self) -> Result<ExitStatus, Fault>;

    /// Ends the process.
    fn kill(&mut self) -> Result<(), Fault>;

    /// Ends the session's use of the process: `Ended` and faults end it, while
    /// `ServerStopped` lets a backing whose process outlives this server detach.
    fn release(&mut self, reason: ExitReason) -> Result<ExitStatus, Fault> {
        let _ = reason;
        let _ = self.kill();
        self.wait()
    }

    fn child_pid(&self) -> u32;

    fn foreground_pid(&self) -> Option<u32>;

    /// Sends a mouse event natively. `None` means the owner encodes it from its
    /// own terminal's modes.
    fn mouse(&mut self, event: &MouseEvent) -> Option<Result<(), Fault>> {
        let _ = event;
        None
    }

    /// Whether the other side re-wraps pasted text for the application's own
    /// bracketed-paste mode, so the owner must always bracket it.
    fn brackets_paste(&self) -> bool {
        false
    }

    /// Whether terminal content survives elsewhere, so the owner skips its
    /// periodic checkpoints and saves only when the session ends.
    fn persists_content(&self) -> bool {
        false
    }

    /// Whether output is streamed only while a client watches the session. Such a
    /// backing starts no reader at session start; the owner calls `set_streaming`.
    fn on_demand(&self) -> bool {
        false
    }

    /// Opens or closes the output stream of an on-demand backing. Opening ends
    /// with a first `PtyEvent::Output` that repaints the whole screen.
    fn set_streaming(&mut self, on: bool, sink: &Sender<OwnerEvent>) -> Result<(), Fault> {
        let _ = (on, sink);
        Ok(())
    }

    /// The visible screen as ANSI text with the size it has, read on demand while
    /// no stream runs.
    fn screen(&self) -> Option<(Size, Vec<u8>)> {
        None
    }

    /// How many rows of scrollback the process's owner keeps, when that is not
    /// the owner's own terminal.
    fn history_rows(&self) -> Option<usize> {
        None
    }

    /// Rows `range` of that scrollback, oldest first.
    fn history(&self, range: Range<usize>, cols: u16) -> Result<Vec<Row>, Fault> {
        let _ = (range, cols);
        Ok(Vec::new())
    }

    /// The title the application set, when the byte stream does not carry it.
    fn title(&self) -> Option<String> {
        None
    }

    /// The agent the process's owner recognised and what it is doing.
    fn agent(&self) -> Option<(AgentProvider, AgentState)> {
        None
    }
}

impl Backing for Pty {
    fn start_reader(&self, sink: Sender<OwnerEvent>) -> Result<(), ServerError> {
        Pty::start_reader(self, sink)
            .map(drop)
            .map_err(ServerError::spawn_failed)
    }

    fn take_writer(&mut self) -> Result<Box<dyn Write + Send>, ServerError> {
        Pty::take_writer(self).map_err(ServerError::spawn_failed)
    }

    fn resize(&mut self, size: Size) -> Result<(), Fault> {
        Ok(Pty::resize(self, size.into())?)
    }

    fn set_cell_size(&mut self, cell: CellSize) -> Result<(), Fault> {
        Ok(Pty::set_cell_size(self, cell)?)
    }

    fn try_wait(&mut self) -> Option<ExitStatus> {
        Pty::try_wait(self)
    }

    fn wait(&mut self) -> Result<ExitStatus, Fault> {
        Ok(Pty::wait(self)?)
    }

    fn kill(&mut self) -> Result<(), Fault> {
        Ok(Pty::kill(self)?)
    }

    fn child_pid(&self) -> u32 {
        Pty::child_pid(self)
    }

    fn foreground_pid(&self) -> Option<u32> {
        Pty::foreground_pid(self)
    }
}
