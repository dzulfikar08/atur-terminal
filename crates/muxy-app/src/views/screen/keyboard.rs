//! How this computer's keys and modifiers become the remote keyboard's.

use std::collections::HashMap;

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

/// Whether letting go of the remote Control also lets go of the keys pressed
/// with it. On a Mac, `AppKit` sends no key release for a key pressed while
/// Command is held, so Command+C would leave `c` down on the remote computer.
pub(super) fn drops_keys(changes: &[(u32, bool)], mac: bool) -> bool {
    mac && changes.contains(&(CONTROL_L, false))
}

/// The remote keysym each key pressed here went down as. A key's name depends
/// on Shift when it is pressed or released (Shift+1 is `!`, then `1` once Shift
/// is up), so a release is matched by name only when the name is known.
#[derive(Debug, Default)]
pub(super) struct Pressed {
    keys: HashMap<String, u32>,
}

impl Pressed {
    pub(super) fn press(&mut self, key: &str, keysym: u32) {
        self.keys.insert(key.to_owned(), keysym);
    }

    /// The keysyms to release for the release of `key`. A key not known to be
    /// down releases every pressed key, since it may be one under another name.
    pub(super) fn release(&mut self, key: &str) -> Vec<u32> {
        match self.keys.remove(key) {
            Some(keysym) => vec![keysym],
            None => self.drain(),
        }
    }

    /// Every keysym held, forgetting them.
    pub(super) fn drain(&mut self) -> Vec<u32> {
        self.keys.drain().map(|(_, keysym)| keysym).collect()
    }

    pub(super) fn clear(&mut self) {
        self.keys.clear();
    }
}

#[cfg(test)]
mod tests;
