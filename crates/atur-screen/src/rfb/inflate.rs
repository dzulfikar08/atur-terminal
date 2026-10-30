//! Bounded zlib inflation shared by ZRLE and the extended clipboard.

use flate2::{Decompress, FlushDecompress, Status};

const MIN_GROWTH: usize = 16 * 1024;

/// Inflates all of `input` with `stream` into `out` (cleared first). Fails when the
/// output would exceed `limit` bytes, the data is corrupt, or `input` is not consumed.
pub(super) fn inflate_into(
    stream: &mut Decompress,
    input: &[u8],
    limit: usize,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    out.clear();
    let mut consumed = 0usize;
    loop {
        if out.capacity() == out.len() {
            let allowed = limit.saturating_add(1).saturating_sub(out.len());
            if allowed == 0 {
                return Err("decompressed data is larger than allowed".to_owned());
            }
            out.reserve(out.len().max(MIN_GROWTH).min(allowed));
        }
        let before_in = stream.total_in();
        let before_out = stream.total_out();
        let rest = input.get(consumed..).unwrap_or(&[]);
        let status = stream
            .decompress_vec(rest, out, FlushDecompress::Sync)
            .map_err(|error| format!("corrupt compressed data ({error})"))?;
        let used = usize::try_from(stream.total_in() - before_in).unwrap_or(usize::MAX);
        consumed = consumed.saturating_add(used);
        if out.len() > limit {
            return Err("decompressed data is larger than allowed".to_owned());
        }
        let produced = stream.total_out() != before_out;
        let output_has_room = out.capacity() > out.len();
        if matches!(status, Status::StreamEnd) {
            break;
        }
        if output_has_room && (consumed >= input.len() || (used == 0 && !produced)) {
            break;
        }
    }
    if consumed < input.len() {
        return Err("compressed data does not decode completely".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    fn compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn round_trip_larger_than_initial_capacity() {
        let data: Vec<u8> = (0..200_000u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let packed = compress(&data);
        let mut stream = Decompress::new(true);
        let mut out = Vec::new();
        inflate_into(&mut stream, &packed, 1 << 20, &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn output_limit_is_enforced() {
        let packed = compress(&vec![7u8; 100_000]);
        let mut stream = Decompress::new(true);
        let mut out = Vec::new();
        assert!(inflate_into(&mut stream, &packed, 1000, &mut out).is_err());
    }

    #[test]
    fn corrupt_data_is_an_error() {
        let mut stream = Decompress::new(true);
        let mut out = Vec::new();
        assert!(inflate_into(&mut stream, &[1, 2, 3, 4, 5, 6], 1000, &mut out).is_err());
    }
}
