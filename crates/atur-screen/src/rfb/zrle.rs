//! ZRLE (encoding 16) for 32 bits per pixel, depth 24: CPIXELs are 3 bytes, B, G, R.

use flate2::Decompress;

use super::inflate::inflate_into;
use super::{Framebuffer, ProtocolError, Rect, protocol_error};

const TILE: usize = 64;

/// One persistent zlib stream per connection plus a scratch buffer for its output.
pub(super) struct Zrle {
    stream: Decompress,
    scratch: Vec<u8>,
}

impl Zrle {
    pub(super) fn new() -> Self {
        Self {
            stream: Decompress::new(true),
            scratch: Vec::new(),
        }
    }

    /// Inflates `compressed` (the body of one ZRLE rectangle) and paints it into `fb`.
    /// `rect` must already be validated against the framebuffer.
    pub(super) fn decode(
        &mut self,
        compressed: &[u8],
        rect: Rect,
        fb: &mut Framebuffer,
    ) -> Result<(), ProtocolError> {
        let tiles = rect.w.div_ceil(TILE) * rect.h.div_ceil(TILE);
        let limit = rect
            .w
            .saturating_mul(rect.h)
            .saturating_mul(4)
            .saturating_add(tiles)
            .saturating_add(64);
        inflate_into(&mut self.stream, compressed, limit, &mut self.scratch)
            .map_err(|reason| protocol_error(format!("ZRLE data rejected: {reason}")))?;
        let mut reader = Reader::new(&self.scratch);
        let stride = fb.width;
        for tile_y in (0..rect.h).step_by(TILE) {
            let tile_h = TILE.min(rect.h - tile_y);
            for tile_x in (0..rect.w).step_by(TILE) {
                let tile_w = TILE.min(rect.w - tile_x);
                let mut painter = Painter {
                    data: &mut fb.data,
                    stride,
                    x0: rect.x + tile_x,
                    y0: rect.y + tile_y,
                    tile_w,
                    total: tile_w * tile_h,
                    done: 0,
                };
                decode_tile(&mut reader, &mut painter, tile_w, tile_h)?;
            }
        }
        Ok(())
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self.pos.checked_add(count);
        let slice = end.and_then(|end| self.data.get(self.pos..end));
        match (slice, end) {
            (Some(slice), Some(end)) => {
                self.pos = end;
                Ok(slice)
            }
            _ => Err(protocol_error("ZRLE data ends in the middle of a tile")),
        }
    }

    fn byte(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?.first().copied().unwrap_or(0))
    }

    fn cpixel(&mut self) -> Result<[u8; 3], ProtocolError> {
        let bytes = self.take(3)?;
        Ok([bytes[0], bytes[1], bytes[2]])
    }

    /// A run length: 1 plus the sum of bytes up to and including the first one below 255.
    fn run_length(&mut self) -> Result<usize, ProtocolError> {
        let mut length = 1usize;
        loop {
            let byte = self.byte()?;
            length = length.saturating_add(usize::from(byte));
            if byte != 255 {
                return Ok(length);
            }
        }
    }
}

/// Writes pixels of one tile in reading order.
struct Painter<'a> {
    data: &'a mut [u8],
    stride: usize,
    x0: usize,
    y0: usize,
    tile_w: usize,
    total: usize,
    done: usize,
}

impl Painter<'_> {
    fn run(&mut self, color: [u8; 3], count: usize) -> Result<(), ProtocolError> {
        if count > self.total - self.done {
            return Err(protocol_error("ZRLE run is longer than its tile"));
        }
        let mut left = count;
        while left > 0 {
            let row = self.done / self.tile_w;
            let column = self.done % self.tile_w;
            let take = left.min(self.tile_w - column);
            let start = ((self.y0 + row) * self.stride + self.x0 + column) * 4;
            let Some(pixels) = self.data.get_mut(start..start + take * 4) else {
                return Err(protocol_error("ZRLE tile lies outside the framebuffer"));
            };
            for pixel in pixels.chunks_exact_mut(4) {
                pixel[0] = color[0];
                pixel[1] = color[1];
                pixel[2] = color[2];
                pixel[3] = 255;
            }
            self.done += take;
            left -= take;
        }
        Ok(())
    }
}

