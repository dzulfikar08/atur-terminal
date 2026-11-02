//! The real `atur-term-screen` program on a Mac, serving its test pattern with
//! H.264 from `VideoToolbox`, viewed with the real viewer and its
//! `VideoToolbox` decoder. Needs no permission and no display: the capture of
//! the screen is not exercised, everything after it is.
#![cfg(target_os = "macos")]
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
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use atur_screen::{Pixels, Screen, ScreenOptions, ScreenState, ScreenTransport};
use atur_screen_host::synthetic;
use atur_screen_source::status::{State, Status};

const PROGRAM: &str = env!("CARGO_BIN_EXE_atur-term-screen");
const TIMEOUT: Duration = Duration::from_secs(30);

struct UnixTransport {
    reader: Mutex<UnixStream>,
    writer: Mutex<UnixStream>,
    stream: UnixStream,
}

impl ScreenTransport for UnixTransport {
    fn recv(&self) -> Option<Vec<u8>> {
        let mut buffer = vec![0_u8; 64 * 1024];
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

/// The program, running its test pattern; ended with the test.
struct Program {
    child: Child,
    folder: PathBuf,
}

impl Program {
    /// Starts the program in `mode` (`synthetic --size WxH` or `serve`).
    fn start(name: &str, mode: &[&str]) -> Self {
        let folder = std::env::temp_dir().join(format!("atur-host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let child = Command::new(PROGRAM)
            .args(mode)
            .arg("--socket")
            .arg(folder.join("s"))
            .arg("--status")
            .arg(folder.join("status"))
            .args(["--owner", &std::process::id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self { child, folder }
    }

    /// The first status that is not `starting`.
    fn wait_decided(&self) -> Status {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.status()
                && status.state != State::Starting
            {
                return status;
            }
            assert!(Instant::now() < deadline, "the program never decided");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn status(&self) -> Option<Status> {
        Status::read(&self.folder.join("status"))
    }

    fn socket(&self) -> PathBuf {
        self.folder.join("s")
    }

    fn wait_ready(&self) -> Status {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.status() {
                match status.state {
                    State::Ready => return status,
                    State::Failed | State::Ended | State::NeedsPermission => {
                        panic!("the program reported {status:?}")
                    }
                    State::Starting => {}
                }
            }
            assert!(Instant::now() < deadline, "the program never became ready");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

fn connect(socket: &Path) -> Screen {
    let stream = UnixStream::connect(socket).unwrap();
    let transport = UnixTransport {
        reader: Mutex::new(stream.try_clone().unwrap()),
        writer: Mutex::new(stream.try_clone().unwrap()),
        stream,
    };
    Screen::start(transport, ScreenOptions::default(), || {})
}

/// The colour of the picture's background (B, G, R), for any picture the viewer
/// holds: pixels of a BGRA frame, or the planes of a `VideoToolbox` surface
/// converted as the viewer's own shader does (BT.601, full range).
fn background(screen: &Screen) -> Option<[u8; 3]> {
    let frame = screen.frame()?;
    match &frame.pixels {
        Pixels::Bgra(bytes) => {
            let width = usize::try_from(frame.width).ok()?;
            let at = (10 * width + 10) * 4;
            let pixel = bytes.get(at..at + 3)?;
            Some([pixel[0], pixel[1], pixel[2]])
        }
        Pixels::Surface(surface) => surface_background(surface.buffer()),
    }
}

fn surface_background(buffer: &core_video::pixel_buffer::CVPixelBuffer) -> Option<[u8; 3]> {
    use core_video::pixel_buffer::{
        kCVPixelBufferLock_ReadOnly, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    };

    if buffer.get_pixel_format() != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        || buffer.get_plane_count() != 2
    {
        return None;
    }
    if buffer.lock_base_address(kCVPixelBufferLock_ReadOnly) != 0 {
        return None;
    }
    let (row, column) = (12, 12);
    // SAFETY: the buffer is locked, so both planes are mapped; row and column are
    // inside the picture and chroma is subsampled by two.
    #[allow(unsafe_code, reason = "reads the planes of a locked CVPixelBuffer")]
    let (y, cb, cr) = unsafe {
        let luma = buffer
            .get_base_address_of_plane(0)
            .cast::<u8>()
            .cast_const();
        let chroma = buffer
            .get_base_address_of_plane(1)
            .cast::<u8>()
            .cast_const();
        let y = *luma.add(row * buffer.get_bytes_per_row_of_plane(0) + column);
        let chroma_row = chroma.add((row / 2) * buffer.get_bytes_per_row_of_plane(1));
        (
            y,
            *chroma_row.add((column / 2) * 2),
            *chroma_row.add((column / 2) * 2 + 1),
        )
    };
    buffer.unlock_base_address(kCVPixelBufferLock_ReadOnly);
    let (y, cb, cr) = (i32::from(y), i32::from(cb) - 128, i32::from(cr) - 128);
    let channel = |value: i32| u8::try_from(value.clamp(0, 255)).unwrap_or(u8::MAX);
    Some([
        channel((256 * y + 454 * cb + 128) >> 8),
        channel((256 * y - 88 * cb - 183 * cr + 128) >> 8),
        channel((256 * y + 359 * cr + 128) >> 8),
    ])
}

fn distance(a: [u8; 3], b: [u8; 3]) -> u8 {
    a.iter()
        .zip(b)
        .map(|(&x, y)| x.abs_diff(y))
        .max()
        .unwrap_or(0)
}

#[test]
fn the_encoded_pattern_decodes_in_the_viewer_with_its_colours() {
    let program = Program::start("pattern", &["synthetic", "--size", "640x360"]);
    let status = program.wait_ready();
    assert_eq!((status.width, status.height), (640, 360));
    let screen = connect(&program.socket());
    let deadline = Instant::now() + TIMEOUT;
    let mut seen: Vec<[u8; 3]> = Vec::new();
    loop {
        if let ScreenState::Closed(reason) = screen.state() {
            panic!("the viewer closed: {reason}");
        }
        if let Some(colour) = background(&screen) {
            // Every colour the pattern shows is one of three far apart ones.
            let nearest = synthetic::COLOURS
                .iter()
                .find(|expected| distance(colour, **expected) <= 64);
            if let Some(expected) = nearest
                && !seen.contains(expected)
            {
                seen.push(*expected);
            }
        }
        if seen.len() >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "saw only {seen:?}; stats {:?}",
            screen.stats()
        );
        thread::sleep(Duration::from_millis(20));
    }
    let stats = screen.stats();
    assert!(
        stats.codec.starts_with("h264"),
        "codec was {:?}",
        stats.codec
    );
    let ScreenState::Ready(info) = screen.state() else {
        panic!("the viewer is not ready");
    };
    assert_eq!((info.width, info.height), (640, 360));
}

/// The real capture. On a machine where Screen Recording is not allowed for the
/// test (a CI runner, for one) the program must say so instead of failing
/// silently: that is what is checked, and why the capture itself is skipped.
/// Where it is allowed, the screen is viewed for real.
#[test]
fn the_screen_is_shared_or_the_missing_permission_is_reported() {
    let program = Program::start("screen", &["serve"]);
    let status = program.wait_decided();
    match status.state {
        State::NeedsPermission => {
            assert_eq!(status.screen_recording, Some(false));
            assert!(
                status.message.contains("Screen & System Audio Recording"),
                "the message does not say where to allow it: {}",
                status.message
            );
            eprintln!(
                "capture skipped: Screen Recording is not allowed for this process here ({})",
                status.message
            );
        }
        State::Failed => {
            assert!(!status.message.is_empty());
            eprintln!("capture skipped: {}", status.message);
        }
        State::Ready => {
            let screen = connect(&program.socket());
            let deadline = Instant::now() + TIMEOUT;
            while screen.frame().is_none() {
                assert!(Instant::now() < deadline, "no picture of the real screen");
                thread::sleep(Duration::from_millis(50));
            }
            assert!(screen.stats().codec.starts_with("h264"));
        }
        State::Starting | State::Ended => panic!("unexpected status {status:?}"),
    }
}
