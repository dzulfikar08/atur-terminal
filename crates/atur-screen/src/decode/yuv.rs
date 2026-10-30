//! Colour conversion for the rare case that `VideoToolbox` returns a picture
//! GPUI cannot show directly.
#![allow(dead_code, reason = "only the macOS decoder converts pictures")]

/// Converts a bi-planar 4:2:0 picture (a full-resolution Y plane and an
/// interleaved half-resolution `CbCr` plane) to packed B, G, R, A bytes with A = 255.
/// BT.601 matrix, which is what GPUI's surface shader assumes too.
/// `None` when a plane is too small for `width` x `height` or the size is odd-sized in a way
/// the planes cannot describe.
pub(super) fn nv12_to_bgra(
    width: usize,
    height: usize,
    luma: &[u8],
    luma_stride: usize,
    chroma: &[u8],
    chroma_stride: usize,
    full_range: bool,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let luma_needed = (height - 1).checked_mul(luma_stride)?.checked_add(width)?;
    let chroma_needed = (chroma_height - 1)
        .checked_mul(chroma_stride)?
        .checked_add(chroma_width * 2)?;
    if luma_stride < width
        || chroma_stride < chroma_width * 2
        || luma.len() < luma_needed
        || chroma.len() < chroma_needed
    {
        return None;
    }

    let mut out = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
    for row in 0..height {
        let luma_row = &luma[row * luma_stride..row * luma_stride + width];
        let chroma_start = (row / 2) * chroma_stride;
        let chroma_row = &chroma[chroma_start..chroma_start + chroma_width * 2];
        let out_row = &mut out[row * width * 4..(row + 1) * width * 4];
        for (column, pixel) in out_row.chunks_exact_mut(4).enumerate() {
            let y = i32::from(luma_row[column]);
            let cb = i32::from(chroma_row[(column / 2) * 2]) - 128;
            let cr = i32::from(chroma_row[(column / 2) * 2 + 1]) - 128;
            let (red, green, blue) = if full_range {
                (
                    (256 * y + 359 * cr + 128) >> 8,
                    (256 * y - 88 * cb - 183 * cr + 128) >> 8,
                    (256 * y + 454 * cb + 128) >> 8,
                )
            } else {
                let scaled = 298 * (y - 16);
                (
                    (scaled + 409 * cr + 128) >> 8,
                    (scaled - 100 * cb - 208 * cr + 128) >> 8,
                    (scaled + 516 * cb + 128) >> 8,
                )
            };
            pixel[0] = clamp(blue);
            pixel[1] = clamp(green);
            pixel[2] = clamp(red);
            pixel[3] = 255;
        }
    }
    Some(out)
}

fn clamp(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, y: u8, cb: u8, cr: u8, full_range: bool) -> Vec<u8> {
        let luma = vec![y; width * height];
        let chroma: Vec<u8> = (0..width.div_ceil(2) * height.div_ceil(2))
            .flat_map(|_| [cb, cr])
            .collect();
        nv12_to_bgra(
            width,
            height,
            &luma,
            width,
            &chroma,
            width.div_ceil(2) * 2,
            full_range,
        )
        .unwrap_or_default()
    }

    #[test]
    fn video_range_white_black_and_red() {
        assert_eq!(solid(2, 2, 235, 128, 128, false)[..4], [255, 255, 255, 255]);
        assert_eq!(solid(2, 2, 16, 128, 128, false)[..4], [0, 0, 0, 255]);
        let red = solid(2, 2, 81, 90, 240, false);
        assert!(red[2] > 245 && red[1] < 8 && red[0] < 8, "{red:?}");
    }

    #[test]
    fn full_range_white_and_grey() {
        assert_eq!(solid(4, 2, 255, 128, 128, true)[..4], [255, 255, 255, 255]);
        assert_eq!(solid(4, 2, 128, 128, 128, true)[..4], [128, 128, 128, 255]);
    }

    #[test]
    fn odd_sizes_and_strides() {
        let out = solid(3, 3, 128, 128, 128, true);
        assert_eq!(out.len(), 3 * 3 * 4);
        assert!(
            out.chunks_exact(4)
                .all(|pixel| pixel == [128, 128, 128, 255])
        );

        // Rows padded past the visible width.
        let luma = vec![200u8; 8 * 2];
        let chroma = vec![128u8; 8];
        let out = nv12_to_bgra(4, 2, &luma, 8, &chroma, 8, true).unwrap_or_default();
        assert_eq!(out.len(), 4 * 2 * 4);
    }

    #[test]
    fn rejects_short_planes() {
        assert!(nv12_to_bgra(4, 4, &[0; 15], 4, &[128; 8], 4, true).is_none());
        assert!(nv12_to_bgra(4, 4, &[0; 16], 4, &[128; 7], 4, true).is_none());
        assert!(nv12_to_bgra(0, 4, &[], 0, &[], 0, true).is_none());
    }
}
