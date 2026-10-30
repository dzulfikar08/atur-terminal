//! Platform-independent H.264 byte-stream helpers: Annex B splitting, AVCC
//! conversion and just enough of the SPS to learn the picture size.
//!
//! Every platform decoder uses a subset of these, so unused items are expected.
#![allow(dead_code, reason = "each platform decoder uses a different subset")]

pub(super) const NAL_SLICE: u8 = 1;
pub(super) const NAL_IDR_SLICE: u8 = 5;
pub(super) const NAL_SPS: u8 = 7;
pub(super) const NAL_PPS: u8 = 8;

/// Largest picture edge accepted from an SPS; anything bigger is a corrupt stream.
const MAX_DIMENSION: u64 = 16_384;

/// The `nal_unit_type` of one NAL unit (without start code), `None` when empty.
pub(super) fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|header| header & 0x1f)
}

/// Splits an Annex B byte stream into NAL units: start codes (`00 00 01` or
/// `00 00 00 01`) and trailing zero bytes are removed, empty units are skipped.
/// Returns nothing when the data holds no start code.
pub(super) fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<usize> = Vec::new();
    let mut zeros = 0usize;
    for (index, &byte) in data.iter().enumerate() {
        if byte == 1 && zeros >= 2 {
            starts.push(index + 1);
            zeros = 0;
        } else if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }

    let mut units = Vec::with_capacity(starts.len());
    for (position, &start) in starts.iter().enumerate() {
        let end = match starts.get(position + 1) {
            // The next start code occupies the three bytes before its payload.
            Some(&next) => next - 3,
            None => data.len(),
        };
        let mut unit = &data[start..end];
        while let [rest @ .., 0] = unit {
            unit = rest;
        }
        if !unit.is_empty() {
            units.push(unit);
        }
    }
    units
}

/// Joins NAL units into AVCC form: each unit behind a 4-byte big-endian length.
/// `None` when a unit is longer than `u32::MAX`.
pub(super) fn to_avcc(nals: &[&[u8]]) -> Option<Vec<u8>> {
    let total: usize = nals.iter().map(|nal| nal.len() + 4).sum();
    let mut out = Vec::with_capacity(total);
    for nal in nals {
        let length = u32::try_from(nal.len()).ok()?;
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(nal);
    }
    Some(out)
}

