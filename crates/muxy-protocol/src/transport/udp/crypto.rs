//! Authenticated encryption for datagrams.
//!
//! Both ends hold one random secret that ssh carried to the client. Each
//! direction derives its own ChaCha20-Poly1305 key from it, so a packet can
//! never be reflected back at its sender. A packet is an 8-byte counter
//! followed by the sealed bytes; the counter is also the nonce, and a sliding
//! window refuses any counter it has seen or that is too old. A packet that
//! fails to open is dropped without a reply, so nothing outside the session
//! learns that anything listens.

use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};

pub(super) const SECRET_LEN: usize = 32;
const TAG_LEN: usize = 16;
const COUNTER_LEN: usize = 8;
/// Bytes a sealed packet adds to its contents.
pub(super) const OVERHEAD: usize = COUNTER_LEN + TAG_LEN;
const WINDOW_WORDS: usize = 32;
const WINDOW_BITS: u64 = (WINDOW_WORDS as u64) * 64;

/// The secret both ends share for one session.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret([u8; SECRET_LEN]);

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(..)")
    }
}

impl Secret {
    pub fn generate() -> std::io::Result<Self> {
        let mut bytes = [0; SECRET_LEN];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| std::io::Error::other("no random numbers available"))?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: [u8; SECRET_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }

    /// Unpadded URL-safe base64, as ssh carries it.
    pub fn encode(&self) -> String {
        base64_encode(&self.0)
    }

    pub fn decode(text: &str) -> Option<Self> {
        let bytes = base64_decode(text)?;
        Some(Self(bytes.try_into().ok()?))
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut word = [0u8; 3];
        word[..chunk.len()].copy_from_slice(chunk);
        let bits = u32::from_be_bytes([0, word[0], word[1], word[2]]);
        for index in 0..=chunk.len() {
            let sextet = (bits >> (18 - 6 * index)) & 0x3f;
            text.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    text
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let mut bits = 0u32;
    let mut count = 0;
    for character in text.bytes() {
        let sextet = ALPHABET
            .iter()
            .position(|&candidate| candidate == character)?;
        bits = (bits << 6) | u32::try_from(sextet).ok()?;
        count += 6;
        if count >= 8 {
            count -= 8;
            bytes.push(u8::try_from((bits >> count) & 0xff).ok()?);
        }
    }
    // Leftover bits must be zero padding, or the text was not canonical.
    (bits & ((1 << count) - 1) == 0).then_some(bytes)
}

/// Which end of the session an endpoint is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Role {
    Client,
    Server,
}

impl Role {
    fn sends(self) -> &'static [u8] {
        match self {
            Self::Client => b"atur-udp/1 client to server",
            Self::Server => b"atur-udp/1 server to client",
        }
    }

    fn receives(self) -> &'static [u8] {
        match self {
            Self::Client => Self::Server.sends(),
            Self::Server => Self::Client.sends(),
        }
    }

    fn direction(self) -> u8 {
        match self {
            Self::Client => 0,
            Self::Server => 1,
        }
    }
}

fn derive(secret: &Secret, label: &[u8]) -> std::io::Result<LessSafeKey> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"atur-udp");
    let prk = salt.extract(&secret.0);
    let info = [label];
    let okm = prk
        .expand(&info, &aead::CHACHA20_POLY1305)
        .map_err(|_| std::io::Error::other("key derivation failed"))?;
    Ok(LessSafeKey::new(UnboundKey::from(okm)))
}

fn nonce(direction: u8, counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[0] = direction;
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::assume_unique_for_key(bytes)
}

/// Seals packets for one direction. Counters never repeat.
pub(super) struct Sealer {
    key: LessSafeKey,
    direction: u8,
    counter: u64,
}

impl Sealer {
    pub(super) fn new(secret: &Secret, role: Role) -> std::io::Result<Self> {
        Ok(Self {
            key: derive(secret, role.sends())?,
            direction: role.direction(),
            counter: 0,
        })
    }

    /// Appends the sealed `plain` to `out`.
    pub(super) fn seal(&mut self, plain: &[u8], out: &mut Vec<u8>) {
        let counter = self.counter;
        self.counter += 1;
        out.extend_from_slice(&counter.to_be_bytes());
        let start = out.len();
        out.extend_from_slice(plain);
        match self.key.seal_in_place_separate_tag(
            nonce(self.direction, counter),
            Aad::empty(),
            &mut out[start..],
        ) {
            Ok(tag) => out.extend_from_slice(tag.as_ref()),
            // Sealing fails only for inputs of gigabytes; send nothing usable.
            Err(_) => out.truncate(start - COUNTER_LEN),
        }
    }
}

/// Opens packets from the other end, refusing replays.
pub(super) struct Opener {
    key: LessSafeKey,
    direction: u8,
    window: ReplayWindow,
}

impl Opener {
    pub(super) fn new(secret: &Secret, role: Role) -> std::io::Result<Self> {
        let other = match role {
            Role::Client => Role::Server,
            Role::Server => Role::Client,
        };
        Ok(Self {
            key: derive(secret, role.receives())?,
            direction: other.direction(),
            window: ReplayWindow::default(),
        })
    }

