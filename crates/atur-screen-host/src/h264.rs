//! H.264 byte stream conversion: the encoder hands over length-prefixed (AVCC)
//! samples; RFB carries Annex B, with start codes and the parameter sets in
//! front of every key frame.

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NAL_IDR_SLICE: u8 = 5;

/// An access unit in Annex B form.
///
/// `sample` is the encoder's output: NAL units, each behind a big-endian length
/// of `length_size` bytes (1 to 4). `parameter_sets` (SPS, then PPS) go first
/// when `key`.
///
/// # Errors
/// When a length runs past the end of the sample.
pub fn annex_b(
    sample: &[u8],
    length_size: usize,
    parameter_sets: &[&[u8]],
    key: bool,
) -> Result<Vec<u8>, String> {
    if !(1..=4).contains(&length_size) {
        return Err(format!("unsupported NAL length size {length_size}"));
    }
    let mut out = Vec::with_capacity(sample.len() + 64);
    if key {
        for set in parameter_sets {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(set);
        }
    }
    let mut pos = 0;
    while pos < sample.len() {
        let prefix = sample
            .get(pos..pos + length_size)
            .ok_or("a NAL length is cut short")?;
        let length = prefix
            .iter()
            .fold(0_usize, |value, &byte| (value << 8) | usize::from(byte));
        pos += length_size;
        let unit = sample
            .get(pos..pos.checked_add(length).ok_or("a NAL length overflows")?)
            .ok_or("a NAL unit runs past the end of the sample")?;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(unit);
        pos += length;
    }
    Ok(out)
}

/// Whether an Annex B access unit holds an IDR picture (a key frame).
#[must_use]
pub fn has_idr(annex_b: &[u8]) -> bool {
    let mut zeros = 0_usize;
    for (index, &byte) in annex_b.iter().enumerate() {
        if byte == 1 && zeros >= 2 {
            if annex_b
                .get(index + 1)
                .is_some_and(|header| header & 0x1f == NAL_IDR_SLICE)
            {
                return true;
            }
            zeros = 0;
        } else if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_frame_carries_its_parameter_sets() {
        let sps = [0x67, 1, 2];
        let pps = [0x68, 3];
        let sample = [0, 0, 0, 3, 0x65, 9, 9, 0, 0, 0, 1, 0x41];
        let out = annex_b(&sample, 4, &[&sps, &pps], true).unwrap();
        assert_eq!(
            out,
            [
                0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0, 0, 0, 1,
                0x41
            ]
        );
        assert!(has_idr(&out));
    }

    #[test]
    fn a_delta_frame_has_no_parameter_sets() {
        let sample = [0, 0, 0, 2, 0x41, 7];
        let out = annex_b(&sample, 4, &[&[0x67, 1]], false).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x41, 7]);
        assert!(!has_idr(&out));
    }

    #[test]
    fn short_lengths_and_damage() {
        assert_eq!(
            annex_b(&[2, 0x41, 7], 1, &[], false).unwrap(),
            [0, 0, 0, 1, 0x41, 7]
        );
        assert!(annex_b(&[0, 0, 0, 9, 1], 4, &[], false).is_err());
        assert!(annex_b(&[0, 0], 4, &[], false).is_err());
        assert!(annex_b(&[], 5, &[], false).is_err());
        assert_eq!(annex_b(&[], 4, &[], false).unwrap(), Vec::<u8>::new());
    }
}
