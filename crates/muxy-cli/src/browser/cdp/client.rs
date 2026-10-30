//! One connection to a browser target: commands out, replies and events in.
//! Commands are JSON messages with an `id`; the reply carries the same one.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::ws::Socket;

/// How long the browser has to answer one command.
const COMMAND_TIME: Duration = Duration::from_secs(30);

/// A message the browser sent without being asked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Event {
    pub(super) method: String,
    pub(super) params: Value,
}

/// What the verbs need from a connection, so they run against a stand-in in
/// tests.
pub(super) trait Transport {
    /// Runs a command and returns its result.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
    /// The next event, waiting up to `wait` for one.
    fn event(&mut self, wait: Duration) -> Result<Option<Event>, String>;
    /// Pauses between polls.
    fn pause(&mut self, time: Duration);
}

#[derive(Debug)]
pub(super) struct Connection {
    socket: Socket,
    next_id: u64,
    events: VecDeque<Event>,
}

impl Connection {
    pub(super) fn open(port: u16, path: &str) -> Result<Self, String> {
        let socket = Socket::connect(port, path)
            .map_err(|error| format!("could not connect to the browser: {error}"))?;
        Ok(Self {
            socket,
            next_id: 0,
            events: VecDeque::new(),
        })
    }

    fn read(&mut self, wait: Duration) -> Result<Option<Value>, String> {
        let text = self
            .socket
            .receive(wait)
            .map_err(|error| format!("lost the browser connection: {error}"))?;
        text.map(|text| {
            serde_json::from_str(&text).map_err(|_| "the browser sent invalid JSON".to_owned())
        })
        .transpose()
    }
}

/// An event message as an [`Event`], or none for a reply.
fn as_event(message: &Value) -> Option<Event> {
    let method = message.get("method")?.as_str()?;
    Some(Event {
        method: method.to_owned(),
        params: message.get("params").cloned().unwrap_or(Value::Null),
    })
}

/// A reply's result, or its error text.
pub(super) fn reply_result(mut message: Value) -> Result<Value, String> {
    if let Some(error) = message.get("error") {
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the browser reported an error");
        return Err(text.to_owned());
    }
    Ok(message.get_mut("result").map_or(Value::Null, Value::take))
}

impl Transport for Connection {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({ "id": id, "method": method, "params": params });
        self.socket
            .send_text(&message.to_string())
            .map_err(|error| format!("lost the browser connection: {error}"))?;
        let deadline = Instant::now() + COMMAND_TIME;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(message) = self.read(remaining)? else {
                return Err(format!("the browser did not answer {method} in time"));
            };
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return reply_result(message);
            }
            if let Some(event) = as_event(&message) {
                self.events.push_back(event);
            }
        }
    }

    fn event(&mut self, wait: Duration) -> Result<Option<Event>, String> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        let deadline = Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.read(remaining)? {
                Some(message) => {
                    if let Some(event) = as_event(&message) {
                        return Ok(Some(event));
                    }
                }
                None => return Ok(None),
            }
        }
    }

    fn pause(&mut self, time: Duration) {
        std::thread::sleep(time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_and_results_are_told_apart() {
        assert_eq!(
            reply_result(json!({"id": 1, "result": {"a": 1}})),
            Ok(json!({"a": 1}))
        );
        assert_eq!(reply_result(json!({"id": 1, "result": {}})), Ok(json!({})));
        assert_eq!(
            reply_result(json!({"id": 1, "error": {"code": -32000, "message": "No target"}})),
            Err("No target".to_owned())
        );
    }

    #[test]
    fn only_messages_with_a_method_are_events() {
        assert_eq!(
            as_event(&json!({"method": "Page.loadEventFired", "params": {"timestamp": 1}})),
            Some(Event {
                method: "Page.loadEventFired".into(),
                params: json!({"timestamp": 1})
            })
        );
        assert_eq!(as_event(&json!({"id": 1, "result": {}})), None);
    }
}