fn decode_tile(
    reader: &mut Reader<'_>,
    painter: &mut Painter<'_>,
    tile_w: usize,
    tile_h: usize,
) -> Result<(), ProtocolError> {
    let kind = reader.byte()?;
    let rle = kind & 0x80 != 0;
    let palette_size = usize::from(kind & 0x7f);
    match (rle, palette_size) {
        (false, 0) => {
            for _ in 0..tile_w * tile_h {
                let color = reader.cpixel()?;
                painter.run(color, 1)?;
            }
        }
        (false, 1) => {
            let color = reader.cpixel()?;
            painter.run(color, tile_w * tile_h)?;
        }
        (false, 2..=16) => {
            let palette = read_palette(reader, palette_size)?;
            let bits = match palette_size {
                2 => 1,
                3..=4 => 2,
                _ => 4,
            };
            let row_bytes = (tile_w * bits).div_ceil(8);
            for _ in 0..tile_h {
                let row = reader.take(row_bytes)?;
                for column in 0..tile_w {
                    let bit = column * bits;
                    let byte = row.get(bit / 8).copied().unwrap_or(0);
                    let shift = 8 - bits - (bit % 8);
                    let index = usize::from((byte >> shift) & ((1u8 << bits) - 1));
                    let color = palette_color(&palette, index)?;
                    painter.run(color, 1)?;
                }
            }
        }
        (true, 0) => {
            while painter.done < painter.total {
                let color = reader.cpixel()?;
                let length = reader.run_length()?;
                painter.run(color, length)?;
            }
        }
        (true, 2..=127) => {
            let palette = read_palette(reader, palette_size)?;
            while painter.done < painter.total {
                let index_byte = reader.byte()?;
                let length = if index_byte & 0x80 != 0 {
                    reader.run_length()?
                } else {
                    1
                };
                let color = palette_color(&palette, usize::from(index_byte & 0x7f))?;
                painter.run(color, length)?;
            }
        }
        _ => {
            return Err(protocol_error(format!(
                "ZRLE tile uses an unknown sub-encoding ({kind})"
            )));
        }
    }
    Ok(())
}

fn read_palette(reader: &mut Reader<'_>, size: usize) -> Result<Vec<[u8; 3]>, ProtocolError> {
    let mut palette = Vec::with_capacity(size);
    for _ in 0..size {
        palette.push(reader.cpixel()?);
    }
    Ok(palette)
}

