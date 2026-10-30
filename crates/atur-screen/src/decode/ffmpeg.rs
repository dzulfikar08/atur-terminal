//! H.264 decoding through an `ffmpeg` child process (Linux and other Unix systems).
//!
//! The access units go to ffmpeg's stdin as an Annex B stream; ffmpeg writes
//! BGRA frames of a fixed size to stdout, where a reader thread picks them up
//! and hands them to the sink. The picture size comes from the SPS, so the
//! process is restarted when it changes and whenever the server resets the
//! context. VA-API decoding is tried first when the machine has a render node;
//! when ffmpeg dies without having produced a picture, the decoder falls back
//! to software decoding for good.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::nal::{self, NAL_IDR_SLICE, NAL_SPS};
use super::{DecodeError, FrameSink, H264Decoder};
use crate::{Frame, Pixels};

/// Access units that may wait for the writer thread.
const QUEUE_DEPTH: usize = 8;
/// How long `decode` waits for room in the queue before it declares ffmpeg overloaded.
const SEND_TIMEOUT: Duration = Duration::from_millis(200);
/// The same while ffmpeg is still starting up and has not produced a picture yet.
const STARTUP_SEND_TIMEOUT: Duration = Duration::from_secs(2);
/// Pictures above this many pixels (8K) are treated as a corrupt SPS.
const MAX_PIXELS: u64 = 8192 * 4608;
/// An access unit delimiter. ffmpeg's H.264 parser only finishes a picture when it sees
/// the start of the next one; with nothing else coming (a still screen) the last picture
/// would stay inside ffmpeg, so every unit is followed by this marker.
const ACCESS_UNIT_DELIMITER: [u8; 6] = [0, 0, 0, 1, 0x09, 0xf0];

/// Whether an `ffmpeg` with an H.264 decoder is on the PATH (checked once).
static FFMPEG_AVAILABLE: LazyLock<bool> = LazyLock::new(|| {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-decoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some("h264")),
        _ => false,
    }
});

/// The output option that stops ffmpeg from dropping or repeating pictures to fit the
/// timestamps it invents for a raw stream. Without it a burst of units loses a picture.
/// It is `-fps_mode passthrough` from ffmpeg 5.1 and `-vsync 0` before, so each is tried.
static PASSTHROUGH: LazyLock<[&'static str; 2]> = LazyLock::new(|| {
    if accepts(["-fps_mode", "passthrough"]) {
        ["-fps_mode", "passthrough"]
    } else {
        ["-vsync", "0"]
    }
});

/// Whether this ffmpeg takes the output option `option` (it encodes one frame of a
/// generated picture to nowhere).
fn accepts(option: [&str; 2]) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "nullsrc=s=16x16:d=0.1"])
        .args(["-frames:v", "1"])
        .args(option)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The first DRM render node, which VA-API opens.
fn find_render_node() -> Option<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().next()
}

/// Flags shared between a `Process` and its threads.
#[derive(Default)]
struct Shared {
    /// ffmpeg's output ended or its input broke.
    exited: AtomicBool,
    /// The process is being torn down: do not publish anything anymore.
    stop: AtomicBool,
    /// Pictures published so far.
    frames: AtomicU64,
}

/// One running ffmpeg for one picture size.
struct Process {
    child: Child,
    queue: Option<SyncSender<Vec<u8>>>,
    threads: Vec<JoinHandle<()>>,
    shared: Arc<Shared>,
    size: (u32, u32),
    vaapi: bool,
}

