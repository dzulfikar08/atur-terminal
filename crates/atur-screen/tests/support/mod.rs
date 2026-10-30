//! A fake RFB server for the integration tests: a transport over channels, byte stream
//! builders for every message the client understands and a parser for the client's input.

#![allow(
    dead_code,
    unreachable_pub,
    missing_debug_implementations,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use atur_screen::decode::{FrameSink, H264Decoder};
use atur_screen::{Frame, Pixels, Screen, ScreenOptions, ScreenState, ScreenTransport};
use flate2::write::ZlibEncoder;
use flate2::{Compress, Compression, FlushCompress};

const TIMEOUT: Duration = Duration::from_secs(5);

struct Sent {
    bytes: Mutex<Vec<u8>>,
    changed: Condvar,
}

struct FakeTransport {
    incoming: Mutex<Receiver<Option<Vec<u8>>>>,
    to_self: Sender<Option<Vec<u8>>>,
    sent: Arc<Sent>,
    processed: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

impl ScreenTransport for FakeTransport {
    fn recv(&self) -> Option<Vec<u8>> {
        self.incoming.lock().unwrap().recv().ok().flatten()
    }

    fn send(&self, bytes: &[u8]) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("closed".to_owned());
        }
        self.sent.bytes.lock().unwrap().extend_from_slice(bytes);
        self.sent.changed.notify_all();
        Ok(())
    }

    fn processed(&self, bytes: usize) {
        self.processed.fetch_add(bytes, Ordering::SeqCst);
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.to_self.send(None);
    }
}

/// How the server splits its byte stream into transport chunks.
#[derive(Clone, Copy, Debug)]
pub enum Chunking {
    Whole,
    Each(usize),
    /// 1 byte, 7 bytes, a few hundred bytes or 4 KiB at random.
    Random(u64),
}

pub struct Server {
    to_client: Sender<Option<Vec<u8>>>,
    sent: Arc<Sent>,
    pub processed: Arc<AtomicUsize>,
    pub closed: Arc<AtomicBool>,
    pub pushed: AtomicUsize,
}

impl Server {
    pub fn push(&self, bytes: &[u8], chunking: Chunking) {
        let mut state = match chunking {
            Chunking::Random(seed) => seed,
            _ => 0,
        };
        let mut position = 0;
        while position < bytes.len() {
            let size = match chunking {
                Chunking::Whole => bytes.len(),
                Chunking::Each(size) => size,
                Chunking::Random(_) => {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    match (state >> 33) % 4 {
                        0 => 1,
                        1 => 7,
                        2 => 1 + usize::try_from((state >> 40) % 500).unwrap(),
                        _ => 4096,
                    }
                }
            };
            let end = (position + size.max(1)).min(bytes.len());
            self.pushed.fetch_add(end - position, Ordering::SeqCst);
            let _ = self.to_client.send(Some(bytes[position..end].to_vec()));
            position = end;
        }
    }

    /// Ends the stream the way a dropped connection does.
    pub fn hang_up(&self) {
        let _ = self.to_client.send(None);
    }

    /// Waits for `count` bytes from the client and removes them from the buffer.
    pub fn read(&self, count: usize) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        let mut guard = self.sent.bytes.lock().unwrap();
        while guard.len() < count {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "timed out waiting for {count} bytes from the client, have {:?}",
                *guard
            );
            guard = self.sent.changed.wait_timeout(guard, left).unwrap().0;
        }
        guard.drain(..count).collect()
    }

    /// Everything the client sent, once it has been quiet for a while.
    pub fn settle(&self) -> Vec<u8> {
        let mut seen = usize::MAX;
        loop {
            thread::sleep(Duration::from_millis(60));
            let mut guard = self.sent.bytes.lock().unwrap();
            if guard.len() == seen {
                return std::mem::take(&mut *guard);
            }
            seen = guard.len();
        }
    }
}

pub struct Harness {
    pub screen: Screen,
    pub server: Server,
    pub wakes: Arc<AtomicUsize>,
}

pub fn start(options: ScreenOptions) -> Harness {
    start_with(options, |_| None)
}

