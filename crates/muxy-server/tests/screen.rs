//! A screen stream through the real server executable, with a stand-in for
//! `wayvnc` (a Python script that greets, streams on request and echoes).
#![cfg(target_os = "linux")]

use std::error::Error;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use muxy_protocol::transport::{ByteStream, StreamCancellation, connect};
use muxy_protocol::wire::{Decoder, Encoder, WireError};
use muxy_protocol::{
    CONTROL, ChannelId, ErrorCode, Feature, Message, ReplyBody, RequestBody, RequestId, SUPPORTED,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const TIMEOUT: Duration = Duration::from_secs(15);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Greets with an RFB version line, echoes what it is sent, and sends 4 MiB
/// when it receives `STREAM`. It exits when the server that started it is gone.
const FAKE_WAYVNC: &str = r#"#!/usr/bin/env python3
import os, socket, sys, threading, time

parent = os.getppid()
with open(os.environ["FAKE_WAYVNC_LOG"], "w") as log:
    log.write(" ".join(sys.argv[1:]) + "\n")
    log.write(str(os.getpid()) + "\n")
address = sys.argv[-1]
if not address.startswith("unix:"):
    sys.exit("only unix sockets here")
path = address[len("unix:"):]
server = socket.socket(socket.AF_UNIX)
server.bind(path)
server.listen(4)

def stream(connection):
    block = bytes(range(256)) * 64
    for _ in range(256):
        connection.sendall(block)

def serve(connection):
    connection.sendall(b"RFB 003.008\n")
    while True:
        data = connection.recv(4096)
        if not data:
            return
        if data == b"STREAM":
            threading.Thread(target=stream, args=(connection,), daemon=True).start()
        else:
            connection.sendall(data)

def watch():
    while os.getppid() == parent:
        time.sleep(0.2)
    os._exit(0)

threading.Thread(target=watch, daemon=True).start()
while True:
    connection, _ = server.accept()
    threading.Thread(target=serve, args=(connection,), daemon=True).start()
"#;

struct Fixture {
    directory: PathBuf,
    child: Option<Child>,
    _desktop: UnixListener,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let directory = std::env::temp_dir().join(format!(
            "mx-screen-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory)?;
        fs::create_dir(directory.join("run"))?;
        fs::set_permissions(directory.join("run"), fs::Permissions::from_mode(0o700))?;
        // A Wayland desktop is a socket named wayland-N in the runtime directory.
        let desktop = UnixListener::bind(directory.join("run/wayland-7"))?;
        let program = directory.join("wayvnc");
        fs::write(&program, FAKE_WAYVNC)?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;
        Ok(Self {
            directory,
            child: None,
            _desktop: desktop,
        })
    }

    fn socket(&self) -> PathBuf {
        self.directory.join("server.sock")
    }

    fn log(&self) -> PathBuf {
        self.directory.join("wayvnc.log")
    }

    fn start(&mut self, desktop: bool) -> TestResult {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atur-term-server"));
        command
            .env("ATUR_TERM_DIR", &self.directory)
            .env("ATUR_TERM_REMOTE_BIND", "127.0.0.1")
            .env("ATUR_TERM_WAYVNC", self.directory.join("wayvnc"))
            .env("ATUR_TERM_SCREEN_GPU", "0")
            .env("FAKE_WAYVNC_LOG", self.log())
            .env("SHELL", "/bin/sh")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if desktop {
            command.env("XDG_RUNTIME_DIR", self.directory.join("run"));
        } else {
            command.env("XDG_RUNTIME_DIR", self.directory.join("empty"));
        }
        self.child = Some(command.spawn()?);
        let socket = self.socket();
        wait_until(|| {
            Ok(
                fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket())
                    && UnixStream::connect(&socket).is_ok(),
            )
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn wait_until(mut condition: impl FnMut() -> TestResult<bool>) -> TestResult {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if condition()? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err("timed out".into())
}

struct Client {
    encoder: Encoder<Box<dyn Write + Send>>,
    incoming: Receiver<Result<(ChannelId, Message), WireError>>,
    _cancellation: Box<dyn StreamCancellation>,
    next_request: u32,
    features: Vec<Feature>,
}

impl Client {
    fn new(path: &Path) -> TestResult<Self> {
        let stream: Box<dyn ByteStream> = connect(path)?;
        let cancellation = stream.cancellation()?;
        let (read, write) = stream.split()?;
        let mut decoder = Decoder::new(read);
        let (sender, incoming) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let received = decoder.next();
                let closed = received.is_err();
                if sender.send(received).is_err() || closed {
                    break;
                }
            }
        });
        let mut encoder = Encoder::new(write);
        encoder.send(
            CONTROL,
            &Message::Hello {
                versions: SUPPORTED.to_vec(),
            },
        )?;
        let features = match incoming.recv_timeout(TIMEOUT)?? {
            (CONTROL, Message::HelloReply { features, .. }) => features,
            other => return Err(format!("expected HelloReply, got {other:?}").into()),
        };
        Ok(Self {
            encoder,
            incoming,
            _cancellation: cancellation,
            next_request: 1,
            features,
        })
    }

    fn next(&self) -> TestResult<(ChannelId, Message)> {
        Ok(self.incoming.recv_timeout(TIMEOUT)??)
    }

    /// The reply to a request. Screen data that arrives first is returned with it.
    fn request(&mut self, body: RequestBody) -> TestResult<(ReplyBody, Vec<u8>)> {
        let id = RequestId(self.next_request);
        self.next_request += 1;
        self.encoder.send(CONTROL, &Message::Request { id, body })?;
        let mut early = Vec::new();
        loop {
            match self.next()? {
                (CONTROL, Message::Reply { id: received, body }) if received == id => {
                    return Ok((body, early));
                }
                (_, Message::ScreenData(bytes)) => early.extend(bytes),
                (_, Message::Changed { .. }) => {}
                other => return Err(format!("unexpected message: {other:?}").into()),
            }
        }
    }

    fn open_screen(&mut self) -> TestResult<(ChannelId, Vec<u8>)> {
        match self.request(RequestBody::OpenScreen)? {
            (ReplyBody::ScreenOpened(channel), early) => Ok((channel, early)),
            (other, _) => Err(format!("expected a screen, got {other:?}").into()),
        }
    }

    fn ack(&mut self, channel: ChannelId, total: u64) -> TestResult {
        self.encoder.send(
            CONTROL,
            &Message::FrameAck {
                channel,
                seq: total,
            },
        )?;
        Ok(())
    }

    /// Collects screen data until `done` says there is enough, or the stream is quiet.
    fn collect(
        &mut self,
        channel: ChannelId,
        mut data: Vec<u8>,
        quiet: Duration,
        mut done: impl FnMut(&[u8]) -> bool,
    ) -> TestResult<Vec<u8>> {
        while !done(&data) {
            match self.incoming.recv_timeout(quiet) {
                Ok(Ok((received, Message::ScreenData(bytes)))) if received == channel => {
                    data.extend(bytes);
                }
                Ok(Ok((_, Message::Changed { .. }))) => {}
                Ok(Ok((_, Message::ScreenClosed(reason)))) => {
                    return Err(format!("the stream closed: {reason}").into());
                }
                Ok(Ok(other)) => return Err(format!("unexpected message: {other:?}").into()),
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => break,
            }
        }
        Ok(data)
    }

    fn send(&mut self, channel: ChannelId, bytes: &[u8]) -> TestResult {
        self.encoder
            .send(channel, &Message::ScreenData(bytes.to_vec()))?;
        Ok(())
    }
}

#[test]
fn screen_bytes_travel_both_ways_and_stop_with_the_server() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.start(true)?;
    let mut client = Client::new(&fixture.socket())?;
    assert!(client.features.contains(&Feature::SCREEN));
    let (channel, early) = client.open_screen()?;
    let greeting = b"RFB 003.008\n";
    let mut data = client.collect(channel, early, Duration::from_secs(5), |data| {
        data.len() >= greeting.len()
    })?;
    assert_eq!(data, greeting);
    let log = fs::read_to_string(fixture.log())?;
    let arguments = log.lines().next().unwrap_or_default().to_owned();
    assert!(arguments.contains("-r"), "{arguments}");
    assert!(arguments.contains(" unix:"), "{arguments}");
    assert!(!arguments.contains("0.0.0.0"), "{arguments}");
    let wayvnc: i32 = log.lines().nth(1).unwrap_or_default().parse()?;
    // What the client sends reaches the capture, which echoes it.
    client.send(channel, b"hello screen")?;
    data = client.collect(channel, Vec::new(), Duration::from_secs(5), |data| {
        data.len() >= b"hello screen".len()
    })?;
    assert_eq!(data, b"hello screen");
    client.ack(channel, (greeting.len() + data.len()) as u64)?;
    // The capture stops when the server does.
    let mut server = fixture.child.take().ok_or("server is not running")?;
    let killed = Command::new("kill")
        .args(["-TERM", &server.id().to_string()])
        .status()?;
    assert!(killed.success());
    server.wait()?;
    wait_until(|| Ok(!Path::new(&format!("/proc/{wayvnc}")).exists()))?;
    Ok(())
}

#[test]
fn the_server_sends_no_more_than_its_window_until_the_client_reports_handling() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.start(true)?;
    let mut client = Client::new(&fixture.socket())?;
    let (channel, early) = client.open_screen()?;
    let mut total = client
        .collect(channel, early, Duration::from_secs(5), |data| {
            data.len() >= 12
        })?
        .len();
    client.send(channel, b"STREAM")?;
    // Without acknowledgements the stream stops at the window (512 KiB).
    let before = client
        .collect(channel, Vec::new(), Duration::from_millis(800), |_| false)?
        .len();
    assert!(before >= 256 * 1024, "only {before} bytes arrived");
    assert!(
        before + total <= 512 * 1024 + 32 * 1024,
        "{} bytes arrived unacknowledged",
        before + total
    );
    total += before;
    client.ack(channel, total as u64)?;
    let after = client
        .collect(channel, Vec::new(), Duration::from_secs(5), |data| {
            data.len() >= 256 * 1024
        })?
        .len();
    assert!(
        after >= 256 * 1024,
        "only {after} bytes after acknowledging"
    );
    Ok(())
}

