//! The server against the real viewer (`atur-screen`), over a Unix socket: the
//! test pattern as ZRLE pixels, a change of size, mouse, keyboard and the
//! clipboard in both directions. Runs on every platform; the H.264 path has
//! its own test on macOS.
#![allow(
    missing_debug_implementations,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use atur_screen::{Pixels, PointerButtons, Screen, ScreenOptions, ScreenState, ScreenTransport};
use atur_screen_host::serve::{Broadcast, Encoder, Host, Server, Stopper};
use atur_screen_host::synthetic;

const TIMEOUT: Duration = Duration::from_secs(10);

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

struct UnixTransport {
    reader: Mutex<UnixStream>,
    writer: Mutex<UnixStream>,
    stream: UnixStream,
}

impl UnixTransport {
    fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        Self {
            reader: Mutex::new(stream.try_clone().unwrap()),
            writer: Mutex::new(stream.try_clone().unwrap()),
            stream,
        }
    }
}

impl ScreenTransport for UnixTransport {
    fn recv(&self) -> Option<Vec<u8>> {
        let mut buffer = vec![0_u8; 16 * 1024];
        match self.reader.lock().unwrap().read(&mut buffer) {
            Ok(0) | Err(_) => None,
            Ok(count) => {
                buffer.truncate(count);
                Some(buffer)
            }
        }
    }

    fn send(&self, bytes: &[u8]) -> Result<(), String> {
        self.writer
            .lock()
            .unwrap()
            .write_all(bytes)
            .map_err(|error| error.to_string())
    }

    fn processed(&self, _bytes: usize) {}

    fn close(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

#[derive(Debug, PartialEq)]
enum Event {
    Pointer(u8, u16, u16),
    Key(u32, bool),
}

#[derive(Default)]
struct TestHost {
    events: Mutex<Vec<Event>>,
    clipboard: Mutex<Option<String>>,
    changes: AtomicU64,
    released: AtomicU64,
}

impl TestHost {
    fn events(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|event| format!("{event:?}"))
            .collect()
    }

    fn copy(&self, text: &str) {
        *self.clipboard.lock().unwrap() = Some(text.to_owned());
        self.changes.fetch_add(1, Ordering::AcqRel);
    }
}

impl Host for TestHost {
    fn name(&self) -> String {
        "test pattern".into()
    }

    fn encoder(&self, _width: u32, _height: u32) -> Option<Box<dyn Encoder>> {
        None
    }

    fn input_allowed(&self) -> bool {
        true
    }

    fn pointer(&self, buttons: u8, x: u16, y: u16) {
        self.events
            .lock()
            .unwrap()
            .push(Event::Pointer(buttons, x, y));
    }

    fn key(&self, keysym: u32, down: bool) {
        self.events.lock().unwrap().push(Event::Key(keysym, down));
    }

    fn release_all(&self) {
        self.released.fetch_add(1, Ordering::AcqRel);
    }

    fn clipboard_text(&self) -> Option<String> {
        self.clipboard.lock().unwrap().clone()
    }

    fn clipboard_changes(&self) -> u64 {
        self.changes.load(Ordering::Acquire)
    }

    fn set_clipboard_text(&self, text: &str) -> u64 {
        *self.clipboard.lock().unwrap() = Some(text.to_owned());
        self.changes.fetch_add(1, Ordering::AcqRel) + 1
    }
}

struct Fixture {
    path: PathBuf,
    host: Arc<TestHost>,
    broadcast: Arc<Broadcast>,
    stopper: Stopper,
    stop: Arc<AtomicBool>,
}

impl Fixture {
    fn start(size: (u32, u32)) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "atur-host-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let host = Arc::new(TestHost::default());
        let broadcast = Broadcast::new(size);
        let dyn_host: Arc<dyn Host> = host.clone();
        let server = Server::bind(&path, dyn_host, Arc::clone(&broadcast), false).unwrap();
        let stopper = server.stopper();
        thread::spawn(move || server.run());
        let fixture = Self {
            path,
            host,
            broadcast,
            stopper,
            stop: Arc::new(AtomicBool::new(false)),
        };
        fixture.paint(size);
        fixture
    }

