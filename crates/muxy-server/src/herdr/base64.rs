//! Standard-alphabet base64 with padding, the encoding of herdr's NDJSON byte fields.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(super) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = match *chunk {
            [a] => u32::from(a) << 16,
            [a, b] => u32::from(a) << 16 | u32::from(b) << 8,
            [a, b, c] => u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c),
            _ => 0,
        };
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(char::from(ALPHABET[(value >> shift & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn sextet(byte: u8) -> Option<u32> {
    Some(u32::from(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    }))
}

/// `None` for anything but canonical padded base64.
pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let last = bytes.len().saturating_sub(4);
    for (index, quad) in bytes.chunks_exact(4).enumerate() {
        let padding = if index * 4 == last {
            quad.iter().rev().take_while(|byte| **byte == b'=').count()
        } else {
            0
        };
        if padding > 2 {
            return None;
        }
        let mut value = 0_u32;
        for byte in &quad[..4 - padding] {
            value = value << 6 | sextet(*byte)?;
        }
        value <<= 6 * padding;
        for shift in [16, 8, 0].into_iter().take(3 - padding) {
            out.push((value >> shift).to_le_bytes()[0]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_padding_length() {
        for input in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"\x1b[?2026h\xff\x00",
        ] {
            assert_eq!(decode(&encode(input)).as_deref(), Some(input));
        }
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode(b"fo"), "Zm8=");
    }

    #[test]
    fn rejects_malformed_input() {
        for input in ["Zm9", "Zm9v!A==", "Z===", "=Zm8"] {
            assert_eq!(decode(input), None, "{input}");
        }
    }
}
