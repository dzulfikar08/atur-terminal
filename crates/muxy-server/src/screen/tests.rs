use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::{Capture, Downstream, Endpoint, Hub};
use crate::ServerError;

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A capture program that greets and echoes, listening on a socket of its own.
struct Echo {
    path: PathBuf,
    starts: AtomicUsize,
    stops: AtomicUsize,
    running: Arc<AtomicBool>,
    hang_up: Arc<AtomicBool>,
    fail: bool,
}

impl Echo {
    fn new(fail: bool) -> Arc<Self> {
        let path = std::env::temp_dir().join(format!(
            "atur-screen-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Arc::new(Self {
            path,
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            running: Arc::new(AtomicBool::new(false)),
            hang_up: Arc::new(AtomicBool::new(false)),
            fail,
        })
    }
}

struct Shared(Arc<Echo>);

impl Capture for Shared {
    fn start(&self) -> Result<Endpoint, ServerError> {
        let echo = &self.0;
        if echo.fail {
            return Err(ServerError::new(
                muxy_protocol::ErrorCode::Unsupported,
                "wayvnc is not installed on this computer",
            ));
        }
        if echo.running.swap(true, Ordering::AcqRel) {
            return Ok(Endpoint::Unix(echo.path.clone()));
        }
        echo.starts.fetch_add(1, Ordering::AcqRel);
        let _ = std::fs::remove_file(&echo.path);
        let listener = UnixListener::bind(&echo.path).map_err(ServerError::spawn_failed)?;
        listener
            .set_nonblocking(true)
            .map_err(ServerError::spawn_failed)?;
        let running = Arc::clone(&echo.running);
        let hang_up = Arc::clone(&echo.hang_up);
        thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let hang_up = Arc::clone(&hang_up);
                        thread::spawn(move || {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
                            let _ = stream.write_all(b"RFB 003.008\n");
                            let mut buffer = [0_u8; 256];
                            loop {
                                if hang_up.load(Ordering::Acquire) {
                                    return;
                                }
                                match stream.read(&mut buffer) {
                                    Ok(0) => return,
                                    Ok(count) => {
                                        if stream.write_all(&buffer[..count]).is_err() {
                                            return;
                                        }
                                    }
                                    Err(_) => {}
                                }
                            }
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Ok(Endpoint::Unix(echo.path.clone()))
    }

    fn alive(&self) -> bool {
        self.0.running.load(Ordering::Acquire)
    }

    fn stop(&self) {
        if self.0.running.swap(false, Ordering::AcqRel) {
            self.0.stops.fetch_add(1, Ordering::AcqRel);
            let _ = std::fs::remove_file(&self.0.path);
        }
    }

    fn failed_to_serve(&self) {}
}

#[derive(Default)]
struct Collected {
    bytes: Mutex<Vec<u8>>,
    finished: Mutex<Option<String>>,
    changed: Condvar,
}

impl Collected {
    fn wait_bytes(&self, wanted: &[u8]) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        while !bytes.ends_with(wanted) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            bytes = self
                .changed
                .wait_timeout(bytes, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    fn wait_finished(&self) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(reason) = self
                .finished
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
            {
                return Some(reason);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Downstream for Collected {
    fn push(&self, bytes: Vec<u8>) -> bool {
        self.bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(bytes);
        self.changed.notify_all();
        true
    }

    fn finish(&self, reason: String) {
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
    }
}

fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

fn hub(echo: &Arc<Echo>, grace: Duration) -> Arc<Hub> {
    Arc::new(Hub::new(Box::new(Shared(Arc::clone(echo))), grace))
}

#[test]
fn bytes_travel_both_ways_and_the_capture_stops_after_the_last_viewer() {
    let echo = Echo::new(false);
    let hub = hub(&echo, Duration::from_millis(50));
    let collected = Arc::new(Collected::default());
    let viewer = hub.connect(collected.clone()).expect("viewer connects");
    assert!(collected.wait_bytes(b"RFB 003.008\n"));
    assert!(viewer.write(b"ping".to_vec()));
    assert!(collected.wait_bytes(b"ping"));
    assert_eq!(echo.stops.load(Ordering::Acquire), 0);
    drop(viewer);
    assert!(eventually(|| echo.stops.load(Ordering::Acquire) == 1));
    assert_eq!(echo.starts.load(Ordering::Acquire), 1);
}

#[test]
fn a_viewer_that_returns_within_the_grace_keeps_the_capture_running() {
    let echo = Echo::new(false);
    let hub = hub(&echo, Duration::from_millis(400));
    let first = Arc::new(Collected::default());
    drop(hub.connect(first.clone()).expect("first viewer"));
    thread::sleep(Duration::from_millis(100));
    let second = Arc::new(Collected::default());
    let viewer = hub.connect(second.clone()).expect("second viewer");
    assert!(second.wait_bytes(b"RFB 003.008\n"));
    thread::sleep(Duration::from_millis(600));
    assert_eq!(echo.starts.load(Ordering::Acquire), 1);
    assert_eq!(echo.stops.load(Ordering::Acquire), 0);
    drop(viewer);
    assert!(eventually(|| echo.stops.load(Ordering::Acquire) == 1));
}

#[test]
fn a_capture_that_cannot_start_reports_why() {
    let echo = Echo::new(true);
    let hub = hub(&echo, Duration::from_millis(50));
    let error = hub
        .connect(Arc::new(Collected::default()))
        .expect_err("no capture");
    assert!(error.message().contains("not installed"), "{error}");
}

#[test]
fn a_capture_that_hangs_up_tells_the_client() {
    let echo = Echo::new(false);
    let hub = hub(&echo, Duration::from_millis(50));
    let collected = Arc::new(Collected::default());
    let viewer = hub.connect(collected.clone()).expect("viewer connects");
    assert!(collected.wait_bytes(b"RFB 003.008\n"));
    echo.hang_up.store(true, Ordering::Release);
    let reason = collected.wait_finished().expect("the stream ends");
    assert!(reason.contains("stopped"), "{reason}");
    drop(viewer);
}