impl Process {
    fn start(
        size: (u32, u32),
        vaapi_device: Option<&Path>,
        sink: &FrameSink,
    ) -> Result<Self, DecodeError> {
        let frame_len = picture_bytes(size)?;
        let mut command = Command::new("ffmpeg");
        // Measured against ffmpeg 9: `-fflags nobuffer` makes the demuxer hold back about one
        // key-frame interval of pictures, and the rawvideo encoder's frame threads hold back
        // a few more unless the output is limited to one thread (`-threads 1` below).
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-fflags",
            "discardcorrupt",
            "-flags",
            "low_delay",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
        ]);
        if let Some(device) = vaapi_device {
            command
                .args(["-hwaccel", "vaapi", "-hwaccel_device"])
                .arg(device);
        }
        command.args([
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-an",
            "-sn",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-threads",
            "1",
            "-flush_packets",
            "1",
        ]);
        command.args(PASSTHROUGH.iter());
        command.arg("pipe:1");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| DecodeError(format!("cannot start ffmpeg: {error}")))?;

        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            terminate(&mut child);
            return Err(DecodeError("cannot connect to ffmpeg".to_string()));
        };

        let (queue, receiver) = mpsc::sync_channel::<Vec<u8>>(QUEUE_DEPTH);
        let mut process = Self {
            child,
            queue: Some(queue),
            threads: Vec::new(),
            shared: Arc::new(Shared::default()),
            size,
            vaapi: vaapi_device.is_some(),
        };

        let shared = Arc::clone(&process.shared);
        process.spawn_thread("ffmpeg-writer", move || {
            write_loop(stdin, &receiver, &shared);
        })?;
        let shared = Arc::clone(&process.shared);
        let sink = Arc::clone(sink);
        process.spawn_thread("ffmpeg-reader", move || {
            read_loop(stdout, frame_len, size, &sink, &shared);
        })?;
        process.spawn_thread("ffmpeg-log", move || log_loop(stderr))?;
        Ok(process)
    }

    fn spawn_thread(
        &mut self,
        name: &str,
        body: impl FnOnce() + Send + 'static,
    ) -> Result<(), DecodeError> {
        let handle = thread::Builder::new()
            .name(name.to_string())
            .spawn(body)
            .map_err(|error| DecodeError(format!("cannot start a decoder thread: {error}")))?;
        self.threads.push(handle);
        Ok(())
    }

    /// Queues one access unit; fails when ffmpeg cannot keep up or is gone.
    fn send(&self, mut unit: Vec<u8>) -> Result<(), DecodeError> {
        let Some(queue) = self.queue.as_ref() else {
            return Err(DecodeError("ffmpeg is not running".to_string()));
        };
        let timeout = if self.produced_frames() {
            SEND_TIMEOUT
        } else {
            STARTUP_SEND_TIMEOUT
        };
        let deadline = Instant::now() + timeout;
        loop {
            match queue.try_send(unit) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    return Err(DecodeError("ffmpeg stopped".to_string()));
                }
                Err(TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        return Err(DecodeError("ffmpeg cannot keep up".to_string()));
                    }
                    unit = back;
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    fn has_exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }

    fn produced_frames(&self) -> bool {
        self.shared.frames.load(Ordering::Acquire) > 0
    }

    /// Kills ffmpeg, reaps it and joins the threads; no picture is published afterwards.
    fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.queue = None;
        terminate(&mut self.child);
        for handle in self.threads.drain(..) {
            // A panicked thread has nothing left to clean up.
            let _ = handle.join();
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Kills and reaps a child process.
fn terminate(child: &mut Child) {
    // The process may have exited already; both calls then fail harmlessly.
    let _ = child.kill();
    let _ = child.wait();
}

/// Bytes of one BGRA picture.
fn picture_bytes((width, height): (u32, u32)) -> Result<usize, DecodeError> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels == 0 || pixels > MAX_PIXELS {
        return Err(DecodeError(format!(
            "unsupported picture size {width}x{height}"
        )));
    }
    usize::try_from(pixels * 4).map_err(|_| DecodeError("picture too large".to_string()))
}

fn write_loop(mut stdin: ChildStdin, receiver: &mpsc::Receiver<Vec<u8>>, shared: &Shared) {
    for unit in receiver {
        if stdin.write_all(&unit).and_then(|()| stdin.flush()).is_err() {
            break;
        }
    }
    shared.exited.store(true, Ordering::Release);
}