fn palette_color(palette: &[[u8; 3]], index: usize) -> Result<[u8; 3], ProtocolError> {
    palette
        .get(index)
        .copied()
        .ok_or_else(|| protocol_error("ZRLE palette index is out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, FlushCompress};

    /// Compresses one rectangle's tile data and ends it with a sync flush, like a server.
    fn pack(encoder: &mut flate2::Compress, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + 256);
        encoder
            .compress_vec(data, &mut out, FlushCompress::Sync)
            .unwrap();
        out
    }

    fn fb(width: usize, height: usize) -> Framebuffer {
        Framebuffer::new(width, height)
    }

    fn px(fb: &Framebuffer, x: usize, y: usize) -> [u8; 4] {
        let at = (y * fb.width + x) * 4;
        [
            fb.data[at],
            fb.data[at + 1],
            fb.data[at + 2],
            fb.data[at + 3],
        ]
    }

    fn rect(x: usize, y: usize, w: usize, h: usize) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn solid_palette_packed_and_rle_tiles() {
        // 10 x 2 rect = one tile of 10 x 2.
        let mut encoder = flate2::Compress::new(Compression::fast(), true);
        let mut framebuffer = fb(12, 4);

        // Solid.
        let data = [1, 10, 20, 30];
        let mut zrle = Zrle::new();
        zrle.decode(
            &pack(&mut encoder, &data),
            rect(1, 1, 10, 2),
            &mut framebuffer,
        )
        .unwrap();
        assert_eq!(px(&framebuffer, 1, 1), [10, 20, 30, 255]);
        assert_eq!(px(&framebuffer, 10, 2), [10, 20, 30, 255]);
        assert_eq!(px(&framebuffer, 0, 0), [0, 0, 0, 255]);

        // Packed palette of 2 colours, 1 bit per pixel: row 0 = 1010101010, row 1 = 0.
        let mut data = vec![2, 1, 2, 3, 4, 5, 6];
        data.extend_from_slice(&[0b1010_1010, 0b1000_0000]);
        data.extend_from_slice(&[0, 0]);
        zrle.decode(
            &pack(&mut encoder, &data),
            rect(1, 1, 10, 2),
            &mut framebuffer,
        )
        .unwrap();
        assert_eq!(px(&framebuffer, 1, 1), [4, 5, 6, 255]);
        assert_eq!(px(&framebuffer, 2, 1), [1, 2, 3, 255]);
        assert_eq!(px(&framebuffer, 9, 1), [4, 5, 6, 255]);
        assert_eq!(px(&framebuffer, 10, 1), [1, 2, 3, 255]);
        assert_eq!(px(&framebuffer, 5, 2), [1, 2, 3, 255]);

        // Plain RLE: 15 pixels of one colour then 5 of another (run = 1 + sum of bytes).
        let data = [128, 9, 9, 9, 14, 7, 7, 7, 4];
        zrle.decode(
            &pack(&mut encoder, &data),
            rect(1, 1, 10, 2),
            &mut framebuffer,
        )
        .unwrap();
        assert_eq!(px(&framebuffer, 1, 1), [9, 9, 9, 255]);
        assert_eq!(px(&framebuffer, 10, 1), [9, 9, 9, 255]);
        assert_eq!(px(&framebuffer, 5, 2), [9, 9, 9, 255]);
        assert_eq!(px(&framebuffer, 6, 2), [7, 7, 7, 255]);
        assert_eq!(px(&framebuffer, 10, 2), [7, 7, 7, 255]);

        // Palette RLE: palette of 2; index 0 once, index 1 with a run of 19.
        let data = [130, 1, 1, 1, 2, 2, 2, 0, 0x81, 18];
        zrle.decode(
            &pack(&mut encoder, &data),
            rect(1, 1, 10, 2),
            &mut framebuffer,
        )
        .unwrap();
        assert_eq!(px(&framebuffer, 1, 1), [1, 1, 1, 255]);
        assert_eq!(px(&framebuffer, 2, 1), [2, 2, 2, 255]);
        assert_eq!(px(&framebuffer, 10, 2), [2, 2, 2, 255]);
    }

    #[test]
    fn raw_tile_and_multiple_tiles() {
        let mut encoder = flate2::Compress::new(Compression::fast(), true);
        let mut framebuffer = fb(70, 3);
        let mut zrle = Zrle::new();
        // 70 x 3: two tiles, 64 x 3 solid then 6 x 3 raw.
        let mut data = vec![1, 5, 5, 5];
        data.push(0);
        for i in 0..18u8 {
            data.extend_from_slice(&[i, i, i]);
        }
        zrle.decode(
            &pack(&mut encoder, &data),
            rect(0, 0, 70, 3),
            &mut framebuffer,
        )
        .unwrap();
        assert_eq!(px(&framebuffer, 63, 2), [5, 5, 5, 255]);
        assert_eq!(px(&framebuffer, 64, 0), [0, 0, 0, 255]);
        assert_eq!(px(&framebuffer, 69, 0), [5, 5, 5, 255]);
        assert_eq!(px(&framebuffer, 64, 1), [6, 6, 6, 255]);
        assert_eq!(px(&framebuffer, 69, 2), [17, 17, 17, 255]);
    }

    #[test]
    fn bad_tiles_are_rejected() {
        let mut framebuffer = fb(4, 4);
        for data in [
            vec![17u8],                     // unused sub-encoding
            vec![129],                      // unused sub-encoding
            vec![1, 1, 2],                  // truncated CPIXEL
            vec![128, 1, 2, 3, 99],         // run longer than the tile
            vec![130, 1, 1, 1, 2, 2, 2, 5], // palette index out of range
            vec![0, 1, 2, 3],               // raw tile too short
        ] {
            let mut encoder = flate2::Compress::new(Compression::fast(), true);
            let mut zrle = Zrle::new();
            let result = zrle.decode(
                &pack(&mut encoder, &data),
                rect(0, 0, 4, 4),
                &mut framebuffer,
            );
            assert!(result.is_err(), "{data:?}");
        }
    }
}