pub fn start_with(
    options: ScreenOptions,
    make_decoder: impl FnOnce(FrameSink) -> Option<Box<dyn H264Decoder>>,
) -> Harness {
    let (to_client, incoming) = mpsc::channel();
    let sent = Arc::new(Sent {
        bytes: Mutex::new(Vec::new()),
        changed: Condvar::new(),
    });
    let processed = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let transport = FakeTransport {
        incoming: Mutex::new(incoming),
        to_self: to_client.clone(),
        sent: Arc::clone(&sent),
        processed: Arc::clone(&processed),
        closed: Arc::clone(&closed),
    };
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&wakes);
    let screen = Screen::start_with_decoder(
        transport,
        options,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
        },
        make_decoder,
    );
    Harness {
        screen,
        server: Server {
            to_client,
            sent,
            processed,
            closed,
            pushed: AtomicUsize::new(0),
        },
        wakes,
    }
}

pub fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

pub fn wait_ready(screen: &Screen) {
    wait_for("the screen to become ready", || {
        matches!(screen.state(), ScreenState::Ready(_))
    });
}

pub fn wait_closed(screen: &Screen) -> String {
    wait_for("the screen to close", || {
        matches!(screen.state(), ScreenState::Closed(_))
    });
    match screen.state() {
        ScreenState::Closed(reason) => reason,
        other => panic!("unexpected state {other:?}"),
    }
}

/// The newest frame once `condition` holds for it.
pub fn wait_frame(screen: &Screen, mut condition: impl FnMut(&Frame) -> bool) -> Frame {
    let mut found = None;
    wait_for("a matching frame", || {
        if let Some(frame) = screen.frame()
            && condition(&frame)
        {
            found = Some(frame);
            return true;
        }
        false
    });
    found.unwrap()
}

#[allow(
    unreachable_patterns,
    clippy::match_wildcard_for_single_variants,
    reason = "macOS has a second pixel variant, other platforms none"
)]
pub fn bgra(frame: &Frame) -> Vec<u8> {
    match &frame.pixels {
        Pixels::Bgra(bytes) => bytes.to_vec(),
        _ => panic!("frame is not BGRA"),
    }
}

pub fn pixel_at(frame: &Frame, x: usize, y: usize) -> [u8; 4] {
    let bytes = bgra(frame);
    let at = (y * usize::try_from(frame.width).unwrap() + x) * 4;
    [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]
}

pub fn be16(value: usize) -> [u8; 2] {
    u16::try_from(value).unwrap().to_be_bytes()
}

pub fn be32(value: usize) -> [u8; 4] {
    u32::try_from(value).unwrap().to_be_bytes()
}

/// Version, security (None, 3.8 style), result, `ServerInit`.
pub fn hello(width: usize, height: usize) -> Vec<u8> {
    let mut bytes = b"RFB 003.008\n".to_vec();
    bytes.extend_from_slice(&[1, 1]);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes.extend_from_slice(&be16(width));
    bytes.extend_from_slice(&be16(height));
    // 32 bpp, depth 24, little endian, true colour, 255/255/255, shifts 16/8/0.
    bytes.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
    bytes.extend_from_slice(&be32(7));
    bytes.extend_from_slice(b"fake-vm");
    bytes
}

/// What the client sends for the handshake: version, security type, `ClientInit`,
/// `SetPixelFormat`, `SetEncodings` and the first full update request.
pub fn handshake_reply(width: usize, height: usize, h264: bool) -> Vec<u8> {
    let mut bytes = b"RFB 003.008\n".to_vec();
    bytes.extend_from_slice(&[1, 1]);
    bytes.extend_from_slice(&[
        0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0,
    ]);
    let mut encodings: Vec<i32> = Vec::new();
    if h264 {
        encodings.push(50);
    }
    encodings.extend([
        16,
        1,
        0,
        -223,
        -308,
        -312,
        -313,
        i32::from_be_bytes([0xc0, 0xa1, 0xe5, 0xce]),
        -224,
    ]);
    bytes.extend_from_slice(&[2, 0]);
    bytes.extend_from_slice(&be16(encodings.len()));
    for encoding in encodings {
        bytes.extend_from_slice(&encoding.to_be_bytes());
    }
    bytes.extend_from_slice(&update_request(false, width, height));
    bytes
}

pub fn update_request(incremental: bool, width: usize, height: usize) -> Vec<u8> {
    let mut bytes = vec![3, u8::from(incremental), 0, 0, 0, 0];
    bytes.extend_from_slice(&be16(width));
    bytes.extend_from_slice(&be16(height));
    bytes
}

