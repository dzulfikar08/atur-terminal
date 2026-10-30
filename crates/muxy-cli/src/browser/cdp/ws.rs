//! A minimal client for the `WebSocket` protocol (RFC 6455), enough to talk
//! to a browser's debugging endpoint on this computer: text messages, ping
//! replies, and fragmented messages. It only connects to loopback addresses.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

/// The largest message accepted; a full-page screenshot is a few megabytes.
const MAX_MESSAGE: usize = 256 * 1024 * 1024;
const CONNECT_TIME: Duration = Duration::from_secs(5);

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// One frame as it arrived.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Frame {
    pub(super) fin: bool,
    pub(super) opcode: u8,
    pub(super) payload: Vec<u8>,
}

/// The loopback address of `port`; nothing else is ever connected to.
pub(super) fn loopback(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

/// A frame from the start of `buffer`, with how many bytes it took, or
/// `None` while the buffer holds only part of one.
pub(super) fn parse_frame(buffer: &[u8]) -> io::Result<Option<(Frame, usize)>> {
    let [first, second, ..] = buffer else {
        return Ok(None);
    };
    let masked = second & 0x80 != 0;
    let mut offset = 2;
    let length = match second & 0x7F {
        126 => {
            let Some(bytes) = buffer.get(offset..offset + 2) else {
                return Ok(None);
            };
            offset += 2;
            usize::from(u16::from_be_bytes([bytes[0], bytes[1]]))
        }
        127 => {
            let Some(bytes) = buffer.get(offset..offset + 8) else {
                return Ok(None);
            };
            offset += 8;
            let mut array = [0_u8; 8];
            array.copy_from_slice(bytes);
            usize::try_from(u64::from_be_bytes(array))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame is too large"))?
        }
        short => usize::from(short),
    };
    if length > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame is too large",
        ));
    }
    let mask = if masked {
        let Some(bytes) = buffer.get(offset..offset + 4) else {
            return Ok(None);
        };
        offset += 4;
        Some([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        None
    };
    let Some(payload) = buffer.get(offset..offset + length) else {
        return Ok(None);
    };
    let mut payload = payload.to_vec();
    if let Some(mask) = mask {
        apply_mask(&mut payload, mask);
    }
    Ok(Some((
        Frame {
            fin: first & 0x80 != 0,
            opcode: first & 0x0F,
            payload,
        },
        offset + length,
    )))
}

fn apply_mask(payload: &mut [u8], mask: [u8; 4]) {
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
}

/// A client frame: always final and masked, as the protocol requires.
pub(super) fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    match payload.len() {
        length if length < 126 => frame.push(0x80 | u8::try_from(length).unwrap_or(0)),
        length if length <= usize::from(u16::MAX) => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&u16::try_from(length).unwrap_or(0).to_be_bytes());
        }
        length => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&u64::try_from(length).unwrap_or(0).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    let start = frame.len();
    frame.extend_from_slice(payload);
    apply_mask(&mut frame[start..], mask);
    frame
}

/// An open connection.
#[derive(Debug)]
pub(super) struct Socket {
    stream: TcpStream,
    /// Bytes read and not yet parsed into frames.
    buffer: Vec<u8>,
    /// The parts of a fragmented message received so far.
    message: Vec<u8>,
    seed: u64,
}

impl Socket {
    /// Connects to `path` on `port` at 127.0.0.1 and upgrades to a
    /// `WebSocket`.
    pub(super) fn connect(port: u16, path: &str) -> io::Result<Self> {
        let mut stream = TcpStream::connect_timeout(&loopback(port), CONNECT_TIME)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(CONNECT_TIME))?;
        stream.set_write_timeout(Some(CONNECT_TIME))?;
        let mut seed = seed();
        let key = crate::clipboard::base64(&random_bytes(&mut seed));
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )?;
        let mut head = Vec::new();
        let mut chunk = [0_u8; 4096];
        let end = loop {
            if let Some(end) = find_header_end(&head) {
                break end;
            }
            if head.len() > 64 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the browser's reply is too long",
                ));
            }
            let count = stream.read(&mut chunk)?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the browser closed the connection",
                ));
            }
            head.extend_from_slice(&chunk[..count]);
        };
        let status = String::from_utf8_lossy(&head[..end]);
        let status = status.lines().next().unwrap_or_default();
        if status.split_whitespace().nth(1) != Some("101") {
            return Err(io::Error::other(format!(
                "the browser refused the DevTools connection ({status})"
            )));
        }
        Ok(Self {
            stream,
            buffer: head.split_off(end),
            message: Vec::new(),
            seed,
        })
    }

    pub(super) fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send(OP_TEXT, text.as_bytes())
    }

    fn send(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mask = random_bytes(&mut self.seed);
        let mask = [mask[0], mask[1], mask[2], mask[3]];
        self.stream
            .write_all(&encode_frame(opcode, payload, mask))?;
        self.stream.flush()
    }

    /// The next text message, or `None` if none arrived within `timeout`.
    pub(super) fn receive(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        loop {
            while let Some((frame, used)) = parse_frame(&self.buffer)? {
                self.buffer.drain(..used);
                if let Some(text) = self.accept(frame)? {
                    return Ok(Some(text));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.stream.set_read_timeout(Some(remaining))?;
            let mut chunk = vec![0_u8; 64 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the browser closed the connection",
                    ));
                }
                Ok(count) => self.buffer.extend_from_slice(&chunk[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Handles a frame; a finished text message comes back.
    fn accept(&mut self, frame: Frame) -> io::Result<Option<String>> {
        match frame.opcode {
            OP_PING => {
                self.send(OP_PONG, &frame.payload)?;
                Ok(None)
            }
            OP_PONG => Ok(None),
            OP_CLOSE => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the browser closed the connection",
            )),
            OP_TEXT | OP_BINARY | OP_CONTINUATION => {
                if self.message.len() + frame.payload.len() > MAX_MESSAGE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "message is too large",
                    ));
                }
                self.message.extend_from_slice(&frame.payload);
                if !frame.fin {
                    return Ok(None);
                }
                let bytes = std::mem::take(&mut self.message);
                String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "message is not text"))
            }
            _ => Ok(None),
        }
    }
}

