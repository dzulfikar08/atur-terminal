//! How this computer's keys and modifiers become the remote keyboard's.

use atur_screen::keys::{ALT_L, CONTROL_L, SHIFT_L, SUPER_L};
use gpui::{Keystroke, Modifiers};

/// The modifier keys the remote keyboard holds down.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "One flag for each modifier key the remote keyboard has"
)]
pub(super) struct Held {
    shift: bool,
    control: bool,
    alt: bool,
    super_key: bool,
}

impl Held {
    fn keys(self) -> [(u32, bool); 4] {
        [
            (SHIFT_L, self.shift),
            (CONTROL_L, self.control),
            (ALT_L, self.alt),
            (SUPER_L, self.super_key),
        ]
    }
}

/// The remote modifiers for the ones held here. On a Mac, Command acts as the
/// remote Control, so Command+C and Command+V copy and paste there as they do
/// here, and Option is Alt. Elsewhere each modifier stays itself.
pub(super) fn held(modifiers: Modifiers, mac: bool) -> Held {
    if mac {
        Held {
            shift: modifiers.shift,
            control: modifiers.control || modifiers.platform,
            alt: modifiers.alt,
            super_key: false,
        }
    } else {
        Held {
            shift: modifiers.shift,
            control: modifiers.control,
            alt: modifiers.alt,
            super_key: modifiers.platform,
        }
    }
}

/// The modifier key presses and releases that take the remote keyboard from
/// `before` to `after`, each as a keysym and whether it goes down.
pub(super) fn changes(before: Held, after: Held) -> Vec<(u32, bool)> {
    before
        .keys()
        .into_iter()
        .zip(after.keys())
        .filter(|((_, was), (_, is))| was != is)
        .map(|(_, (keysym, is))| (keysym, is))
        .collect()
}

/// The one chord the view keeps for itself: Control+Alt+G (Control+Option+G on
/// a Mac) shows or hides its toolbar. Every other key goes to the remote
/// computer, including those the app uses.
pub(super) fn is_reserved(keystroke: &Keystroke) -> bool {
    let modifiers = &keystroke.modifiers;
    keystroke.key == "g"
        && modifiers.control
        && modifiers.alt
        && !modifiers.shift
        && !modifiers.platform
}

/// How the reserved chord is written for the user.
pub(super) fn chord_label(mac: bool) -> &'static str {
    if mac { "⌃⌥G" } else { "Ctrl+Alt+G" }
}

/// The text a key press typed, which names its keysym. With Control, Alt or
/// Command held, what the system reports is a control or composed character,
/// so the key itself decides.
pub(super) fn typed(keystroke: &Keystroke) -> Option<&str> {
    let modifiers = &keystroke.modifiers;
    if modifiers.control || modifiers.alt || modifiers.platform {
        None
    } else {
        keystroke.key_char.as_deref()
    }
}

#[cfg(test)]
mod tests;
