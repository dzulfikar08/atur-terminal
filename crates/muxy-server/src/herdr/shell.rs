//! The part of herdr's client-shell protocol that carries images.
//!
//! herdr keeps every Kitty image an application sent in its own terminal and shows
//! them only to its client shells (the connection the herdr app makes): neither
//! `terminal session control` nor the socket API exposes them. A client shell asks
//! for one tab's surface and receives, besides the cells, the images that tab's
//! panes place: each image once per connection (`assets`) and the complete list of
//! clipped placements with every surface (`placements`).
//!
//! A frame is `[u32 LE length][bincode payload]`, bincode's standard configuration:
//! little-endian variable-length integers, strings and sequences prefixed by their
//! length, enum variants numbered in declaration order. The tags below are those of
//! herdr 0.9.5's `ClientMessage` and `ServerMessage`; the shell endpoint contract
//! ("generation 1") keeps them for good. Only what images need is written or read:
//! the cells of a surface are walked over, not kept.

use std::io::{self, Read, Write};

use muxy_terminal::CellSize;
use serde_json::json;

/// Largest frame herdr sends to a client that shows images (`MAX_GRAPHICS_FRAME_SIZE`).
pub(super) const MAX_FRAME: usize = 32 * 1024 * 1024;

const CLIENT_DETACH: u64 = 4;
const CLIENT_SHELL_RESIZE: u64 = 12;
const CLIENT_SHELL_ENDPOINT_REQUEST: u64 = 15;
const CLIENT_ENDPOINT_CONTROL: u64 = 20;

const SERVER_SHUTDOWN: u64 = 3;
const SERVER_PANE_SURFACE: u64 = 13;
const SERVER_SHELL_ERROR: u64 = 15;
const SERVER_ENDPOINT_RESPONSE: u64 = 18;
const SERVER_ENDPOINT_CONTROL: u64 = 20;

pub(super) const WELCOME: &str = "endpoint.welcome.v1";
pub(super) const SNAPSHOT: &str = "shell.snapshot.v1";

fn put_varint(out: &mut Vec<u8>, value: u64) {
    if let Ok(byte) = u8::try_from(value)
        && byte < 251
    {
        out.push(byte);
    } else if let Ok(short) = u16::try_from(value) {
        out.push(251);
        out.extend_from_slice(&short.to_le_bytes());
    } else if let Ok(word) = u32::try_from(value) {
        out.push(252);
        out.extend_from_slice(&word.to_le_bytes());
    } else {
        out.push(253);
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

/// `ClientMessage::EndpointControl` carrying the endpoint hello: a surface of
/// `cols` by `rows` cells, active, with every optional surface encoding left off so
/// that each update is a complete `PaneSurface`.
pub(super) fn hello(cols: u16, rows: u16, cell: CellSize) -> Vec<u8> {
    let data = json!({
        "generation": 1,
        "cell_width_px": cell.width,
        "cell_height_px": cell.height,
        "surface_size": {"cols": cols, "rows": rows},
        "pixel_mouse": false,
        "direct_graphics": false,
        "endpoint_keybindings": false,
        "mouse_capture": false,
        "surface_active": true,
        "surface_reuse": false,
        "surface_delta": false,
        "surface_scroll": false,
        "snapshot_codecs": ["shell.snapshot.v1"],
        "surface_codecs": ["shell.surface.v1"],
        "input_codecs": ["shell.input.semantic.v1"],
        "blob_codecs": ["shell.blob.v1"],
    });
    let mut out = Vec::new();
    put_varint(&mut out, CLIENT_ENDPOINT_CONTROL);
    put_str(&mut out, "endpoint.hello.v1");
    put_str(&mut out, &data.to_string());
    out
}

/// `ClientMessage::ClientShellResize`.
pub(super) fn resize(cols: u16, rows: u16, cell: CellSize) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, CLIENT_SHELL_RESIZE);
    put_varint(&mut out, u64::from(cell.width));
    put_varint(&mut out, u64::from(cell.height));
    put_varint(&mut out, u64::from(cols));
    put_varint(&mut out, u64::from(rows));
    out.push(0);
    out
}