    /// Publishes the test pattern at `size` from a thread of its own.
    fn paint(&self, size: (u32, u32)) {
        let broadcast = Arc::clone(&self.broadcast);
        let stop = Arc::clone(&self.stop);
        thread::spawn(move || synthetic::run(&broadcast, size, &stop));
    }

    fn viewer(&self) -> Screen {
        Screen::start(
            UnixTransport::connect(&self.path),
            ScreenOptions {
                view_only: false,
                prefer_h264: false,
            },
            || {},
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.stopper.stop();
    }
}

fn corner(screen: &Screen) -> Option<(u32, u32, [u8; 3])> {
    let frame = screen.frame()?;
    #[allow(
        irrefutable_let_patterns,
        reason = "macOS pictures can also be surfaces"
    )]
    let Pixels::Bgra(bytes) = &frame.pixels else {
        return None;
    };
    let width = usize::try_from(frame.width).ok()?;
    let offset = (10 * width + 10) * 4;
    let pixel = bytes.get(offset..offset + 3)?;
    Some((frame.width, frame.height, [pixel[0], pixel[1], pixel[2]]))
}

fn wait_ready(screen: &Screen) -> (u32, u32) {
    let mut size = (0, 0);
    wait_for("the viewer to be ready", || {
        if let ScreenState::Ready(info) = screen.state() {
            size = (info.width, info.height);
            true
        } else {
            false
        }
    });
    size
}

#[test]
fn the_viewer_sees_the_pattern_change_through_zrle() {
    let fixture = Fixture::start((320, 200));
    let screen = fixture.viewer();
    assert_eq!(wait_ready(&screen), (320, 200));
    let mut seen = Vec::new();
    wait_for("two background colours", || {
        if let Some((width, height, colour)) = corner(&screen) {
            assert_eq!((width, height), (320, 200));
            if !seen.contains(&colour) {
                seen.push(colour);
            }
        }
        seen.len() >= 2
    });
    for colour in &seen {
        assert!(
            synthetic::COLOURS.contains(colour),
            "unexpected colour {colour:?}"
        );
    }
    assert_eq!(screen.stats().codec, "zrle");
}

#[test]
fn a_new_size_reaches_the_viewer() {
    let fixture = Fixture::start((320, 200));
    let screen = fixture.viewer();
    wait_ready(&screen);
    wait_for("the first picture", || corner(&screen).is_some());
    fixture.stop.store(true, Ordering::Release);
    thread::sleep(Duration::from_millis(100));
    fixture.stop.store(false, Ordering::Release);
    fixture.paint((400, 240));
    wait_for("the new size", || {
        corner(&screen).is_some_and(|(width, height, _)| (width, height) == (400, 240))
    });
}

#[test]
fn mouse_keyboard_and_release_reach_the_host() {
    let fixture = Fixture::start((320, 200));
    let screen = fixture.viewer();
    wait_ready(&screen);
    screen.pointer(
        5,
        6,
        PointerButtons {
            left: true,
            ..PointerButtons::default()
        },
    );
    screen.key(0x61, true);
    screen.key(0x61, false);
    wait_for("the events", || fixture.host.events().len() >= 3);
    let events = fixture.host.events();
    assert!(
        events.contains(&"Pointer(1, 5, 6)".to_owned()),
        "{events:?}"
    );
    assert!(events.contains(&"Key(97, true)".to_owned()), "{events:?}");
    assert!(events.contains(&"Key(97, false)".to_owned()), "{events:?}");
    screen.close();
    wait_for("the release", || {
        fixture.host.released.load(Ordering::Acquire) >= 1
    });
}

#[test]
fn the_clipboard_travels_both_ways() {
    let fixture = Fixture::start((320, 200));
    let screen = fixture.viewer();
    wait_ready(&screen);
    // Until the viewer knows the extended clipboard works it sends legacy text; both arrive.
    wait_for("the clipboard to reach the host", || {
        screen.send_clipboard("h\u{e9}llo");
        fixture.host.clipboard_text().as_deref() == Some("h\u{e9}llo")
    });
    fixture.host.copy("from the mac");
    let mut received = None;
    wait_for("the Mac's clipboard to reach the viewer", || {
        // Text the viewer itself sent may come back first; the newest is what counts.
        if let Some(text) = screen.take_clipboard() {
            received = Some(text);
        }
        received.as_deref() == Some("from the mac")
    });
}
