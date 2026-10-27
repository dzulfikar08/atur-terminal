use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use ratatui::crossterm::{cursor, event, execute, terminal};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGSTOP, SIGTERM, SIGTSTP};

pub(crate) fn require_interactive() -> io::Result<()> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        Ok(())
    } else {
        Err(io::Error::other(
            "interactive mode requires terminal stdin and stdout",
        ))
    }
}

/// Whether this runs in a login over SSH, away from the computer's screen.
pub(crate) fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

pub(crate) type Terminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>;

enum Signal {
    Input(io::Result<event::Event>),
    Redraw,
}

/// Wakes the event loop to draw again, at most once until it has.
#[derive(Clone)]
pub(crate) struct Waker {
    sender: SyncSender<Signal>,
    pending: Arc<AtomicBool>,
}

impl Waker {
    pub(crate) fn wake(&self) {
        if self.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.sender.try_send(Signal::Redraw).is_err() {
            self.pending.store(false, Ordering::Release);
        }
    }
}

pub(crate) struct Events {
    receiver: Receiver<Signal>,
    waker: Waker,
    reader: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Events {
    pub(crate) fn start() -> io::Result<Self> {
        let mut events = Self::detached();
        let sender = events.waker.sender.clone();
        let stop = Arc::clone(&events.stop);
        events.reader = Some(
            thread::Builder::new()
                .name("muxy-tui-host-input".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let event = match event::poll(Duration::from_millis(16)) {
                            Ok(false) => continue,
                            Ok(true) => event::read(),
                            Err(error) => Err(error),
                        };
                        let failed = event.is_err();
                        if sender.send(Signal::Input(event)).is_err() || failed {
                            break;
                        }
                    }
                })?,
        );
        Ok(events)
    }

    fn detached() -> Self {
        let (sender, receiver) = mpsc::sync_channel(128);
        Self {
            receiver,
            waker: Waker {
                sender,
                pending: Arc::new(AtomicBool::new(false)),
            },
            reader: None,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn waker(&self) -> Waker {
        self.waker.clone()
    }

    /// Waits for input or a requested redraw, giving `None` for the latter
    /// or when nothing happened within a frame.
    pub(crate) fn next(&self) -> io::Result<Option<event::Event>> {
        let signal = self.receiver.recv_timeout(Duration::from_millis(16));
        if let Ok(Signal::Input(event)) = signal {
            return event.map(Some);
        }
        if self
            .reader
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
        {
            return Err(io::Error::other("Terminal input closed"));
        }
        if matches!(signal, Ok(Signal::Redraw)) {
            self.waker.pending.store(false, Ordering::Release);
        }
        Ok(None)
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Crossterm 0.29's Mio reader can spin on a revoked TTY. Never join it:
        // the main loop still restores the terminal and exits this CLI process.
    }
}

pub(crate) struct Host {
    pub terminal: Terminal,
    pub stop: Arc<AtomicBool>,
    suspend: Arc<AtomicBool>,
    _signals: Signals,
}

impl Host {
    pub(crate) fn enter() -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let suspend = Arc::new(AtomicBool::new(false));
        let mut signals = Signals::default();
        for signal in [SIGHUP, SIGINT, SIGTERM] {
            signals
                .0
                .push(signal_hook::flag::register(signal, Arc::clone(&stop))?);
        }
        signals
            .0
            .push(signal_hook::flag::register(SIGTSTP, Arc::clone(&suspend))?);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        let mut host = Self {
            terminal: Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))?,
            stop,
            suspend,
            _signals: signals,
        };
        host.activate()?;
        Ok(host)
    }

    fn activate(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            cursor::Hide,
            event::EnableBracketedPaste,
            event::EnableFocusChange,
            event::EnableMouseCapture
        )?;
        self.terminal.clear()
    }

    pub(crate) fn resume_if_needed(&mut self) -> io::Result<bool> {
        if !self.suspend.swap(false, Ordering::AcqRel) {
            return Ok(false);
        }
        restore();
        signal_hook::low_level::raise(SIGSTOP)?;
        self.activate()?;
        Ok(true)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        restore();
    }
}

#[derive(Default)]
struct Signals(Vec<signal_hook::SigId>);

impl Drop for Signals {
    fn drop(&mut self) {
        for signal in self.0.drain(..) {
            signal_hook::low_level::unregister(signal);
        }
    }
}

fn restore() {
    let _ = execute!(
        io::stdout(),
        event::DisableMouseCapture,
        event::DisableFocusChange,
        event::DisableBracketedPaste,
        cursor::SetCursorStyle::DefaultUserShape,
        cursor::Show,
        terminal::LeaveAlternateScreen
    );
    let _ = io::stdout().flush();
    discard_pending_input();
    let _ = terminal::disable_raw_mode();
}

/// Reads away mouse reports already on their way, so none reach the shell
/// once the terminal stops reporting.
fn discard_pending_input() {
    for _ in 0..64 {
        if !event::poll(Duration::from_millis(5)).unwrap_or(false) || event::read().is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_restores_terminal_settings_and_output_modes() -> Result<(), Box<dyn std::error::Error>>
    {
        if std::env::var_os("MUXY_TEST_HOST_PANIC").is_some() {
            let _host = Host::enter()?;
            panic!("deliberate terminal cleanup test");
        }
        let output = std::process::Command::new("python3")
            .args(["-c", include_str!("terminal/panic.py")])
            .arg(std::env::current_exe()?)
            .env("TERM", "xterm-256color")
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[test]
    fn a_frame_wakes_the_loop_through_the_input_queue_once_per_draw() -> io::Result<()> {
        let events = Events::detached();
        let waker = events.waker();
        for _ in 0..1000 {
            waker.wake();
        }
        assert!(events.next()?.is_none());
        assert!(events.receiver.try_recv().is_err());
        waker.wake();
        assert!(matches!(events.receiver.try_recv(), Ok(Signal::Redraw)));
        Ok(())
    }

    #[test]
    fn a_full_input_queue_does_not_lose_later_wakes() {
        let events = Events::detached();
        let waker = events.waker();
        while waker
            .sender
            .try_send(Signal::Input(Ok(event::Event::FocusGained)))
            .is_ok()
        {}
        waker.wake();
        assert!(!waker.pending.load(Ordering::Acquire));
        while events.receiver.try_recv().is_ok() {}
        waker.wake();
        assert!(matches!(events.receiver.try_recv(), Ok(Signal::Redraw)));
    }
}
