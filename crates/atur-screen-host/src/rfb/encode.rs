//! Pixel encodings for viewers that do not take video: raw and ZRLE, plus the
//! comparison that limits an update to what changed.

use flate2::{Compress, Compression, FlushCompress};

use super::message::{PixelFormat, rect_header, update_header};
use super::{ENC_RAW, ENC_ZRLE, Rect};

/// ZRLE works in tiles of this many pixels on a side.
pub const TILE: usize = 64;

/// A picture's pixels: B, G, R, A bytes, `stride` bytes per row.
#[derive(Clone, Copy, Debug)]
pub struct Pixels<'a> {
    pub stride: usize,
    pub bytes: &'a [u8],
    pub width: usize,
    pub height: usize,
}

impl Pixels<'_> {
    fn row(&self, y: usize, x: usize, count: usize) -> Option<&[u8]> {
        let start = y.checked_mul(self.stride)?.checked_add(x.checked_mul(4)?)?;
        self.bytes
            .get(start..start.checked_add(count.checked_mul(4)?)?)
    }

    fn contains(&self, rect: Rect) -> bool {
        rect.w > 0
            && rect.h > 0
            && rect.x + rect.w <= self.width
            && rect.y + rect.h <= self.height
            && self.row(rect.y + rect.h - 1, rect.x, rect.w).is_some()
    }
}

/// The smallest rectangle of whole tiles holding every difference between two
/// pictures of one size, the whole picture when there is no previous one, and
/// `None` when they are the same.
#[must_use]
pub fn changed_area(previous: Option<Pixels<'_>>, current: Pixels<'_>) -> Option<Rect> {
    let full = Rect {
        x: 0,
        y: 0,
        w: current.width,
        h: current.height,
    };
    let Some(previous) = previous else {
        return Some(full);
    };
    if previous.width != current.width || previous.height != current.height {
        return Some(full);
    }
    let (mut first_x, mut first_y) = (usize::MAX, usize::MAX);
    let (mut last_x, mut last_y) = (0, 0);
    for tile_y in (0..current.height).step_by(TILE) {
        let rows = TILE.min(current.height - tile_y);
        for tile_x in (0..current.width).step_by(TILE) {
            let columns = TILE.min(current.width - tile_x);
            let same = (tile_y..tile_y + rows).all(|y| {
                match (
                    previous.row(y, tile_x, columns),
                    current.row(y, tile_x, columns),
                ) {
                    (Some(old), Some(new)) => old == new,
                    _ => false,
                }
            });
            if !same {
                first_x = first_x.min(tile_x);
                first_y = first_y.min(tile_y);
                last_x = last_x.max(tile_x + columns);
                last_y = last_y.max(tile_y + rows);
            }
        }
    }
    (first_x != usize::MAX).then_some(Rect {
        x: first_x,
        y: first_y,
        w: last_x - first_x,
        h: last_y - first_y,
    })
}

/// A `FramebufferUpdate` of one raw rectangle. `None` when `rect` is not inside
/// the pixels.
#[must_use]
pub fn raw_update(format: PixelFormat, pixels: Pixels<'_>, rect: Rect) -> Option<Vec<u8>> {
    if !pixels.contains(rect) {
        return None;
    }
    let mut out = Vec::with_capacity(16 + rect.w * rect.h * 4);
    update_header(&mut out, 1);
    rect_header(&mut out, rect, ENC_RAW);
    for y in rect.y..rect.y + rect.h {
        let row = pixels.row(y, rect.x, rect.w)?;
        if format == PixelFormat::NATIVE {
            out.extend_from_slice(row);
        } else {
            for pixel in row.chunks_exact(4) {
                out.extend_from_slice(&format.pack(pixel[0], pixel[1], pixel[2]));
            }
        }
    }
    Some(out)
}

/// ZRLE with one zlib stream for the whole connection, as the encoding requires.
/// Tiles are solid colours or raw 3-byte pixels (B, G, R); the compressor does
/// the rest, which is plenty for desktops.
pub struct Zrle {
    stream: Compress,
    tiles: Vec<u8>,
}

impl std::fmt::Debug for Zrle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Zrle").finish_non_exhaustive()
    }
}

impl Default for Zrle {
    fn default() -> Self {
        Self::new()
    }
}

impl Zrle {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stream: Compress::new(Compression::fast(), true),
            tiles: Vec::new(),
        }
    }

    /// A `FramebufferUpdate` of one ZRLE rectangle. The viewer must be given
    /// every update this returns, in order, for the zlib stream to stay in step.
    ///
    /// # Errors
    /// When `rect` is not inside the pixels or compression fails.
    pub fn update(&mut self, pixels: Pixels<'_>, rect: Rect) -> Result<Vec<u8>, String> {
        if !pixels.contains(rect) {
            return Err("the rectangle is outside the picture".into());
        }
        self.tiles.clear();
        for tile_y in (rect.y..rect.y + rect.h).step_by(TILE) {
            let rows = TILE.min(rect.y + rect.h - tile_y);
            for tile_x in (rect.x..rect.x + rect.w).step_by(TILE) {
                let columns = TILE.min(rect.x + rect.w - tile_x);
                tile(&mut self.tiles, pixels, tile_x, tile_y, columns, rows);
            }
        }
        let compressed = self.deflate()?;
        let mut out = Vec::with_capacity(20 + compressed.len());
        update_header(&mut out, 1);
        rect_header(&mut out, rect, ENC_ZRLE);
        out.extend_from_slice(
            &u32::try_from(compressed.len())
                .map_err(|_| "the compressed rectangle is too large")?
                .to_be_bytes(),
        );
        out.extend_from_slice(&compressed);
        Ok(out)
    }

    /// Compresses the tiles and ends with a sync flush, so the viewer can
    /// inflate everything sent so far.
    fn deflate(&mut self) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(self.tiles.len() / 2 + 64);
        let mut consumed = 0;
        loop {
            if out.capacity() - out.len() < 64 {
                out.reserve(self.tiles.len().max(4096));
            }
            let before = self.stream.total_in();
            self.stream
                .compress_vec(&self.tiles[consumed..], &mut out, FlushCompress::Sync)
                .map_err(|error| format!("compression failed: {error}"))?;
            let taken = self.stream.total_in() - before;
            consumed += usize::try_from(taken).map_err(|_| "compression consumed too much")?;
            // A sync flush is complete once all input is taken and output space is left.
            if consumed == self.tiles.len() && out.len() < out.capacity() {
                return Ok(out);
            }
        }
    }
}

