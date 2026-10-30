//! Client-to-server RFB messages, appended to a byte vector.

pub(crate) const ENC_RAW: i32 = 0;
pub(crate) const ENC_COPY_RECT: i32 = 1;
pub(crate) const ENC_ZRLE: i32 = 16;
pub(crate) const ENC_H264: i32 = 50;
pub(crate) const ENC_LAST_RECT: i32 = -224;
pub(crate) const ENC_DESKTOP_SIZE: i32 = -223;
pub(crate) const ENC_EXT_DESKTOP_SIZE: i32 = -308;
pub(crate) const ENC_FENCE: i32 = -312;
pub(crate) const ENC_CONTINUOUS_UPDATES: i32 = -313;
/// 0xC0A1E5CE as the signed value that goes on the wire.
pub(crate) const ENC_EXT_CLIPBOARD: i32 = -1_063_131_698;

pub(crate) const MSG_SET_PIXEL_FORMAT: u8 = 0;
pub(crate) const MSG_SET_ENCODINGS: u8 = 2;
pub(crate) const MSG_UPDATE_REQUEST: u8 = 3;
pub(crate) const MSG_KEY_EVENT: u8 = 4;
pub(crate) const MSG_POINTER_EVENT: u8 = 5;
pub(crate) const MSG_CLIENT_CUT_TEXT: u8 = 6;
pub(crate) const MSG_ENABLE_CONTINUOUS_UPDATES: u8 = 150;
pub(crate) const MSG_FENCE: u8 = 248;

/// The encodings this client asks for, in order of preference.
pub(crate) fn encodings(h264: bool) -> Vec<i32> {
    let mut list = Vec::with_capacity(10);
    if h264 {
        list.push(ENC_H264);
    }
    list.extend([
        ENC_ZRLE,
        ENC_COPY_RECT,
        ENC_RAW,
        ENC_DESKTOP_SIZE,
        ENC_EXT_DESKTOP_SIZE,
        ENC_FENCE,
        ENC_CONTINUOUS_UPDATES,
        ENC_EXT_CLIPBOARD,
        ENC_LAST_RECT,
    ]);
    list
}

/// 32 bits per pixel, depth 24, little endian, true colour, shifts R16 G8 B0
/// (memory order B, G, R, unused).
pub(crate) fn set_pixel_format(out: &mut Vec<u8>) {
    out.extend_from_slice(&[MSG_SET_PIXEL_FORMAT, 0, 0, 0]);
    out.extend_from_slice(&[32, 24, 0, 1]);
    out.extend_from_slice(&255u16.to_be_bytes());
    out.extend_from_slice(&255u16.to_be_bytes());
    out.extend_from_slice(&255u16.to_be_bytes());
    out.extend_from_slice(&[16, 8, 0, 0, 0, 0]);
}

pub(crate) fn set_encodings(out: &mut Vec<u8>, encodings: &[i32]) {
    out.extend_from_slice(&[MSG_SET_ENCODINGS, 0]);
    let count = u16::try_from(encodings.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    for encoding in encodings.iter().take(usize::from(count)) {
        out.extend_from_slice(&encoding.to_be_bytes());
    }
}

/// A rectangle as `x, y, width, height`, clamped to what fits in the wire format.
fn push_area(out: &mut Vec<u8>, width: usize, height: usize) {
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&u16::try_from(width).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(&u16::try_from(height).unwrap_or(u16::MAX).to_be_bytes());
}

/// `FramebufferUpdateRequest` for the whole framebuffer.
pub(crate) fn update_request(out: &mut Vec<u8>, incremental: bool, width: usize, height: usize) {
    out.extend_from_slice(&[MSG_UPDATE_REQUEST, u8::from(incremental)]);
    push_area(out, width, height);
}

/// `EnableContinuousUpdates` for the whole framebuffer.
pub(crate) fn enable_continuous_updates(out: &mut Vec<u8>, width: usize, height: usize) {
    out.extend_from_slice(&[MSG_ENABLE_CONTINUOUS_UPDATES, 1]);
    push_area(out, width, height);
}

/// Fence response: the request bit is cleared, the payload is echoed.
pub(crate) fn fence(out: &mut Vec<u8>, flags: u32, payload: &[u8]) {
    out.extend_from_slice(&[MSG_FENCE, 0, 0, 0]);
    out.extend_from_slice(&flags.to_be_bytes());
    out.push(u8::try_from(payload.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(payload.get(..usize::from(u8::MAX)).unwrap_or(payload));
}

pub(crate) fn key_event(out: &mut Vec<u8>, down: bool, keysym: u32) {
    out.extend_from_slice(&[MSG_KEY_EVENT, u8::from(down), 0, 0]);
    out.extend_from_slice(&keysym.to_be_bytes());
}

pub(crate) fn pointer_event(out: &mut Vec<u8>, buttons: u8, x: u16, y: u16) {
    out.extend_from_slice(&[MSG_POINTER_EVENT, buttons]);
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
}

/// Legacy `ClientCutText`: Latin-1, characters outside it become `?`.
/// Returns `false` (and writes nothing) when the text is longer than the clipboard limit.
pub(crate) fn cut_text_latin1(out: &mut Vec<u8>, text: &str) -> bool {
    let bytes: Vec<u8> = text
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect();
    if bytes.len() > super::MAX_CLIPBOARD {
        return false;
    }
    let Ok(length) = u32::try_from(bytes.len()) else {
        return false;
    };
    out.extend_from_slice(&[MSG_CLIENT_CUT_TEXT, 0, 0, 0]);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&bytes);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_format_bytes() {
        let mut out = Vec::new();
        set_pixel_format(&mut out);
        assert_eq!(
            out,
            [
                0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn encodings_wire() {
        let mut out = Vec::new();
        set_encodings(&mut out, &[16, -223]);
        assert_eq!(out, [2, 0, 0, 2, 0, 0, 0, 16, 0xff, 0xff, 0xff, 0x21]);
        assert_eq!(
            ENC_EXT_CLIPBOARD,
            i32::from_be_bytes([0xc0, 0xa1, 0xe5, 0xce])
        );
    }

    #[test]
    fn requests_and_input() {
        let mut out = Vec::new();
        update_request(&mut out, true, 1920, 1080);
        assert_eq!(out, [3, 1, 0, 0, 0, 0, 0x07, 0x80, 0x04, 0x38]);
        out.clear();
        enable_continuous_updates(&mut out, 256, 2);
        assert_eq!(out, [150, 1, 0, 0, 0, 0, 1, 0, 0, 2]);
        out.clear();
        key_event(&mut out, true, 0xff0d);
        assert_eq!(out, [4, 1, 0, 0, 0, 0, 0xff, 0x0d]);
        out.clear();
        pointer_event(&mut out, 1, 258, 3);
        assert_eq!(out, [5, 1, 1, 2, 0, 3]);
        out.clear();
        fence(&mut out, 1, b"ab");
        assert_eq!(out, [248, 0, 0, 0, 0, 0, 0, 1, 2, b'a', b'b']);
    }

    #[test]
    fn latin1_cut_text() {
        let mut out = Vec::new();
        assert!(cut_text_latin1(&mut out, "h\u{e9}\u{20ac}"));
        assert_eq!(out, [6, 0, 0, 0, 0, 0, 0, 3, b'h', 0xe9, b'?']);
    }
}