/// Picture size in luma samples after frame cropping, from an SPS NAL unit
/// (header byte included). `None` when the SPS is truncated or not plausible.
pub(super) fn sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    if nal_type(sps)? != NAL_SPS {
        return None;
    }
    let rbsp = unescape(sps.get(1..)?);
    let mut reader = BitReader::new(&rbsp);

    let profile_idc = reader.bits(8)?;
    reader.bits(8)?; // constraint flags and reserved bits
    reader.bits(8)?; // level_idc
    reader.ue()?; // seq_parameter_set_id

    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = reader.ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = reader.flag()?;
        }
        reader.ue()?; // bit_depth_luma_minus8
        reader.ue()?; // bit_depth_chroma_minus8
        reader.flag()?; // qpprime_y_zero_transform_bypass_flag
        if reader.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for index in 0..lists {
                if reader.flag()? {
                    reader.skip_scaling_list(if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    reader.ue()?; // log2_max_frame_num_minus4
    match reader.ue()? {
        0 => {
            reader.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            reader.flag()?; // delta_pic_order_always_zero_flag
            reader.se()?; // offset_for_non_ref_pic
            reader.se()?; // offset_for_top_to_bottom_field
            let cycle = reader.ue()?;
            if cycle > 255 {
                return None;
            }
            for _ in 0..cycle {
                reader.se()?; // offset_for_ref_frame
            }
        }
        _ => {}
    }
    reader.ue()?; // max_num_ref_frames
    reader.flag()?; // gaps_in_frame_num_value_allowed_flag
    let width_in_mbs = u64::from(reader.ue()?) + 1;
    let height_in_map_units = u64::from(reader.ue()?) + 1;
    let frame_mbs_only = reader.flag()?;
    if !frame_mbs_only {
        reader.flag()?; // mb_adaptive_frame_field_flag
    }
    reader.flag()?; // direct_8x8_inference_flag

    let (mut crop_left, mut crop_right, mut crop_top, mut crop_bottom) = (0u64, 0u64, 0u64, 0u64);
    if reader.flag()? {
        crop_left = u64::from(reader.ue()?);
        crop_right = u64::from(reader.ue()?);
        crop_top = u64::from(reader.ue()?);
        crop_bottom = u64::from(reader.ue()?);
    }

    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (unit_x, unit_y) = match chroma_array_type {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let field_factor = if frame_mbs_only { 1 } else { 2 };
    let unit_y = unit_y * field_factor;

    let width = (width_in_mbs * 16).checked_sub(unit_x * (crop_left + crop_right))?;
    let height =
        (height_in_map_units * 16 * field_factor).checked_sub(unit_y * (crop_top + crop_bottom))?;
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return None;
    }
    Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
}

/// Removes emulation prevention bytes (`00 00 03` becomes `00 00`).
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// MSB-first bit reader with Exp-Golomb support.
struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn flag(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.position / 8)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Some(bit == 1)
    }

    /// Reads up to 32 bits.
    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 1) | u64::from(self.flag()?);
        }
        u32::try_from(value).ok()
    }

    /// Unsigned Exp-Golomb code, `ue(v)`.
    fn ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u32;
        while !self.flag()? {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        let suffix = u64::from(self.bits(leading_zeros)?);
        u32::try_from((1u64 << leading_zeros) - 1 + suffix).ok()
    }

    /// Signed Exp-Golomb code, `se(v)`.
    fn se(&mut self) -> Option<i64> {
        let code = i64::from(self.ue()?);
        Some(if code % 2 == 1 {
            (code + 1) / 2
        } else {
            -(code / 2)
        })
    }

    /// Skips one `scaling_list()` of `size` entries.
    fn skip_scaling_list(&mut self, size: usize) -> Option<()> {
        let mut last = 8i64;
        let mut next = 8i64;
        for _ in 0..size {
            if next != 0 {
                next = (last + self.se()? + 256).rem_euclid(256);
            }
            if next != 0 {
                last = next;
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/solid-cbp.h264");

    /// SPS of `x264 --profile high`, 1920x1080 4:2:0: coded 1920x1088, cropped by 8 rows.
    const SPS_1080P: &[u8] = &[
        0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xc8, 0x3c, 0x60, 0xc6, 0x58,
    ];

    /// SPS of `x264 --profile high`, 1366x768: coded width 1376, cropped by 10 columns.
    const SPS_1366X768: &[u8] = &[
        0x67, 0x64, 0x00, 0x20, 0xac, 0xd9, 0x40, 0x56, 0x06, 0x1e, 0x6f, 0x01, 0x10, 0x00, 0x00,
        0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xf1, 0x83, 0x19, 0x60,
    ];

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 0,
        ];
        let units = split_annex_b(&data);
        assert_eq!(
            units,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9][..]]
        );
    }

    #[test]
    fn split_ignores_data_without_start_code() {
        assert!(split_annex_b(&[1, 2, 3, 4]).is_empty());
        assert!(split_annex_b(&[]).is_empty());
        assert!(split_annex_b(&[0, 0, 1]).is_empty());
    }

    #[test]
    fn split_keeps_emulation_prevented_zeros() {
        let data = [0, 0, 1, 0x41, 0, 0, 3, 1, 0x55];
        assert_eq!(split_annex_b(&data), vec![&[0x41, 0, 0, 3, 1, 0x55][..]]);
    }

    #[test]
    fn fixture_has_expected_unit_types() {
        let units = split_annex_b(FIXTURE);
        let types: Vec<u8> = units.iter().filter_map(|unit| nal_type(unit)).collect();
        assert_eq!(
            &types[..5],
            &[NAL_SPS, NAL_PPS, 6, NAL_IDR_SLICE, NAL_SLICE]
        );
        let key_frames = types.iter().filter(|&&kind| kind == NAL_IDR_SLICE).count();
        assert_eq!(key_frames, 2);
        let slices = types
            .iter()
            .filter(|&&kind| kind == NAL_SLICE || kind == NAL_IDR_SLICE)
            .count();
        assert_eq!(slices, 20);
    }

    #[test]
    fn avcc_prefixes_big_endian_lengths() {
        let a: &[u8] = &[0x65, 1, 2, 3];
        let b: &[u8] = &[0x41, 9];
        let avcc = to_avcc(&[a, b]).unwrap_or_default();
        assert_eq!(avcc, vec![0, 0, 0, 4, 0x65, 1, 2, 3, 0, 0, 0, 2, 0x41, 9]);
        assert_eq!(to_avcc(&[]), Some(Vec::new()));
    }

    #[test]
    fn fixture_sps_is_320_by_180() {
        let units = split_annex_b(FIXTURE);
        let sps = units.iter().find(|unit| nal_type(unit) == Some(NAL_SPS));
        assert_eq!(sps.and_then(|unit| sps_dimensions(unit)), Some((320, 180)));
    }

    #[test]
    fn high_profile_sps_applies_cropping() {
        assert_eq!(sps_dimensions(SPS_1080P), Some((1920, 1080)));
        assert_eq!(sps_dimensions(SPS_1366X768), Some((1366, 768)));
    }

    #[test]
    fn sps_parser_rejects_garbage() {
        assert_eq!(sps_dimensions(&[]), None);
        assert_eq!(sps_dimensions(&[0x67]), None);
        assert_eq!(sps_dimensions(&[0x68, 0x64, 0x00, 0x28]), None);
        assert_eq!(sps_dimensions(&SPS_1080P[..6]), None);
    }

    #[test]
    fn unescape_drops_prevention_bytes() {
        assert_eq!(
            unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]),
            vec![0, 0, 1, 0, 0, 0, 0]
        );
        assert_eq!(unescape(&[0, 3, 0, 0, 3]), vec![0, 3, 0, 0]);
    }

    #[test]
    fn exp_golomb_codes() {
        // 1 | 010 | 011 | 00100 | 00101 -> ue 0, 1, 2, 3, 4
        let bytes = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut reader = BitReader::new(&bytes);
        let values: Vec<Option<u32>> = (0..5).map(|_| reader.ue()).collect();
        assert_eq!(values, vec![Some(0), Some(1), Some(2), Some(3), Some(4)]);

        // se mapping: 1 -> 0, 010 -> 1, 011 -> -1, 00100 -> 2, 00101 -> -2
        let mut reader = BitReader::new(&bytes);
        let values: Vec<Option<i64>> = (0..5).map(|_| reader.se()).collect();
        assert_eq!(values, vec![Some(0), Some(1), Some(-1), Some(2), Some(-2)]);
    }
}