/// Appends one ZRLE tile: a solid colour (sub-encoding 1) or raw pixels (0).
fn tile(out: &mut Vec<u8>, pixels: Pixels<'_>, x: usize, y: usize, columns: usize, rows: usize) {
    let first = pixels
        .row(y, x, 1)
        .map(|pixel| [pixel[0], pixel[1], pixel[2]]);
    let solid = first.filter(|colour| {
        (y..y + rows).all(|row| {
            pixels
                .row(row, x, columns)
                .is_some_and(|line| line.chunks_exact(4).all(|pixel| pixel[..3] == colour[..]))
        })
    });
    if let Some(colour) = solid {
        out.push(1);
        out.extend_from_slice(&colour);
        return;
    }
    out.push(0);
    for row in y..y + rows {
        if let Some(line) = pixels.row(row, x, columns) {
            for pixel in line.chunks_exact(4) {
                out.extend_from_slice(&pixel[..3]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: usize, height: usize, paint: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let [b, g, r] = paint(x, y);
                bytes.extend_from_slice(&[b, g, r, 255]);
            }
        }
        bytes
    }

    fn view(bytes: &[u8], width: usize, height: usize) -> Pixels<'_> {
        Pixels {
            stride: width * 4,
            bytes,
            width,
            height,
        }
    }

    #[test]
    fn only_the_tiles_that_changed_are_sent() {
        let before = picture(200, 150, |_, _| [10, 20, 30]);
        let mut after = before.clone();
        assert_eq!(
            changed_area(Some(view(&before, 200, 150)), view(&after, 200, 150)),
            None
        );
        // One pixel in the tile at column 2, row 1 (x 128..192, y 64..128).
        let at = (70 * 200 + 130) * 4;
        after[at] = 99;
        assert_eq!(
            changed_area(Some(view(&before, 200, 150)), view(&after, 200, 150)),
            Some(Rect {
                x: 128,
                y: 64,
                w: 64,
                h: 64
            })
        );
        // Edge tiles are clipped to the picture.
        let at = (149 * 200 + 199) * 4;
        after[at] = 7;
        assert_eq!(
            changed_area(Some(view(&before, 200, 150)), view(&after, 200, 150)),
            Some(Rect {
                x: 128,
                y: 64,
                w: 72,
                h: 86
            })
        );
        assert_eq!(
            changed_area(None, view(&after, 200, 150)),
            Some(Rect {
                x: 0,
                y: 0,
                w: 200,
                h: 150
            })
        );
    }

    #[test]
    fn raw_pixels_follow_the_viewers_format() {
        let bytes = picture(2, 1, |x, _| [u8::try_from(x).unwrap() + 1, 2, 3]);
        let rect = Rect {
            x: 0,
            y: 0,
            w: 2,
            h: 1,
        };
        let native = raw_update(PixelFormat::NATIVE, view(&bytes, 2, 1), rect).unwrap();
        assert_eq!(&native[16..], [1, 2, 3, 255, 2, 2, 3, 255]);
        let rgbx = PixelFormat {
            big_endian: true,
            red_shift: 24,
            green_shift: 16,
            blue_shift: 8,
        };
        let packed = raw_update(rgbx, view(&bytes, 2, 1), rect).unwrap();
        assert_eq!(&packed[16..], [3, 2, 1, 0, 3, 2, 2, 0]);
        assert_eq!(
            raw_update(
                PixelFormat::NATIVE,
                view(&bytes, 2, 1),
                Rect {
                    x: 1,
                    y: 0,
                    w: 2,
                    h: 1
                }
            ),
            None
        );
    }

    #[test]
    fn zrle_round_trips_through_one_zlib_stream() {
        let (width, height) = (130, 70);
        let bytes = picture(width, height, |x, y| {
            if x < 64 {
                [5, 6, 7]
            } else {
                [
                    u8::try_from(x * 3 % 256).unwrap(),
                    u8::try_from(y).unwrap(),
                    9,
                ]
            }
        });
        let mut zrle = Zrle::new();
        let mut inflate = flate2::Decompress::new(true);
        let full = Rect {
            x: 0,
            y: 0,
            w: width,
            h: height,
        };
        for _ in 0..2 {
            let update = zrle.update(view(&bytes, width, height), full).unwrap();
            assert_eq!(&update[12..16], 16_i32.to_be_bytes());
            let length = usize::try_from(u32::from_be_bytes([
                update[16], update[17], update[18], update[19],
            ]))
            .unwrap();
            assert_eq!(update.len(), 20 + length);
            let mut tiles = Vec::with_capacity(width * height * 4);
            inflate
                .decompress_vec(&update[20..], &mut tiles, flate2::FlushDecompress::Sync)
                .unwrap();
            // First tile is solid; the second tile (x 64..128, 64 rows) is raw.
            assert_eq!(&tiles[..4], [1, 5, 6, 7]);
            assert_eq!(tiles[4], 0);
            assert_eq!(&tiles[5..8], [192, 0, 9]);
        }
    }
}