pub fn enable_continuous(width: usize, height: usize) -> Vec<u8> {
    let mut bytes = vec![150, 1, 0, 0, 0, 0];
    bytes.extend_from_slice(&be16(width));
    bytes.extend_from_slice(&be16(height));
    bytes
}

/// Starts a screen of the given size, completes the handshake and checks the client's reply.
pub fn start_ready(
    options: ScreenOptions,
    width: usize,
    height: usize,
    chunking: Chunking,
) -> Harness {
    let harness = start(options);
    connect(&harness, width, height, chunking, false);
    harness
}

pub fn connect(harness: &Harness, width: usize, height: usize, chunking: Chunking, h264: bool) {
    harness.server.push(&hello(width, height), chunking);
    wait_ready(&harness.screen);
    let expected = handshake_reply(width, height, h264);
    assert_eq!(harness.server.read(expected.len()), expected);
}

pub fn update(rects: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&be16(rects.len()));
    for rect in rects {
        bytes.extend_from_slice(rect);
    }
    bytes
}

pub fn rect_header(x: usize, y: usize, w: usize, h: usize, encoding: i32) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [x, y, w, h] {
        bytes.extend_from_slice(&be16(value));
    }
    bytes.extend_from_slice(&encoding.to_be_bytes());
    bytes
}

/// Raw rectangle from `[b, g, r, x]` pixels in reading order.
pub fn raw_rect(x: usize, y: usize, w: usize, h: usize, pixels: &[[u8; 4]]) -> Vec<u8> {
    assert_eq!(pixels.len(), w * h);
    let mut bytes = rect_header(x, y, w, h, 0);
    for pixel in pixels {
        bytes.extend_from_slice(pixel);
    }
    bytes
}

pub fn copy_rect(
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    source_x: usize,
    source_y: usize,
) -> Vec<u8> {
    let mut bytes = rect_header(x, y, w, h, 1);
    bytes.extend_from_slice(&be16(source_x));
    bytes.extend_from_slice(&be16(source_y));
    bytes
}

pub fn zrle_rect(x: usize, y: usize, w: usize, h: usize, compressed: &[u8]) -> Vec<u8> {
    let mut bytes = rect_header(x, y, w, h, 16);
    bytes.extend_from_slice(&be32(compressed.len()));
    bytes.extend_from_slice(compressed);
    bytes
}

/// The server's single zlib stream for ZRLE, flushed after every rectangle.
pub struct ZrleStream(Compress);

impl ZrleStream {
    pub fn new() -> Self {
        Self(Compress::new(Compression::fast(), true))
    }

    pub fn pack(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + 512);
        self.0
            .compress_vec(data, &mut out, FlushCompress::Sync)
            .unwrap();
        out
    }
}

/// One ZRLE tile: its encoded bytes and the `[b, g, r]` pixels it paints.
pub struct Tile {
    pub bytes: Vec<u8>,
    pub pixels: Vec<[u8; 3]>,
}

pub fn solid(w: usize, h: usize, color: [u8; 3]) -> Tile {
    let mut bytes = vec![1];
    bytes.extend_from_slice(&color);
    Tile {
        bytes,
        pixels: vec![color; w * h],
    }
}

pub fn raw_tile(w: usize, h: usize, color: impl Fn(usize, usize) -> [u8; 3]) -> Tile {
    let mut bytes = vec![0];
    let mut pixels = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let value = color(x, y);
            bytes.extend_from_slice(&value);
            pixels.push(value);
        }
    }
    Tile { bytes, pixels }
}

/// Palette tile with packed indexes (2 colours: 1 bit, 3 or 4: 2 bits, up to 16: 4 bits).
pub fn packed(
    w: usize,
    h: usize,
    palette: &[[u8; 3]],
    index: impl Fn(usize, usize) -> usize,
) -> Tile {
    let count = palette.len();
    let bits = if count == 2 {
        1
    } else if count <= 4 {
        2
    } else {
        4
    };
    let mut bytes = vec![u8::try_from(count).unwrap()];
    for color in palette {
        bytes.extend_from_slice(color);
    }
    let mut pixels = Vec::new();
    for y in 0..h {
        let mut row = vec![0u8; (w * bits).div_ceil(8)];
        for x in 0..w {
            let choice = index(x, y);
            let bit = x * bits;
            row[bit / 8] |= u8::try_from(choice).unwrap() << (8 - bits - bit % 8);
            pixels.push(palette[choice]);
        }
        bytes.extend_from_slice(&row);
    }
    Tile { bytes, pixels }
}

