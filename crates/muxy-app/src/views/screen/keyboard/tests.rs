use atur_screen::keys::{ALT_L, CONTROL_L, SHIFT_L, SUPER_L};
use gpui::{Keystroke, Modifiers};

use super::*;

#[allow(
    clippy::fn_params_excessive_bools,
    reason = "Spells out every modifier of a test case"
)]
fn keys(control: bool, alt: bool, shift: bool, platform: bool) -> Modifiers {
    Modifiers {
        control,
        alt,
        shift,
        platform,
        ..Modifiers::default()
    }
}

fn stroke(source: &str) -> Keystroke {
    Keystroke::parse(source).expect("keystroke")
}

#[test]
fn on_a_mac_command_is_control_and_option_is_alt() {
    let command = held(keys(false, false, false, true), true);
    assert_eq!(changes(Held::default(), command), [(CONTROL_L, true)]);

    let control = held(keys(true, false, false, false), true);
    assert_eq!(control, command);

    let option = held(keys(false, true, false, false), true);
    assert_eq!(changes(Held::default(), option), [(ALT_L, true)]);
}

#[test]
fn on_a_mac_nothing_is_the_remote_super_key() {
    let everything = held(keys(true, true, true, true), true);
    assert_eq!(
        changes(Held::default(), everything),
        [(SHIFT_L, true), (CONTROL_L, true), (ALT_L, true)]
    );
}

#[test]
fn elsewhere_each_modifier_stays_itself() {
    let everything = held(keys(true, true, true, true), false);
    assert_eq!(
        changes(Held::default(), everything),
        [
            (SHIFT_L, true),
            (CONTROL_L, true),
            (ALT_L, true),
            (SUPER_L, true)
        ]
    );
    let super_only = held(keys(false, false, false, true), false);
    assert_eq!(changes(Held::default(), super_only), [(SUPER_L, true)]);
}

#[test]
fn only_modifiers_that_changed_are_sent() {
    let shift = held(keys(false, false, true, false), false);
    let both = held(keys(true, false, true, false), false);
    assert_eq!(changes(shift, both), [(CONTROL_L, true)]);
    assert_eq!(changes(both, shift), [(CONTROL_L, false)]);
    assert!(changes(both, both).is_empty());
    assert_eq!(
        changes(both, Held::default()),
        [(SHIFT_L, false), (CONTROL_L, false)]
    );
}

#[test]
fn on_a_mac_control_stays_down_while_either_key_for_it_is_held() {
    let both = held(keys(true, false, false, true), true);
    let control = held(keys(true, false, false, false), true);
    let command = held(keys(false, false, false, true), true);
    assert!(changes(both, control).is_empty());
    assert!(changes(both, command).is_empty());
    assert_eq!(changes(control, Held::default()), [(CONTROL_L, false)]);
}

#[test]
fn only_control_alt_g_is_kept_by_the_view() {
    assert!(is_reserved(&stroke("ctrl-alt-g")));
    assert!(!is_reserved(&stroke("ctrl-g")));
    assert!(!is_reserved(&stroke("alt-g")));
    assert!(!is_reserved(&stroke("ctrl-alt-shift-g")));
    assert!(!is_reserved(&stroke("cmd-ctrl-alt-g")));
    assert!(!is_reserved(&stroke("ctrl-alt-h")));
    assert!(!is_reserved(&stroke("g")));
}

#[test]
fn keys_the_app_binds_are_not_reserved() {
    for chord in [
        "cmd-t",
        "ctrl-tab",
        "escape",
        "cmd-w",
        "cmd-shift-p",
        "ctrl-shift-tab",
    ] {
        assert!(!is_reserved(&stroke(chord)), "{chord}");
    }
}

#[test]
fn the_chord_is_written_for_the_platform() {
    assert_eq!(chord_label(true), "⌃⌥G");
    assert_eq!(chord_label(false), "Ctrl+Alt+G");
}

#[test]
fn typed_text_names_the_key_only_without_control_alt_or_command() {
    let mut plain = stroke("a");
    plain.key_char = Some("a".into());
    assert_eq!(typed(&plain), Some("a"));

    let mut shifted = stroke("shift-a");
    shifted.key_char = Some("A".into());
    assert_eq!(typed(&shifted), Some("A"));

    for chord in ["ctrl-a", "alt-s", "cmd-c"] {
        let mut keystroke = stroke(chord);
        keystroke.key_char = Some("\u{1}".into());
        assert_eq!(typed(&keystroke), None, "{chord}");
    }
    assert_eq!(typed(&stroke("enter")), None);
}