    /// The packet's contents, appended to `out`, and its counter. A replayed,
    /// altered or foreign packet gives `None` and changes nothing.
    pub(super) fn open(&mut self, packet: &[u8], out: &mut Vec<u8>) -> Option<u64> {
        if packet.len() < OVERHEAD {
            return None;
        }
        let (counter, sealed) = packet.split_at(COUNTER_LEN);
        let counter = u64::from_be_bytes(counter.try_into().ok()?);
        if !self.window.fresh(counter) {
            return None;
        }
        let start = out.len();
        out.extend_from_slice(sealed);
        match self.key.open_in_place(
            nonce(self.direction, counter),
            Aad::empty(),
            &mut out[start..],
        ) {
            Ok(plain) => {
                let length = plain.len();
                out.truncate(start + length);
            }
            Err(_) => {
                out.truncate(start);
                return None;
            }
        }
        self.window.accept(counter);
        Some(counter)
    }

    pub(super) fn highest(&self) -> Option<u64> {
        self.window.highest
    }
}

/// Remembers which of the latest counters arrived.
struct ReplayWindow {
    highest: Option<u64>,
    seen: [u64; WINDOW_WORDS],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self {
            highest: None,
            seen: [0; WINDOW_WORDS],
        }
    }
}

impl ReplayWindow {
    fn slot(counter: u64) -> (usize, u64) {
        let bit = counter % WINDOW_BITS;
        // `bit` is below 2048, so both parts fit.
        (usize::try_from(bit / 64).unwrap_or(0), 1 << (bit % 64))
    }

    fn fresh(&self, counter: u64) -> bool {
        let Some(highest) = self.highest else {
            return true;
        };
        if counter > highest {
            return true;
        }
        if highest - counter >= WINDOW_BITS {
            return false;
        }
        let (word, mask) = Self::slot(counter);
        self.seen[word] & mask == 0
    }

    fn accept(&mut self, counter: u64) {
        match self.highest {
            None => {
                self.seen = [0; WINDOW_WORDS];
                self.highest = Some(counter);
            }
            Some(highest) if counter > highest => {
                // Forget the slots the window slides over.
                let advance = counter - highest;
                if advance >= WINDOW_BITS {
                    self.seen = [0; WINDOW_WORDS];
                } else {
                    for step in 1..=advance {
                        let (word, mask) = Self::slot(highest + step);
                        self.seen[word] &= !mask;
                    }
                }
                self.highest = Some(counter);
            }
            Some(_) => {}
        }
        let (word, mask) = Self::slot(counter);
        self.seen[word] |= mask;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Sealer, Opener, Sealer, Opener) {
        let secret = Secret::from_bytes([7; SECRET_LEN]);
        (
            Sealer::new(&secret, Role::Client).unwrap(),
            Opener::new(&secret, Role::Server).unwrap(),
            Sealer::new(&secret, Role::Server).unwrap(),
            Opener::new(&secret, Role::Client).unwrap(),
        )
    }

    #[test]
    fn a_sealed_packet_opens_once() {
        let (mut client, mut server, _, _) = pair();
        let mut packet = Vec::new();
        client.seal(b"hello", &mut packet);
        assert_eq!(packet.len(), 5 + OVERHEAD);
        let mut plain = Vec::new();
        assert_eq!(server.open(&packet, &mut plain), Some(0));
        assert_eq!(plain, b"hello");
        plain.clear();
        assert_eq!(server.open(&packet, &mut plain), None, "replay");
        assert!(plain.is_empty());
    }

    #[test]
    fn altered_truncated_and_reflected_packets_are_refused() {
        let (mut client, mut server, _, mut client_opener) = pair();
        let mut packet = Vec::new();
        client.seal(b"hello", &mut packet);
        let mut plain = Vec::new();
        for index in 0..packet.len() {
            let mut altered = packet.clone();
            altered[index] ^= 1;
            assert_eq!(server.open(&altered, &mut plain), None, "byte {index}");
        }
        assert_eq!(server.open(&packet[..OVERHEAD - 1], &mut plain), None);
        assert_eq!(client_opener.open(&packet, &mut plain), None, "reflection");
        assert!(plain.is_empty());
        assert_eq!(server.open(&packet, &mut plain), Some(0), "still fresh");
    }

    #[test]
    fn another_secret_opens_nothing() {
        let (mut client, _, _, _) = pair();
        let mut other = Opener::new(&Secret::from_bytes([8; SECRET_LEN]), Role::Server).unwrap();
        let mut packet = Vec::new();
        client.seal(b"hello", &mut packet);
        assert_eq!(other.open(&packet, &mut Vec::new()), None);
    }

    #[test]
    fn reordered_packets_open_but_old_ones_do_not() {
        let (mut client, mut server, _, _) = pair();
        let packets: Vec<Vec<u8>> = (0..3000)
            .map(|_| {
                let mut packet = Vec::new();
                client.seal(b"x", &mut packet);
                packet
            })
            .collect();
        let mut plain = Vec::new();
        assert!(server.open(&packets[10], &mut plain).is_some());
        assert!(server.open(&packets[5], &mut plain).is_some());
        assert!(server.open(&packets[5], &mut plain).is_none());
        assert!(server.open(&packets[2500], &mut plain).is_some());
        assert!(server.open(&packets[10], &mut plain).is_none(), "too old");
        assert!(server.open(&packets[2499], &mut plain).is_some());
        assert_eq!(server.highest(), Some(2500));
    }

    #[test]
    fn secrets_round_trip_through_text() {
        let secret = Secret::generate().unwrap();
        let text = secret.encode();
        assert_eq!(text.len(), 43);
        assert_eq!(Secret::decode(&text), Some(secret));
        assert_eq!(Secret::decode("short"), None);
        assert_eq!(Secret::decode(&"!".repeat(43)), None);
    }
}