/// `ClientMessage::ClientShellEndpointRequest` for an API request of the shell lane.
pub(super) fn focus_tab(boot: &str, tab: &str, request: u64) -> Vec<u8> {
    let body = json!({
        "id": format!("atur-{request}"),
        "method": "tab.focus",
        "params": {"tab_id": tab},
    });
    let mut out = Vec::new();
    put_varint(&mut out, CLIENT_SHELL_ENDPOINT_REQUEST);
    put_str(&mut out, boot);
    put_str(&mut out, &body.to_string());
    out
}

/// `ClientMessage::Detach`.
pub(super) fn detach() -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, CLIENT_DETACH);
    out
}

pub(super) fn write_frame(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len()).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(payload);
    writer.write_all(&frame)?;
    writer.flush()
}

/// Reads the next frame into `frame`, replacing what it held.
pub(super) fn read_frame(reader: &mut impl Read, frame: &mut Vec<u8>) -> io::Result<()> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = usize::try_from(u32::from_le_bytes(length)).map_err(io::Error::other)?;
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {length} byte frame exceeds {MAX_FRAME}"),
        ));
    }
    frame.clear();
    frame.resize(length, 0);
    reader.read_exact(frame)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Format {
    Rgb,
    Rgba,
    Png,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum Source {
    /// An image a pane's application sent, by the id it gave.
    Pane { pane: String, image: u32 },
    /// A popup's image or a layer: nothing this server shows.
    Other,
}

/// What identifies an image's bytes: herdr resends nothing that a connection holds.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct AssetKey {
    pub(super) source: Source,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) format: Format,
    pub(super) len: u64,
    pub(super) fingerprint: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Rect {
    pub(super) x: u16,
    pub(super) y: u16,
    pub(super) width: u16,
    pub(super) height: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Pane {
    pub(super) id: String,
    /// Where the pane's terminal starts on the surface.
    pub(super) inner: Rect,
}

/// One image placement, already clipped to the surface by herdr.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Placement {
    pub(super) asset: AssetKey,
    pub(super) id: u32,
    pub(super) x: u16,
    pub(super) y: u16,
    pub(super) cols: u32,
    pub(super) rows: u32,
    /// `[x, y, width, height]` of the part of the image shown, in pixels.
    pub(super) source: [u32; 4],
    /// Pixels from the top left of the first cell.
    pub(super) offset: [u32; 2],
    pub(super) z: i32,
}

#[derive(Debug, Default)]
pub(super) struct Surface {
    pub(super) panes: Vec<Pane>,
    /// Images this connection did not hold yet.
    pub(super) assets: Vec<(AssetKey, Vec<u8>)>,
    pub(super) placements: Vec<Placement>,
    /// Images that stay valid while no placement shows them.
    pub(super) retained: Vec<AssetKey>,
}