fn run_length(length: usize) -> Vec<u8> {
    let mut rest = length - 1;
    let mut bytes = Vec::new();
    while rest >= 255 {
        bytes.push(255);
        rest -= 255;
    }
    bytes.push(u8::try_from(rest).unwrap());
    bytes
}

pub fn plain_rle(runs: &[([u8; 3], usize)]) -> Tile {
    let mut bytes = vec![128];
    let mut pixels = Vec::new();
    for &(color, length) in runs {
        bytes.extend_from_slice(&color);
        bytes.extend_from_slice(&run_length(length));
        pixels.extend(std::iter::repeat_n(color, length));
    }
    Tile { bytes, pixels }
}

pub fn palette_rle(palette: &[[u8; 3]], runs: &[(usize, usize)]) -> Tile {
    let mut bytes = vec![128 + u8::try_from(palette.len()).unwrap()];
    for color in palette {
        bytes.extend_from_slice(color);
    }
    let mut pixels = Vec::new();
    for &(index, length) in runs {
        let index_byte = u8::try_from(index).unwrap();
        if length == 1 {
            bytes.push(index_byte);
        } else {
            bytes.push(index_byte | 0x80);
            bytes.extend_from_slice(&run_length(length));
        }
        pixels.extend(std::iter::repeat_n(palette[index], length));
    }
    Tile { bytes, pixels }
}

/// Writes a tile into an expected BGRA framebuffer of width `stride_pixels`.
pub fn paint(
    expected: &mut [u8],
    stride_pixels: usize,
    x0: usize,
    y0: usize,
    w: usize,
    tile: &Tile,
) {
    for (i, color) in tile.pixels.iter().enumerate() {
        let at = ((y0 + i / w) * stride_pixels + x0 + i % w) * 4;
        expected[at..at + 3].copy_from_slice(color);
        expected[at + 3] = 255;
    }
}

/// What the client sends for input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Input {
    Key { down: bool, keysym: u32 },
    Pointer { mask: u8, x: u16, y: u16 },
}

pub fn parse_input(mut bytes: &[u8]) -> Vec<Input> {
    let mut events = Vec::new();
    while !bytes.is_empty() {
        match bytes[0] {
            4 => {
                events.push(Input::Key {
                    down: bytes[1] == 1,
                    keysym: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
                });
                bytes = &bytes[8..];
            }
            5 => {
                events.push(Input::Pointer {
                    mask: bytes[1],
                    x: u16::from_be_bytes([bytes[2], bytes[3]]),
                    y: u16::from_be_bytes([bytes[4], bytes[5]]),
                });
                bytes = &bytes[6..];
            }
            other => panic!("unexpected client message {other}: {bytes:?}"),
        }
    }
    events
}

pub fn pointer_bytes(mask: u8, x: u16, y: u16) -> Vec<u8> {
    let mut bytes = vec![5, mask];
    bytes.extend_from_slice(&x.to_be_bytes());
    bytes.extend_from_slice(&y.to_be_bytes());
    bytes
}

/// Server-to-client legacy `ServerCutText`.
pub fn cut_text(text: &[u8]) -> Vec<u8> {
    let mut bytes = vec![3, 0, 0, 0];
    bytes.extend_from_slice(&be32(text.len()));
    bytes.extend_from_slice(text);
    bytes
}

/// Server-to-client extended clipboard message with the given body.
pub fn ext_clipboard(body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![3, 0, 0, 0];
    bytes.extend_from_slice(&(-i32::try_from(body.len()).unwrap()).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

pub fn ext_provide_text(text: &str) -> Vec<u8> {
    let mut data = text.as_bytes().to_vec();
    data.push(0);
    let mut payload = be32(data.len()).to_vec();
    payload.extend_from_slice(&data);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&payload).unwrap();
    let mut body = 0x1000_0001u32.to_be_bytes().to_vec();
    body.extend_from_slice(&encoder.finish().unwrap());
    ext_clipboard(&body)
}