fn read_loop(
    mut stdout: ChildStdout,
    frame_len: usize,
    (width, height): (u32, u32),
    sink: &FrameSink,
    shared: &Shared,
) {
    loop {
        // One allocation per picture that the frame then owns: zero-filled
        // `Arc<[u8]>`, read into in place.
        let mut bytes: Arc<[u8]> = std::iter::repeat_n(0u8, frame_len).collect();
        let Some(buffer) = Arc::get_mut(&mut bytes) else {
            break;
        };
        if stdout.read_exact(buffer).is_err() || shared.stop.load(Ordering::Acquire) {
            break;
        }
        shared.frames.fetch_add(1, Ordering::AcqRel);
        sink(Frame {
            id: 0,
            width,
            height,
            pixels: Pixels::Bgra(bytes),
        });
    }
    shared.exited.store(true, Ordering::Release);
}

fn log_loop(stderr: ChildStderr) {
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        log::warn!("ffmpeg: {line}");
    }
}

/// Decodes H.264 with an ffmpeg process.
pub(super) struct FfmpegDecoder {
    sink: FrameSink,
    /// The render node to hand to VA-API; `None` when there is none or it failed once.
    vaapi_device: Option<PathBuf>,
    process: Option<Process>,
}

impl FfmpegDecoder {
    /// `None` when there is no usable ffmpeg on this machine.
    pub(super) fn new(sink: FrameSink) -> Option<Self> {
        if !*FFMPEG_AVAILABLE {
            return None;
        }
        Some(Self {
            sink,
            vaapi_device: find_render_node(),
            process: None,
        })
    }

    /// Reaps a process that exited by itself and gives up on VA-API when it died
    /// before showing a single picture.
    fn collect_exited(&mut self) {
        let Some(process) = self.process.as_ref() else {
            return;
        };
        if !process.has_exited() {
            return;
        }
        if process.vaapi && !process.produced_frames() {
            log::info!("ffmpeg exited without a picture with VA-API; decoding in software");
            self.vaapi_device = None;
        }
        self.process = None;
    }
}

impl H264Decoder for FfmpegDecoder {
    fn name(&self) -> String {
        let label = if self.vaapi_device.is_some() {
            "ffmpeg (vaapi)"
        } else {
            "ffmpeg (software)"
        };
        label.to_string()
    }

    fn decode(&mut self, annex_b: &[u8], reset: bool) -> Result<(), DecodeError> {
        let nals = nal::split_annex_b(annex_b);
        if nals.is_empty() {
            return Err(DecodeError("H.264 data without NAL units".to_string()));
        }
        self.collect_exited();
        if reset {
            self.process = None;
        }

        let size = nals
            .iter()
            .find(|unit| nal::nal_type(unit) == Some(NAL_SPS))
            .and_then(|unit| nal::sps_dimensions(unit));
        let size_changed = matches!(
            (size, self.process.as_ref()),
            (Some(new), Some(process)) if process.size != new
        );
        if size_changed {
            self.process = None;
        }

        if self.process.is_none() {
            let has_key_frame = nals
                .iter()
                .any(|unit| nal::nal_type(unit) == Some(NAL_IDR_SLICE));
            let Some(size) = size.filter(|_| has_key_frame) else {
                return Err(DecodeError("waiting for a key frame".to_string()));
            };
            self.process = Some(Process::start(
                size,
                self.vaapi_device.as_deref(),
                &self.sink,
            )?);
        }

        let mut unit = Vec::with_capacity(annex_b.len() + ACCESS_UNIT_DELIMITER.len());
        unit.extend_from_slice(annex_b);
        unit.extend_from_slice(&ACCESS_UNIT_DELIMITER);
        let Some(process) = self.process.as_ref() else {
            return Err(DecodeError("ffmpeg is not running".to_string()));
        };
        if let Err(error) = process.send(unit) {
            self.process = None;
            return Err(error);
        }
        Ok(())
    }
}