/// What a frame from herdr means here.
#[derive(Debug)]
pub(super) enum Message<'a> {
    /// The endpoint welcome, as JSON.
    Welcome(&'a str),
    /// The shell snapshot, as JSON; it names the boot that requests must quote.
    Snapshot(&'a str),
    Surface(Box<Surface>),
    /// The answer to a request, as JSON.
    Response(&'a [u8]),
    /// herdr refused something, or is stopping.
    Closed(String),
    Other,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn varint(&mut self) -> Option<u64> {
        match self.byte()? {
            251 => Some(u64::from(u16::from_le_bytes(
                self.take(2)?.try_into().ok()?,
            ))),
            252 => Some(u64::from(u32::from_le_bytes(
                self.take(4)?.try_into().ok()?,
            ))),
            253 => Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            254.. => None,
            byte => Some(u64::from(byte)),
        }
    }

    fn u32(&mut self) -> Option<u32> {
        u32::try_from(self.varint()?).ok()
    }

    fn u16(&mut self) -> Option<u16> {
        u16::try_from(self.varint()?).ok()
    }

    /// A signed integer: bincode zigzag-encodes it.
    fn i32(&mut self) -> Option<i32> {
        let value = self.u32()?;
        let magnitude = i32::try_from(value >> 1).ok()?;
        Some(if value & 1 == 0 {
            magnitude
        } else {
            -magnitude - 1
        })
    }

    fn bool(&mut self) -> Option<bool> {
        Some(self.byte()? != 0)
    }

    fn length(&mut self) -> Option<usize> {
        let length = usize::try_from(self.varint()?).ok()?;
        // Every element takes at least a byte: a longer claim is a corrupt frame.
        (length <= self.bytes.len() - self.at).then_some(length)
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let length = self.length()?;
        self.take(length)
    }

    fn string(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }

    fn skip_string(&mut self) -> Option<()> {
        self.bytes().map(drop)
    }

    fn option(&mut self) -> Option<bool> {
        match self.byte()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn rect(&mut self) -> Option<Rect> {
        Some(Rect {
            x: self.u16()?,
            y: self.u16()?,
            width: self.u16()?,
            height: self.u16()?,
        })
    }

    /// A `FrameData`: cells, size, cursor, hyperlinks and inline graphics bytes.
    fn skip_frame(&mut self) -> Option<()> {
        let cells = self.length()?;
        for _ in 0..cells {
            self.skip_string()?; // symbol
            self.varint()?; // fg
            self.varint()?; // bg
            self.varint()?; // modifier
            self.byte()?; // skip
            if self.option()? {
                self.varint()?; // hyperlink
            }
        }
        self.varint()?; // width
        self.varint()?; // height
        if self.option()? {
            self.varint()?; // cursor x
            self.varint()?; // cursor y
            self.byte()?; // visible
            self.byte()?; // shape
        }
        for _ in 0..self.length()? {
            self.skip_string()?;
        }
        self.bytes().map(drop)
    }

    fn pane(&mut self) -> Option<Pane> {
        let id = self.string()?;
        self.varint()?; // content revision
        self.rect()?; // rect
        let inner = self.rect()?;
        if self.option()? {
            self.rect()?; // scrollbar
        }
        if self.option()? {
            self.varint()?; // offset from bottom
            self.varint()?; // max offset from bottom
            self.varint()?; // viewport rows
        }
        for _ in 0..4 {
            self.byte()?; // focused, mouse reporting, SGR pixel mouse, alternate screen
        }
        self.varint()?; // pixel width
        self.varint()?; // pixel height
        Some(Pane { id, inner })
    }

    fn split(&mut self) -> Option<()> {
        self.varint()?; // direction
        self.varint()?; // position
        self.rect()?; // area
        self.rect()?; // hit rect
        let path = self.length()?;
        self.take(path).map(drop)
    }

    fn popup_size(&mut self) -> Option<()> {
        if self.option()? {
            match self.varint()? {
                0 => self.varint().map(drop),
                1 => self.byte().map(drop),
                _ => None,
            }
        } else {
            Some(())
        }
    }

    fn skip_popup(&mut self) -> Option<()> {
        self.skip_string()?; // terminal id
        self.skip_string()?; // title
        self.popup_size()?; // width
        self.popup_size()?; // height
        self.skip_frame()?;
        self.byte()?; // mouse reporting
        self.byte()?; // SGR pixel mouse
        self.varint()?; // pixel width
        self.varint()?; // pixel height
        Some(())
    }

    fn asset_key(&mut self) -> Option<AssetKey> {
        let source = match self.varint()? {
            0 => {
                let popup = match self.varint()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                let target = self.string()?;
                let image = self.u32()?;
                if popup {
                    Source::Other
                } else {
                    Source::Pane {
                        pane: target,
                        image,
                    }
                }
            }
            1 => {
                self.skip_string()?; // pane
                self.skip_string()?; // layer
                Source::Other
            }
            _ => return None,
        };
        let width = self.u32()?;
        let height = self.u32()?;
        let format = match self.varint()? {
            0 => Format::Rgb,
            1 => Format::Rgba,
            2 => Format::Png,
            _ => return None,
        };
        Some(AssetKey {
            source,
            width,
            height,
            format,
            len: self.varint()?,
            fingerprint: self.varint()?,
        })
    }

    fn placement(&mut self) -> Option<Placement> {
        let asset = self.asset_key()?;
        let id = self.u32()?;
        let x = self.u16()?;
        let y = self.u16()?;
        let cols = self.u32()?;
        let rows = self.u32()?;
        let source = [self.u32()?, self.u32()?, self.u32()?, self.u32()?];
        let offset = [self.u32()?, self.u32()?];
        let z = self.i32()?;
        self.u32()?; // rows scrolled back
        Some(Placement {
            asset,
            id,
            x,
            y,
            cols,
            rows,
            source,
            offset,
            z,
        })
    }

    fn surface(&mut self) -> Option<Surface> {
        self.skip_string()?; // boot id
        self.varint()?; // projection revision
        self.varint()?; // surface revision
        self.skip_frame()?;
        let mut surface = Surface::default();
        for _ in 0..self.length()? {
            surface.panes.push(self.pane()?);
        }
        for _ in 0..self.length()? {
            self.split()?;
        }
        if self.option()? {
            self.skip_popup()?;
        }
        for _ in 0..self.length()? {
            let key = self.asset_key()?;
            let data = self.bytes()?.to_vec();
            surface.assets.push((key, data));
        }
        for _ in 0..self.length()? {
            surface.placements.push(self.placement()?);
        }
        for _ in 0..self.length()? {
            surface.retained.push(self.asset_key()?);
        }
        Some(surface)
    }
}

/// Reads one frame. Whatever this server does not use, or cannot read, is `Other`.
pub(super) fn parse(frame: &[u8]) -> Message<'_> {
    let mut reader = Reader {
        bytes: frame,
        at: 0,
    };
    let Some(tag) = reader.varint() else {
        return Message::Other;
    };
    match tag {
        SERVER_PANE_SURFACE => reader.surface().map_or(Message::Other, |surface| {
            Message::Surface(Box::new(surface))
        }),
        SERVER_ENDPOINT_CONTROL => {
            let (Some(kind), Some(data)) = (reader.bytes(), reader.bytes()) else {
                return Message::Other;
            };
            let Ok(data) = std::str::from_utf8(data) else {
                return Message::Other;
            };
            match kind {
                kind if kind == WELCOME.as_bytes() => Message::Welcome(data),
                kind if kind == SNAPSHOT.as_bytes() => Message::Snapshot(data),
                _ => Message::Other,
            }
        }
        SERVER_ENDPOINT_RESPONSE => {
            // boot id, request id, final chunk, data
            let parsed = (|| {
                reader.skip_string()?;
                reader.skip_string()?;
                reader.bool()?;
                reader.bytes()
            })();
            parsed.map_or(Message::Other, Message::Response)
        }
        SERVER_SHELL_ERROR => Message::Closed(reader.string().unwrap_or_default()),
        SERVER_SHUTDOWN => {
            let reason = reader
                .option()
                .filter(|present| *present)
                .and_then(|_| reader.string());
            Message::Closed(reason.unwrap_or_else(|| "herdr is shutting down".into()))
        }
        _ => Message::Other,
    }
}

#[cfg(test)]
pub(super) mod encode {
    //! Builds frames the way herdr does, for tests.

    use super::{AssetKey, Format, Placement, Source, put_str, put_varint};

    pub(in crate::herdr) fn varint(out: &mut Vec<u8>, value: u64) {
        put_varint(out, value);
    }

    pub(in crate::herdr) fn string(out: &mut Vec<u8>, text: &str) {
        put_str(out, text);
    }

    pub(in crate::herdr) fn key(out: &mut Vec<u8>, key: &AssetKey) {
        match &key.source {
            Source::Pane { pane, image } => {
                varint(out, 0);
                varint(out, 0);
                string(out, pane);
                varint(out, u64::from(*image));
            }
            Source::Other => {
                varint(out, 1);
                string(out, "pane");
                string(out, "layer");
            }
        }
        varint(out, u64::from(key.width));
        varint(out, u64::from(key.height));
        varint(
            out,
            match key.format {
                Format::Rgb => 0,
                Format::Rgba => 1,
                Format::Png => 2,
            },
        );
        varint(out, key.len);
        varint(out, key.fingerprint);
    }

    pub(in crate::herdr) fn placement(out: &mut Vec<u8>, placement: &Placement) {
        key(out, &placement.asset);
        varint(out, u64::from(placement.id));
        varint(out, u64::from(placement.x));
        varint(out, u64::from(placement.y));
        varint(out, u64::from(placement.cols));
        varint(out, u64::from(placement.rows));
        for value in placement.source {
            varint(out, u64::from(value));
        }
        for value in placement.offset {
            varint(out, u64::from(value));
        }
        // Zigzag.
        let z = i64::from(placement.z);
        varint(out, u64::try_from((z << 1) ^ (z >> 63)).unwrap_or(0));
        varint(out, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::encode::{key, placement, string, varint};
    use super::*;

    fn asset(pane: &str, image: u32, width: u32, height: u32) -> AssetKey {
        AssetKey {
            source: Source::Pane {
                pane: pane.into(),
                image,
            },
            width,
            height,
            format: Format::Rgba,
            len: u64::from(width) * u64::from(height) * 4,
            fingerprint: 0x9e37_79b9_7f4a_7c15,
        }
    }

    /// A `PaneSurface` of two cells, one pane, an optional popup and the scene.
    fn surface_frame(
        popup: bool,
        assets: &[(AssetKey, Vec<u8>)],
        placements: &[Placement],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        varint(&mut out, SERVER_PANE_SURFACE);
        string(&mut out, "boot-1");
        varint(&mut out, 7); // projection revision
        varint(&mut out, 300); // surface revision
        frame_data(&mut out);
        varint(&mut out, 1); // panes
        string(&mut out, "w1:p1");
        varint(&mut out, 4); // content revision
        for rect in [[0, 0, 100, 40], [2, 3, 99, 40]] {
            for value in rect {
                varint(&mut out, value);
            }
        }
        out.push(1); // scrollbar
        for value in [99, 0, 1, 40] {
            varint(&mut out, value);
        }
        out.push(1); // scroll metrics
        for value in [0, 250, 40] {
            varint(&mut out, value);
        }
        out.extend_from_slice(&[1, 0, 0, 0]);
        varint(&mut out, 792);
        varint(&mut out, 640);
        varint(&mut out, 1); // splits
        varint(&mut out, 1);
        varint(&mut out, 50);
        for value in [0, 0, 10, 10, 0, 0, 1, 1] {
            varint(&mut out, value);
        }
        varint(&mut out, 2);
        out.extend_from_slice(&[1, 0]);
        if popup {
            out.push(1);
            string(&mut out, "term_popup");
            string(&mut out, "popup");
            out.push(1);
            varint(&mut out, 0);
            varint(&mut out, 300);
            out.push(1);
            varint(&mut out, 1);
            out.push(80);
            frame_data(&mut out);
            out.extend_from_slice(&[1, 0]);
            varint(&mut out, 400);
            varint(&mut out, 300);
        } else {
            out.push(0);
        }
        varint(&mut out, assets.len() as u64);
        for (asset, data) in assets {
            key(&mut out, asset);
            varint(&mut out, data.len() as u64);
            out.extend_from_slice(data);
        }
        varint(&mut out, placements.len() as u64);
        for item in placements {
            placement(&mut out, item);
        }
        varint(&mut out, 1); // retained
        key(&mut out, &asset("w9:p9", 1, 2, 2));
        out
    }

    fn frame_data(out: &mut Vec<u8>) {
        varint(out, 2);
        for symbol in ["é", " "] {
            string(out, symbol);
            varint(out, 0x02_ff_00_00); // fg
            varint(out, 0); // bg
            varint(out, 300); // modifier
            out.push(0);
            out.push(1);
            varint(out, 1); // hyperlink
        }
        varint(out, 2);
        varint(out, 1);
        out.push(1); // cursor
        varint(out, 5);
        varint(out, 0);
        out.extend_from_slice(&[1, 2]);
        varint(out, 1); // hyperlinks
        string(out, "https://example.com");
        varint(out, 0); // inline graphics
    }

    fn placed(asset: AssetKey, id: u32) -> Placement {
        Placement {
            asset,
            id,
            x: 4,
            y: 7,
            cols: 10,
            rows: 4,
            source: [0, 8, 64, 24],
            offset: [3, 5],
            z: -1,
        }
    }

    #[test]
    fn a_surface_yields_its_panes_assets_and_placements() {
        let image = asset("w1:p1", 7, 2, 1);
        let data = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let frame = surface_frame(
            false,
            &[(image.clone(), data.clone())],
            &[placed(image.clone(), 118_916)],
        );
        let Message::Surface(surface) = parse(&frame) else {
            panic!("not a surface");
        };
        assert_eq!(
            surface.panes,
            [Pane {
                id: "w1:p1".into(),
                inner: Rect {
                    x: 2,
                    y: 3,
                    width: 99,
                    height: 40
                }
            }]
        );
        assert_eq!(surface.assets, [(image.clone(), data)]);
        assert_eq!(surface.placements, [placed(image, 118_916)]);
        assert_eq!(surface.retained, [asset("w9:p9", 1, 2, 2)]);
    }

    #[test]
    fn a_popup_before_the_scene_is_walked_over() {
        let image = asset("w1:p1", 7, 1, 1);
        let frame = surface_frame(true, &[], &[placed(image.clone(), 0)]);
        let Message::Surface(surface) = parse(&frame) else {
            panic!("not a surface");
        };
        assert_eq!(surface.placements, [placed(image, 0)]);
    }

    #[test]
    fn a_truncated_or_forged_surface_is_not_a_surface() {
        let image = asset("w1:p1", 7, 1, 1);
        let frame = surface_frame(false, &[(image.clone(), vec![0; 4])], &[placed(image, 0)]);
        for end in [1, frame.len() / 2, frame.len() - 1] {
            assert!(matches!(parse(&frame[..end]), Message::Other), "{end}");
        }
        // A claim of more cells than bytes must not allocate or loop on it.
        let mut forged = Vec::new();
        varint(&mut forged, SERVER_PANE_SURFACE);
        string(&mut forged, "boot");
        varint(&mut forged, 1);
        varint(&mut forged, 1);
        varint(&mut forged, u64::from(u32::MAX));
        assert!(matches!(parse(&forged), Message::Other));
    }

    #[test]
    fn endpoint_controls_and_responses_are_told_apart() {
        let control = |kind: &str, data: &str| {
            let mut out = Vec::new();
            varint(&mut out, SERVER_ENDPOINT_CONTROL);
            string(&mut out, kind);
            string(&mut out, data);
            out
        };
        assert!(matches!(
            parse(&control(WELCOME, "{}")),
            Message::Welcome("{}")
        ));
        assert!(matches!(
            parse(&control(SNAPSHOT, "{\"boot_id\":\"b\"}")),
            Message::Snapshot(_)
        ));
        assert!(matches!(
            parse(&control("endpoint.agent-completions.v1", "{}")),
            Message::Other
        ));
        let mut response = Vec::new();
        varint(&mut response, SERVER_ENDPOINT_RESPONSE);
        string(&mut response, "boot");
        string(&mut response, "atur-1");
        response.push(1);
        varint(&mut response, 2);
        response.extend_from_slice(b"{}");
        assert!(matches!(parse(&response), Message::Response(b"{}")));
        let mut shutdown = Vec::new();
        varint(&mut shutdown, SERVER_SHUTDOWN);
        shutdown.push(1);
        string(&mut shutdown, "live update");
        assert!(matches!(parse(&shutdown), Message::Closed(reason) if reason == "live update"));
    }

    #[test]
    fn requests_are_framed_the_way_herdr_reads_them() {
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &resize(
                300,
                80,
                CellSize {
                    width: 9,
                    height: 18,
                },
            ),
        )
        .expect("write");
        // Length, variant 12, cell 9x18, surface 300x80 (300 needs the 2-byte form), no pixel mouse.
        assert_eq!(bytes, [8, 0, 0, 0, 12, 9, 18, 251, 44, 1, 80, 0]);
        let mut frame = Vec::new();
        read_frame(&mut bytes.as_slice(), &mut frame).expect("read");
        assert_eq!(frame, [12, 9, 18, 251, 44, 1, 80, 0]);
        assert_eq!(detach(), [4]);
        let request = focus_tab("boot", "w2:t1", 3);
        assert_eq!(request[..6], [15, 4, b'b', b'o', b'o', b't']);
        assert!(String::from_utf8_lossy(&request).contains("\"method\":\"tab.focus\""));
    }

    #[test]
    fn an_oversized_frame_is_refused() {
        let length = u32::try_from(MAX_FRAME + 1)
            .unwrap_or(u32::MAX)
            .to_le_bytes();
        let mut frame = Vec::new();
        assert!(read_frame(&mut length.as_slice(), &mut frame).is_err());
    }
}