fn find_header_end(head: &[u8]) -> Option<usize> {
    head.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

/// Mask keys only have to vary, so a clock-seeded generator is enough.
fn seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let low = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(0);
    low ^ (u64::from(std::process::id()) << 32) | 1
}

fn random_bytes(seed: &mut u64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    for half in bytes.chunks_mut(8) {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        half.copy_from_slice(&seed.to_le_bytes());
    }
    bytes
}

/// A `GET` to the browser's HTTP side on `port`, returning the body.
pub(super) fn http_get(port: u16, path: &str, timeout: Duration) -> io::Result<String> {
    http_request("GET", port, path, timeout)
}

/// An HTTP request to loopback `port` with `Connection: close`, returning
/// the body of a successful reply.
pub(super) fn http_request(
    method: &str,
    port: u16,
    path: &str,
    timeout: Duration,
) -> io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&loopback(port), timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
         Content-Length: 0\r\n\r\n"
    )?;
    let mut reply = Vec::new();
    stream
        .take(u64::try_from(MAX_MESSAGE).unwrap_or(u64::MAX))
        .read_to_end(&mut reply)?;
    let split = find_header_end(&reply).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "the browser's reply is invalid")
    })?;
    let head = String::from_utf8_lossy(&reply[..split]);
    let status = head.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(io::Error::other(format!(
            "the browser answered {method} {path} with: {status}"
        )));
    }
    Ok(String::from_utf8_lossy(&reply[split..]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_through_masking() -> io::Result<()> {
        let mask = [1, 2, 3, 4];
        for length in [0_usize, 5, 125, 126, 300, 70_000] {
            let payload = vec![b'a'; length];
            let bytes = encode_frame(OP_TEXT, &payload, mask);
            let (frame, used) = parse_frame(&bytes)?.ok_or(io::ErrorKind::UnexpectedEof)?;
            assert_eq!(used, bytes.len(), "{length}");
            assert_eq!(
                frame,
                Frame {
                    fin: true,
                    opcode: OP_TEXT,
                    payload
                }
            );
        }
        Ok(())
    }

    #[test]
    fn a_partial_frame_waits_for_the_rest() -> io::Result<()> {
        let bytes = encode_frame(OP_TEXT, b"hello", [9, 9, 9, 9]);
        for cut in 0..bytes.len() {
            assert!(parse_frame(&bytes[..cut])?.is_none(), "{cut}");
        }
        assert!(parse_frame(&bytes)?.is_some());
        Ok(())
    }

    #[test]
    fn server_frames_are_unmasked() -> io::Result<()> {
        let (frame, used) =
            parse_frame(&[0x81, 0x02, b'o', b'k', 0x88])?.ok_or(io::ErrorKind::UnexpectedEof)?;
        assert_eq!((frame.payload.as_slice(), used), (b"ok".as_slice(), 4));
        Ok(())
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut header = vec![0x81, 127];
        header.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(parse_frame(&header).is_err());
    }

    #[test]
    fn the_header_end_is_found() {
        assert_eq!(find_header_end(b"HTTP/1.1 101\r\n\r\nxyz"), Some(16));
        assert_eq!(find_header_end(b"HTTP/1.1 101\r\n"), None);
    }

    #[test]
    fn mask_keys_vary() {
        let mut seed = 7;
        assert_ne!(random_bytes(&mut seed), random_bytes(&mut seed));
    }
}
