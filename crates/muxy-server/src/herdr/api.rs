//! Blocking client for herdr's newline-delimited JSON socket API.

use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) enum ApiError {
    Io(io::Error),
    /// herdr answered with an error object.
    Rejected {
        code: String,
        message: String,
    },
    Malformed(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "herdr socket: {error}"),
            Self::Rejected { code, message } => write!(formatter, "herdr {code}: {message}"),
            Self::Malformed(message) => write!(formatter, "herdr sent {message}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<io::Error> for ApiError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct Connection {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Connection {
    fn open(path: &Path) -> io::Result<Self> {
        let writer = UnixStream::connect(path)?;
        writer.set_read_timeout(Some(TIMEOUT))?;
        writer.set_write_timeout(Some(TIMEOUT))?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self { writer, reader })
    }
}

/// herdr answers one request per connection, so each call opens its own.
#[derive(Debug)]
pub(crate) struct Api {
    socket: PathBuf,
    next: AtomicU64,
}

impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Connection")
    }
}

impl Api {
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            next: AtomicU64::new(1),
        }
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// Sends one request and returns its `result`.
    pub(crate) fn call(&self, method: &str, params: &Value) -> Result<Value, ApiError> {
        let id = format!("atur-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let mut line = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))
            .map_err(|error| ApiError::Malformed(error.to_string()))?;
        line.push(b'\n');
        exchange(&mut Connection::open(&self.socket)?, &line, &id)
    }
}

fn exchange(connection: &mut Connection, line: &[u8], id: &str) -> Result<Value, ApiError> {
    connection.writer.write_all(line)?;
    loop {
        let mut reply = String::new();
        if connection.reader.read_line(&mut reply)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        let mut value: Value = serde_json::from_str(&reply)
            .map_err(|error| ApiError::Malformed(format!("invalid JSON: {error}")))?;
        if value.get("id").and_then(Value::as_str) != Some(id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(ApiError::Rejected {
                code: error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
        return value
            .get_mut("result")
            .map(Value::take)
            .ok_or_else(|| ApiError::Malformed("a reply without a result".into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn serve(path: &Path, replies: Vec<&'static str>) -> std::thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(path).expect("bind");
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut writer = stream;
            let mut seen = Vec::new();
            for reply in replies {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read");
                let id = serde_json::from_str::<Value>(&line).expect("json")["id"].clone();
                seen.push(line);
                let reply = reply.replace("ID", id.as_str().unwrap_or_default());
                writer.write_all(reply.as_bytes()).expect("write");
            }
            seen
        })
    }

    fn directory() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("atur-herdr-api-{}", getrandom::u64().unwrap_or(1)));
        std::fs::create_dir_all(&path).expect("directory");
        path
    }

    #[test]
    fn returns_results_and_skips_unrelated_lines() {
        let dir = directory();
        let socket = dir.join("herdr.sock");
        let server = serve(
            &socket,
            vec![
                "{\"id\":\"other\",\"result\":{}}\n{\"id\":\"ID\",\"result\":{\"type\":\"pong\"}}\n",
            ],
        );
        let api = Api::new(socket);
        let result = api.call("ping", &json!({})).expect("ping");
        assert_eq!(result["type"], "pong");
        let seen = server.join().expect("server");
        assert!(seen[0].contains("\"method\":\"ping\""));
    }

    #[test]
    fn maps_error_replies_and_detects_missing_objects() {
        let dir = directory();
        let socket = dir.join("herdr.sock");
        let server = serve(
            &socket,
            vec!["{\"id\":\"ID\",\"error\":{\"code\":\"pane_not_found\",\"message\":\"gone\"}}\n"],
        );
        let api = Api::new(socket);
        let error = api
            .call("pane.get", &json!({"pane_id": "w1:p1"}))
            .expect_err("rejected");
        assert_eq!(error.to_string(), "herdr pane_not_found: gone");
        server.join().expect("server");
    }
}
