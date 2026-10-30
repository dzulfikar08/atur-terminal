use super::*;

fn close(left: f32, right: f32) -> bool {
    (left - right).abs() < 0.01
}

fn rect(fit: Fit, view: (f32, f32), picture: (u32, u32), scale: f32, pan: (f32, f32)) -> Rect {
    placement(fit, view, picture, scale, pan).expect("placement")
}

#[test]
fn a_wide_view_letterboxes_the_picture_at_the_sides() {
    let placed = rect(Fit::Window, (2000.0, 540.0), (1920, 1080), 1.0, (0.5, 0.5));
    assert!(close(placed.height, 540.0));
    assert!(close(placed.width, 960.0));
    assert!(close(placed.x, 520.0));
    assert!(close(placed.y, 0.0));
}

#[test]
fn a_tall_view_letterboxes_the_picture_above_and_below() {
    let placed = rect(Fit::Window, (960.0, 1000.0), (1920, 1080), 1.0, (0.5, 0.5));
    assert!(close(placed.width, 960.0));
    assert!(close(placed.height, 540.0));
    assert!(close(placed.y, 230.0));
}

#[test]
fn a_view_position_maps_to_the_picture_pixel_under_it() {
    let picture = (1920, 1080);
    let placed = rect(Fit::Window, (2000.0, 540.0), picture, 1.0, (0.5, 0.5));
    assert_eq!(picture_point(placed, picture, (1000.0, 270.0)), (960, 540));
    assert_eq!(picture_point(placed, picture, (520.0, 0.0)), (0, 0));
    assert_eq!(
        picture_point(placed, picture, (1479.25, 539.25)),
        (1918, 1078)
    );
}

#[test]
fn positions_in_the_letterbox_or_outside_the_view_stay_on_the_picture() {
    let picture = (1920, 1080);
    let placed = rect(Fit::Window, (2000.0, 540.0), picture, 1.0, (0.5, 0.5));
    assert_eq!(picture_point(placed, picture, (10.0, 10.25)), (0, 20));
    assert_eq!(picture_point(placed, picture, (-300.0, -300.0)), (0, 0));
    assert_eq!(
        picture_point(placed, picture, (9000.0, 9000.0)),
        (1919, 1079)
    );
}

#[test]
fn a_small_picture_is_centered_at_its_own_size() {
    let picture = (800, 600);
    let placed = rect(Fit::Actual, (1000.0, 800.0), picture, 1.0, (0.0, 0.0));
    assert!(close(placed.x, 100.0));
    assert!(close(placed.y, 100.0));
    assert!(close(placed.width, 800.0));
    assert_eq!(picture_point(placed, picture, (110.5, 100.5)), (10, 0));
}

#[test]
fn a_sharp_display_shows_one_picture_pixel_per_screen_pixel() {
    let placed = rect(Fit::Actual, (1000.0, 500.0), (2000, 1000), 2.0, (0.0, 0.0));
    assert!(close(placed.width, 1000.0));
    assert!(close(placed.height, 500.0));
    assert!(close(placed.x, 0.0));
}

#[test]
fn a_large_picture_slides_so_the_pointer_stays_over_the_pixel_it_acts_on() {
    let view = (1000.0, 500.0);
    let picture = (3000, 1500);
    for (fraction, expected_x, expected_y) in [
        (0.0_f32, 0_i32, 0_i32),
        (0.25, 750, 375),
        (0.5, 1500, 750),
        (1.0, 2999, 1499),
    ] {
        let point = (view.0 * fraction, view.1 * fraction);
        let placed = rect(Fit::Actual, view, picture, 1.0, pan(view, point));
        assert!(placed.x <= 0.01 && placed.x >= view.0 - 3000.0 - 0.01);
        let (x, y) = picture_point(placed, picture, point);
        assert!((x - expected_x).abs() <= 1, "{fraction}: {x}");
        assert!((y - expected_y).abs() <= 1, "{fraction}: {y}");
    }
}

#[test]
fn the_pointer_position_in_the_view_pans_within_its_range() {
    assert_eq!(pan((100.0, 50.0), (50.0, 25.0)), (0.5, 0.5));
    assert_eq!(pan((100.0, 50.0), (-20.0, 90.0)), (0.0, 1.0));
    assert_eq!(pan((0.0, 0.0), (5.0, 5.0)), (0.5, 0.5));
}

#[test]
fn nothing_is_placed_without_a_picture_or_a_view() {
    assert!(placement(Fit::Window, (100.0, 100.0), (0, 10), 1.0, (0.5, 0.5)).is_none());
    assert!(placement(Fit::Window, (0.0, 100.0), (10, 10), 1.0, (0.5, 0.5)).is_none());
    assert!(placement(Fit::Actual, (100.0, -1.0), (10, 10), 1.0, (0.5, 0.5)).is_none());
}

#[test]
fn a_degenerate_rectangle_maps_to_the_first_pixel() {
    let flat = Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };
    assert_eq!(picture_point(flat, (100, 100), (0.0, 0.0)), (0, 0));
}

#[test]
fn the_fit_modes_alternate() {
    assert_eq!(Fit::default(), Fit::Window);
    assert_eq!(Fit::Window.toggled(), Fit::Actual);
    assert_eq!(Fit::Actual.toggled(), Fit::Window);
}

#[test]
fn line_wheels_send_whole_notches() {
    let mut remainder = (0.0, 0.0);
    assert_eq!(notches((0.0, 1.0), false, &mut remainder), (0, 1));
    assert_eq!(notches((0.0, -2.0), false, &mut remainder), (0, -2));
    assert_eq!(notches((1.0, 0.0), false, &mut remainder), (1, 0));
}

#[test]
fn smooth_scrolling_accumulates_until_a_notch_is_reached() {
    let mut remainder = (0.0, 0.0);
    assert_eq!(notches((0.0, 20.0), true, &mut remainder), (0, 0));
    assert_eq!(notches((0.0, 20.0), true, &mut remainder), (0, 1));
    assert_eq!(notches((0.0, 100.0), true, &mut remainder), (0, 2));
    assert!(remainder.1.abs() < 0.51);
}

#[test]
fn reversing_the_wheel_drops_what_was_left_over() {
    let mut remainder = (0.0, 0.0);
    assert_eq!(notches((0.0, 30.0), true, &mut remainder), (0, 0));
    assert_eq!(notches((0.0, -10.0), true, &mut remainder), (0, 0));
    assert_eq!(notches((0.0, -30.0), true, &mut remainder), (0, -1));
}

#[test]
fn a_flood_of_scrolling_is_bounded() {
    let mut remainder = (0.0, 0.0);
    let (_, count) = notches((0.0, 1_000_000.0), false, &mut remainder);
    assert_eq!(count, 100);
}
