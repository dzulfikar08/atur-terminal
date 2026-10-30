use std::sync::Arc;

use super::*;

fn frame(width: u32, height: u32, bytes: usize) -> atur_screen::Frame {
    atur_screen::Frame {
        id: 1,
        width,
        height,
        pixels: atur_screen::Pixels::Bgra(Arc::from(vec![0_u8; bytes])),
    }
}

#[test]
fn a_picture_of_plain_pixels_becomes_a_texture() {
    assert!(texture(&frame(4, 3, 4 * 3 * 4)).is_some());
}

#[test]
fn a_picture_whose_pixels_do_not_fit_its_size_is_not_drawn() {
    assert!(texture(&frame(4, 3, 4 * 3 * 4 - 1)).is_none());
    assert!(texture(&frame(4, 3, 4 * 3 * 4 + 4)).is_none());
}

#[test]
fn statistics_name_the_codec_and_the_rates() {
    let stats = ScreenStats {
        codec: "h264 (ffmpeg vaapi)".into(),
        fps: 59.6,
        kbps: 3201.2,
        ..ScreenStats::default()
    };
    assert_eq!(
        stats_label(&stats),
        "h264 (ffmpeg vaapi) · 60 fps · 3201 kbps"
    );
    assert_eq!(stats_label(&ScreenStats::default()), "");
}