#[test]
fn closing_a_screen_ends_it_and_a_second_viewer_shares_the_capture() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.start(true)?;
    let mut client = Client::new(&fixture.socket())?;
    let (first, early) = client.open_screen()?;
    client.collect(first, early, Duration::from_secs(5), |data| {
        data.len() >= 12
    })?;
    let (second, early) = client.open_screen()?;
    assert_ne!(first, second);
    client.collect(second, early, Duration::from_secs(5), |data| {
        data.len() >= 12
    })?;
    let (reply, _) = client.request(RequestBody::Detach(first))?;
    assert!(matches!(reply, ReplyBody::Detached), "{reply:?}");
    // One capture serves both viewers: the stand-in logs once, when it starts.
    assert_eq!(fs::read_to_string(fixture.log())?.lines().count(), 2);
    client.send(second, b"still here")?;
    let data = client.collect(second, Vec::new(), Duration::from_secs(5), |data| {
        data.len() >= b"still here".len()
    })?;
    assert_eq!(data, b"still here");
    Ok(())
}

#[test]
fn a_computer_without_a_desktop_explains_why() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.start(false)?;
    let mut client = Client::new(&fixture.socket())?;
    let (reply, _) = client.request(RequestBody::OpenScreen)?;
    let ReplyBody::Error(error) = reply else {
        return Err(format!("expected an error, got {reply:?}").into());
    };
    assert_eq!(error.code, ErrorCode::Unsupported);
    assert!(error.message.contains("Wayland"), "{}", error.message);
    Ok(())
}
