use crate::async_request::Response;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use muxy_protocol::{ReplyBody, RequestId};

use crate::ClientError;

#[derive(Default)]
struct State {
    next: u32,
    waiting: HashMap<RequestId, (Sender<ReplyBody>, Instant)>,
    asynchronous: HashMap<RequestId, Asynchronous>,
    rtt: Rtt,
    closed: bool,
}

struct Asynchronous {
    response: Arc<Response>,
    sent: Instant,
    deadline: Instant,
}

/// Smoothed round trip time of light request/reply pairs (weight 1/8).
#[derive(Default)]
struct Rtt {
    smoothed: Option<Duration>,
    sampled: Option<Instant>,
}

impl Rtt {
    fn sample(&mut self, sample: Duration, now: Instant) {
        self.sampled = Some(now);
        self.smoothed = Some(match self.smoothed {
            Some(smoothed) => (smoothed * 7 + sample) / 8,
            None => sample,
        });
    }
}

#[derive(Default)]
pub(crate) struct Pending {
    state: Mutex<State>,
    deadlines: Condvar,
}

impl Pending {
    pub(crate) fn register(&self) -> Result<(RequestId, Receiver<ReplyBody>), ClientError> {
        let mut state = self.lock();
        if state.closed {
            return Err(ClientError::Disconnected);
        }
        let id = loop {
            let id = RequestId(state.next);
            state.next = state.next.wrapping_add(1);
            if !state.waiting.contains_key(&id) && !state.asynchronous.contains_key(&id) {
                break id;
            }
        };
        let (sender, receiver) = mpsc::channel();
        state.waiting.insert(id, (sender, Instant::now()));
        Ok((id, receiver))
    }

    pub(crate) fn resolve(&self, id: RequestId, body: ReplyBody) {
        let mut state = self.lock();
        let asynchronous = state.asynchronous.remove(&id);
        let waiting = state.waiting.remove(&id);
        if matches!(body, ReplyBody::Pong) {
            let now = Instant::now();
            let sent = asynchronous
                .as_ref()
                .map(|request| request.sent)
                .or_else(|| waiting.as_ref().map(|(_, sent)| *sent));
            if let Some(sent) = sent {
                state.rtt.sample(now.saturating_duration_since(sent), now);
            }
        }
        drop(state);
        if let Some(request) = asynchronous {
            request.response.resolve(match body {
                ReplyBody::Error(error) => Err(ClientError::Server(error)),
                body => Ok(body),
            });
        } else if let Some((sender, _)) = waiting {
            let _ = sender.send(body);
        }
    }

    pub(crate) fn forget(&self, id: RequestId) {
        let mut state = self.lock();
        state.waiting.remove(&id);
        state.asynchronous.remove(&id);
    }

    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        self.deadlines.notify_one();
        state.waiting.clear();
        let responses = std::mem::take(&mut state.asynchronous);
        drop(state);
        for request in responses.into_values() {
            request.response.resolve(Err(ClientError::Disconnected));
        }
    }

    pub(crate) fn register_async(
        &self,
        timeout: Duration,
    ) -> Result<(RequestId, Arc<Response>), ClientError> {
        let mut state = self.lock();
        if state.closed {
            return Err(ClientError::Disconnected);
        }
        if state.asynchronous.len() >= 128 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "too many pending asynchronous requests",
            )
            .into());
        }
        let id = loop {
            let id = RequestId(state.next);
            state.next = state.next.wrapping_add(1);
            if !state.waiting.contains_key(&id) && !state.asynchronous.contains_key(&id) {
                break id;
            }
        };
        let response = Arc::new(Response::default());
        let sent = Instant::now();
        state.asynchronous.insert(
            id,
            Asynchronous {
                response: response.clone(),
                sent,
                deadline: sent + timeout,
            },
        );
        self.deadlines.notify_one();
        Ok((id, response))
    }

    pub(crate) fn contains_async(&self, id: RequestId) -> bool {
        self.lock().asynchronous.contains_key(&id)
    }

    pub(crate) fn fail_async(&self, id: RequestId, error: ClientError) {
        let response = self.lock().asynchronous.remove(&id);
        if let Some(request) = response {
            request.response.resolve(Err(error));
        }
    }

    pub(crate) fn watch_deadlines(&self) {
        loop {
            let mut state = self.lock();
            while !state.closed {
                let next = state
                    .asynchronous
                    .values()
                    .map(|request| request.deadline)
                    .min();
                if next.is_some_and(|deadline| deadline <= Instant::now()) {
                    break;
                }
                state = if let Some(deadline) = next {
                    self.deadlines
                        .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                } else {
                    self.deadlines
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner)
                };
            }
            if state.closed {
                return;
            }
            drop(state);
            self.expire();
        }
    }

    pub(crate) fn expire(&self) {
        let now = Instant::now();
        let mut state = self.lock();
        let expired: Vec<_> = state
            .asynchronous
            .iter()
            .filter(|(_, request)| request.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        let responses: Vec<_> = expired
            .into_iter()
            .filter_map(|id| state.asynchronous.remove(&id))
            .collect();
        drop(state);
        for request in responses {
            request.response.resolve(Err(ClientError::Timeout));
        }
    }

    /// Smoothed round trip time of recent pings, if one was answered within `max_age`.
    pub(crate) fn rtt(&self, max_age: Duration) -> Option<Duration> {
        let state = self.lock();
        state
            .rtt
            .sampled
            .filter(|sampled| sampled.elapsed() <= max_age)
            .and(state.rtt.smoothed)
    }

    /// When a ping was last answered.
    pub(crate) fn last_rtt_sample(&self) -> Option<Instant> {
        self.lock().rtt.sampled
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
