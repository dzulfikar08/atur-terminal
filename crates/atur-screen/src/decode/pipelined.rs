//! A decoder moved to a thread of its own.

use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use super::{DecodeError, H264Decoder};

/// Access units that may wait for a [`Pipelined`] decoder before the reader of the
/// connection has to wait too.
const PIPELINE_DEPTH: usize = 3;

/// One access unit on its way to the decoder's own thread.
struct Unit {
    annex_b: Vec<u8>,
    reset: bool,
}

/// Runs a decoder that blocks (`VideoToolbox` waits for each picture) on a thread of its
/// own, so decoding never holds up the protocol: the server's fence pings, which it times
/// to estimate the link's bandwidth, and its next update are answered while the picture
/// is still being decoded.
///
/// A failure shows on the next [`decode`](H264Decoder::decode) call, one access unit
/// late, which is when the session asks the server for a key frame.
pub(crate) struct Pipelined {
    name: Arc<Mutex<String>>,
    failure: Arc<Mutex<Option<DecodeError>>>,
    units: Option<SyncSender<Unit>>,
    worker: Option<JoinHandle<()>>,
}

impl Pipelined {
    /// `None` when the thread cannot start; the caller then keeps `inner` out of use.
    pub(crate) fn start(mut inner: Box<dyn H264Decoder>) -> Option<Self> {
        let name = Arc::new(Mutex::new(inner.name()));
        let failure = Arc::new(Mutex::new(None));
        let (units, receiver) = mpsc::sync_channel::<Unit>(PIPELINE_DEPTH);
        let (worker_name, worker_failure) = (Arc::clone(&name), Arc::clone(&failure));
        let worker = thread::Builder::new()
            .name("screen-decoder".to_owned())
            .spawn(move || {
                while let Ok(unit) = receiver.recv() {
                    let result = inner.decode(&unit.annex_b, unit.reset);
                    // A decoder that fell back to software says so.
                    *worker_name.lock().unwrap_or_else(PoisonError::into_inner) = inner.name();
                    if let Err(error) = result {
                        *worker_failure
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(error);
                    }
                }
            })
            .ok()?;
        Some(Self {
            name,
            failure,
            units: Some(units),
            worker: Some(worker),
        })
    }
}

impl H264Decoder for Pipelined {
    fn name(&self) -> String {
        self.name
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn decode(&mut self, annex_b: &[u8], reset: bool) -> Result<(), DecodeError> {
        if let Some(error) = self
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Err(error);
        }
        let units = self
            .units
            .as_ref()
            .ok_or_else(|| DecodeError("the decoder thread has ended".to_owned()))?;
        units
            .send(Unit {
                annex_b: annex_b.to_vec(),
                reset,
            })
            .map_err(|_| DecodeError("the decoder thread has ended".to_owned()))
    }
}

impl Drop for Pipelined {
    fn drop(&mut self) {
        // The worker finishes what is queued, then sees the channel close.
        self.units = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    use super::*;

    type Seen = Arc<Mutex<Vec<(Vec<u8>, bool)>>>;

    /// The depth of the queue, as a byte value for the units the tests send.
    const UNITS: u8 = 3;

    /// Blocks in `decode` until released, records what it was given.
    struct Slow {
        seen: Seen,
        gate: mpsc::Receiver<()>,
        fail_on: Option<u8>,
    }

    impl H264Decoder for Slow {
        fn name(&self) -> String {
            "slow".to_owned()
        }

        fn decode(&mut self, annex_b: &[u8], reset: bool) -> Result<(), DecodeError> {
            let _ = self.gate.recv_timeout(Duration::from_secs(5));
            self.seen.lock().unwrap().push((annex_b.to_vec(), reset));
            if self.fail_on == annex_b.first().copied() {
                return Err(DecodeError("broken".to_owned()));
            }
            Ok(())
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn decoding_does_not_hold_the_caller_up_and_keeps_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (release, gate) = channel();
        let mut decoder = Pipelined::start(Box::new(Slow {
            seen: Arc::clone(&seen),
            gate,
            fail_on: None,
        }))
        .unwrap();
        assert_eq!(decoder.name(), "slow");
        // The decoder is blocked, yet the caller is not, for the depth of the queue.
        let started = Instant::now();
        assert_eq!(usize::from(UNITS), PIPELINE_DEPTH);
        for unit in 0..UNITS {
            decoder.decode(&[unit], unit == 0).unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        for _ in 0..PIPELINE_DEPTH {
            release.send(()).unwrap();
        }
        wait_until(|| seen.lock().unwrap().len() == PIPELINE_DEPTH);
        let expected: Vec<(Vec<u8>, bool)> =
            (0..UNITS).map(|unit| (vec![unit], unit == 0)).collect();
        assert_eq!(*seen.lock().unwrap(), expected);
    }

    #[test]
    fn a_failure_is_reported_by_the_next_call() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (release, gate) = channel();
        let mut decoder = Pipelined::start(Box::new(Slow {
            seen: Arc::clone(&seen),
            gate,
            fail_on: Some(7),
        }))
        .unwrap();
        release.send(()).unwrap();
        decoder.decode(&[7], false).unwrap();
        wait_until(|| seen.lock().unwrap().len() == 1);
        // The worker stores the error after recording the unit.
        wait_until(|| decoder.failure.lock().unwrap().is_some());
        let error = decoder.decode(&[8], false).unwrap_err();
        assert_eq!(error.to_string(), "broken");
        release.send(()).unwrap();
        decoder.decode(&[9], false).unwrap();
        wait_until(|| seen.lock().unwrap().len() == 2);
    }
}
