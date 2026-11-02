//! Turning X11 keysyms from viewers into macOS key presses.
//!
//! Viewers send X11 keysyms (see the viewer's key table). macOS wants virtual key codes plus
//! modifier flags, so a keysym becomes one of three things: a modifier key (after the
//! [`KeyStyle`] remapping), a virtual key with the modifiers it needs, or a character to type
//! as text when no key on the current layout produces it.
//!
//! Everything here is plain data and runs on every platform; the `CGEvent` side lives in
//! `mac::input`.

use std::collections::HashMap;
use std::ops::{BitOr, BitOrAssign};

/// How the modifier keys of a viewer map onto the Mac's.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KeyStyle {
    /// Shortcut friendly: Control acts as Command, Alt as Option, Super and Meta as Control, so
    /// Ctrl+C, Ctrl+V, Ctrl+T from a Linux or Windows viewer are copy, paste and new tab.
    #[default]
    Command,
    /// Keys act as labelled: Control is Control, Alt is Option, Super and Meta are Command.
    Literal,
}

impl KeyStyle {
    /// The style named on the command line (`command` or `literal`).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "command" => Some(Self::Command),
            "literal" => Some(Self::Literal),
            _ => None,
        }
    }
}

/// A set of modifier keys: its own small bitset, translated to `CGEventFlags` by the injector.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Flags(pub u8);

impl Flags {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1);
    pub const OPTION: Self = Self(1 << 1);
    pub const CONTROL: Self = Self(1 << 2);
    pub const COMMAND: Self = Self(1 << 3);

    /// Whether every flag of `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether at least one flag of `other` is set.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for Flags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// A modifier key of the Mac keyboard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Modifier {
    Shift,
    Control,
    Option,
    Command,
    CapsLock,
}

impl Modifier {
    /// The macOS virtual key code of the left or right key.
    #[must_use]
    pub const fn keycode(self, right: bool) -> u16 {
        match (self, right) {
            (Self::Shift, false) => 0x38,
            (Self::Shift, true) => 0x3C,
            (Self::Control, false) => 0x3B,
            (Self::Control, true) => 0x3E,
            (Self::Option, false) => 0x3A,
            (Self::Option, true) => 0x3D,
            (Self::Command, false) => 0x37,
            (Self::Command, true) => 0x36,
            (Self::CapsLock, _) => 0x39,
        }
    }

    /// The flag the key sets while held. Caps Lock is a lock, not a held flag: it has none.
    #[must_use]
    pub const fn flag(self) -> Flags {
        match self {
            Self::Shift => Flags::SHIFT,
            Self::Control => Flags::CONTROL,
            Self::Option => Flags::OPTION,
            Self::Command => Flags::COMMAND,
            Self::CapsLock => Flags::NONE,
        }
    }
}

/// What a keysym means on the Mac.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resolved {
    /// A modifier key itself, after style remapping.
    Modifier { modifier: Modifier, right: bool },
    /// A virtual key. `flags` are the modifiers this keysym needs on top of the held ones
    /// (`!` needs Shift on a US layout).
    Key { keycode: u16, flags: Flags },
    /// No key on the current layout produces this character: type it as text.
    Text(char),
}

const KEYCODE_SPACE: u16 = 0x31;

/// Keys of a US keyboard: the character without Shift, with Shift, and the virtual key code.
const ANSI_KEYS: [(char, char, u16); 47] = [
    ('a', 'A', 0x00),
    ('b', 'B', 0x0B),
    ('c', 'C', 0x08),
    ('d', 'D', 0x02),
    ('e', 'E', 0x0E),
    ('f', 'F', 0x03),
    ('g', 'G', 0x05),
    ('h', 'H', 0x04),
    ('i', 'I', 0x22),
    ('j', 'J', 0x26),
    ('k', 'K', 0x28),
    ('l', 'L', 0x25),
    ('m', 'M', 0x2E),
    ('n', 'N', 0x2D),
    ('o', 'O', 0x1F),
    ('p', 'P', 0x23),
    ('q', 'Q', 0x0C),
    ('r', 'R', 0x0F),
    ('s', 'S', 0x01),
    ('t', 'T', 0x11),
    ('u', 'U', 0x20),
    ('v', 'V', 0x09),
    ('w', 'W', 0x0D),
    ('x', 'X', 0x07),
    ('y', 'Y', 0x10),
    ('z', 'Z', 0x06),
    ('1', '!', 0x12),
    ('2', '@', 0x13),
    ('3', '#', 0x14),
    ('4', '$', 0x15),
    ('5', '%', 0x17),
    ('6', '^', 0x16),
    ('7', '&', 0x1A),
    ('8', '*', 0x1C),
    ('9', '(', 0x19),
    ('0', ')', 0x1D),
    ('-', '_', 0x1B),
    ('=', '+', 0x18),
    ('[', '{', 0x21),
    (']', '}', 0x1E),
    ('\\', '|', 0x2A),
    (';', ':', 0x29),
    ('\'', '"', 0x27),
    (',', '<', 0x2B),
    ('.', '>', 0x2F),
    ('/', '?', 0x2C),
    ('`', '~', 0x32),
];

/// Modifier combinations a layout is probed with, most basic first so that a character
/// reachable without modifiers is never filed under Option.
const PROBE_COMBINATIONS: [Flags; 4] = [
    Flags::NONE,
    Flags::SHIFT,
    Flags::OPTION,
    Flags(Flags::SHIFT.0 | Flags::OPTION.0),
];
/// The last virtual key code of the main block (the backtick key).
const LAST_PROBED_KEYCODE: u16 = 0x32;
/// Return, Tab and Space: keys, not characters.
const NON_TEXT_KEYCODES: [u16; 3] = [0x24, 0x30, KEYCODE_SPACE];
/// A probe that finds fewer characters than this did not read a real layout.
const MIN_PROBED_CHARACTERS: usize = 30;

/// Which key types which character on the current keyboard layout.
#[derive(Clone, Debug)]
pub struct Layout {
    keys: HashMap<char, (u16, Flags)>,
}

impl Layout {
    /// The US QWERTY layout.
    #[must_use]
    pub fn ansi_us() -> Self {
        Self::from_entries(
            ANSI_KEYS
                .iter()
                .flat_map(|&(plain, shifted, keycode)| {
                    [
                        (plain, keycode, Flags::NONE),
                        (shifted, keycode, Flags::SHIFT),
                    ]
                })
                .chain([(' ', KEYCODE_SPACE, Flags::NONE)]),
        )
    }

    /// A layout from `(character, virtual key code, modifiers)`; a later entry replaces an
    /// earlier one for the same character.
    #[must_use]
    pub fn from_entries(entries: impl IntoIterator<Item = (char, u16, Flags)>) -> Self {
        Self {
            keys: entries
                .into_iter()
                .map(|(c, keycode, flags)| (c, (keycode, flags)))
                .collect(),
        }
    }

    /// Reads a layout through `translate`, which gives the character that a virtual key code
    /// types with the given modifiers (only Shift and Option are asked for), if any. Returns
    /// `None` when too few characters came out for it to be a real layout.
    #[must_use]
    pub fn probe(mut translate: impl FnMut(u16, Flags) -> Option<char>) -> Option<Self> {
        let mut keys = HashMap::new();
        for flags in PROBE_COMBINATIONS {
            for keycode in 0..=LAST_PROBED_KEYCODE {
                if NON_TEXT_KEYCODES.contains(&keycode) {
                    continue;
                }
                if let Some(c) = translate(keycode, flags)
                    && !c.is_control()
                {
                    keys.entry(c).or_insert((keycode, flags));
                }
            }
        }
        if keys.len() < MIN_PROBED_CHARACTERS {
            return None;
        }
        keys.insert(' ', (KEYCODE_SPACE, Flags::NONE));
        Some(Self { keys })
    }

    /// The virtual key code and modifiers that type `c`.
    #[must_use]
    pub fn lookup(&self, c: char) -> Option<(u16, Flags)> {
        self.keys.get(&c).copied()
    }
}

/// The key press for a keysym, or `None` when it has no Mac equivalent.
#[must_use]
pub fn resolve(keysym: u32, style: KeyStyle, layout: &Layout) -> Option<Resolved> {
    if let Some((modifier, right)) = modifier_key(keysym, style) {
        return Some(Resolved::Modifier { modifier, right });
    }
    if let Some(keycode) = special_keycode(keysym) {
        return Some(Resolved::Key {
            keycode,
            flags: Flags::NONE,
        });
    }
    let c = keysym_char(keysym)?;
    if c == ' ' {
        return Some(Resolved::Key {
            keycode: KEYCODE_SPACE,
            flags: Flags::NONE,
        });
    }
    Some(match layout.lookup(c) {
        Some((keycode, flags)) => Resolved::Key { keycode, flags },
        None => Resolved::Text(c),
    })
}

/// The keys of the viewer's modifier groups.
#[derive(Clone, Copy)]
enum Group {
    Shift,
    CapsLock,
    Control,
    Alt,
    Super,
}

/// The Mac modifier (and whether it is the right-hand key) for an X11 modifier keysym.
fn modifier_key(keysym: u32, style: KeyStyle) -> Option<(Modifier, bool)> {
    let (group, right) = match keysym {
        0xffe1 => (Group::Shift, false),
        0xffe2 => (Group::Shift, true),
        // Hyper acts as Control.
        0xffe3 | 0xffed => (Group::Control, false),
        0xffe4 | 0xffee => (Group::Control, true),
        0xffe5 => (Group::CapsLock, false),
        // Meta acts as Super.
        0xffe7 | 0xffeb => (Group::Super, false),
        0xffe8 | 0xffec => (Group::Super, true),
        0xffe9 => (Group::Alt, false),
        0xffea => (Group::Alt, true),
        _ => return None,
    };
    let modifier = match (group, style) {
        (Group::Shift, _) => Modifier::Shift,
        (Group::CapsLock, _) => Modifier::CapsLock,
        (Group::Control, KeyStyle::Command) => Modifier::Command,
        (Group::Control, KeyStyle::Literal) => Modifier::Control,
        (Group::Alt, _) => Modifier::Option,
        (Group::Super, KeyStyle::Command) => Modifier::Control,
        (Group::Super, KeyStyle::Literal) => Modifier::Command,
    };
    Some((modifier, right))
}

const KEYSYM_F1: u32 = 0xffbe;
const KEYSYM_KP_0: u32 = 0xffb0;
/// Virtual key codes of F1 to F20.
const FUNCTION_KEYCODES: [u16; 20] = [
    0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F, 0x69, 0x6B, 0x71, 0x6A,
    0x40, 0x4F, 0x50, 0x5A,
];
/// Virtual key codes of the keypad digits 0 to 9.
const KEYPAD_DIGIT_KEYCODES: [u16; 10] =
    [0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5B, 0x5C];

/// The virtual key code of a keysym that is a key, not a character.
fn special_keycode(keysym: u32) -> Option<u16> {
    let keycode = match keysym {
        0xff08 => 0x33, // BackSpace
        0xff09 => 0x30, // Tab
        0xff0d => 0x24, // Return
        0xff1b => 0x35, // Escape
        0xff50 => 0x73, // Home
        0xff51 => 0x7B, // Left
        0xff52 => 0x7E, // Up
        0xff53 => 0x7C, // Right
        0xff54 => 0x7D, // Down
        0xff55 => 0x74, // Page_Up
        0xff56 => 0x79, // Page_Down
        0xff57 => 0x77, // End
        0xff63 => 0x72, // Insert (Help on a Mac)
        0xff8d => 0x4C, // KP_Enter
        0xffaa => 0x43, // KP_Multiply
        0xffab => 0x45, // KP_Add
        0xffad => 0x4E, // KP_Subtract
        0xffae => 0x41, // KP_Decimal
        0xffaf => 0x4B, // KP_Divide
        0xffbd => 0x51, // KP_Equal
        0xffff => 0x75, // Delete (forward delete)
        _ => {
            return table_keycode(&FUNCTION_KEYCODES, KEYSYM_F1, keysym)
                .or_else(|| table_keycode(&KEYPAD_DIGIT_KEYCODES, KEYSYM_KP_0, keysym));
        }
    };
    Some(keycode)
}

/// The entry for `keysym` in a table whose first entry belongs to the keysym `first`.
fn table_keycode(table: &[u16], first: u32, keysym: u32) -> Option<u16> {
    let index = usize::try_from(keysym.checked_sub(first)?).ok()?;
    table.get(index).copied()
}

/// Keysyms for code points above Latin-1 are the code point with this bit set.
const UNICODE_FLAG: u32 = 0x0100_0000;
const UNICODE_FLAG_MASK: u32 = 0xff00_0000;

/// The character a keysym stands for: Latin-1 as it is, or a Unicode keysym.
/// `None` for control characters and everything that is not a character.
fn keysym_char(keysym: u32) -> Option<char> {
    let code = match keysym {
        0x20..=0x7e | 0xa0..=0xff => keysym,
        _ if keysym & UNICODE_FLAG_MASK == UNICODE_FLAG => keysym & !UNICODE_FLAG_MASK,
        _ => return None,
    };
    char::from_u32(code).filter(|c| !c.is_control())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const SHIFT_L: u32 = 0xffe1;
    const SHIFT_R: u32 = 0xffe2;
    const CONTROL_L: u32 = 0xffe3;
    const CONTROL_R: u32 = 0xffe4;
    const CAPS_LOCK: u32 = 0xffe5;
    const META_L: u32 = 0xffe7;
    const META_R: u32 = 0xffe8;
    const ALT_L: u32 = 0xffe9;
    const ALT_R: u32 = 0xffea;
    const SUPER_L: u32 = 0xffeb;
    const SUPER_R: u32 = 0xffec;
    const HYPER_L: u32 = 0xffed;
    const HYPER_R: u32 = 0xffee;

    fn resolved(keysym: u32, style: KeyStyle) -> Option<Resolved> {
        resolve(keysym, style, &Layout::ansi_us())
    }

    fn modifier(modifier: Modifier, right: bool) -> Option<Resolved> {
        Some(Resolved::Modifier { modifier, right })
    }

    fn key(keycode: u16, flags: Flags) -> Option<Resolved> {
        Some(Resolved::Key { keycode, flags })
    }

    #[test]
    fn command_style_maps_control_to_command_and_super_to_control() {
        let style = KeyStyle::Command;
        assert_eq!(resolved(SHIFT_L, style), modifier(Modifier::Shift, false));
        assert_eq!(resolved(SHIFT_R, style), modifier(Modifier::Shift, true));
        assert_eq!(
            resolved(CONTROL_L, style),
            modifier(Modifier::Command, false)
        );
        assert_eq!(
            resolved(CONTROL_R, style),
            modifier(Modifier::Command, true)
        );
        assert_eq!(resolved(HYPER_L, style), modifier(Modifier::Command, false));
        assert_eq!(resolved(HYPER_R, style), modifier(Modifier::Command, true));
        assert_eq!(resolved(ALT_L, style), modifier(Modifier::Option, false));
        assert_eq!(resolved(ALT_R, style), modifier(Modifier::Option, true));
        assert_eq!(resolved(SUPER_L, style), modifier(Modifier::Control, false));
        assert_eq!(resolved(SUPER_R, style), modifier(Modifier::Control, true));
        assert_eq!(resolved(META_L, style), modifier(Modifier::Control, false));
        assert_eq!(resolved(META_R, style), modifier(Modifier::Control, true));
        assert_eq!(
            resolved(CAPS_LOCK, style),
            modifier(Modifier::CapsLock, false)
        );
    }

    #[test]
    fn literal_style_keeps_control_and_maps_super_to_command() {
        let style = KeyStyle::Literal;
        assert_eq!(resolved(SHIFT_L, style), modifier(Modifier::Shift, false));
        assert_eq!(resolved(SHIFT_R, style), modifier(Modifier::Shift, true));
        assert_eq!(
            resolved(CONTROL_L, style),
            modifier(Modifier::Control, false)
        );
        assert_eq!(
            resolved(CONTROL_R, style),
            modifier(Modifier::Control, true)
        );
        assert_eq!(resolved(HYPER_L, style), modifier(Modifier::Control, false));
        assert_eq!(resolved(HYPER_R, style), modifier(Modifier::Control, true));
        assert_eq!(resolved(ALT_L, style), modifier(Modifier::Option, false));
        assert_eq!(resolved(ALT_R, style), modifier(Modifier::Option, true));
        assert_eq!(resolved(SUPER_L, style), modifier(Modifier::Command, false));
        assert_eq!(resolved(SUPER_R, style), modifier(Modifier::Command, true));
        assert_eq!(resolved(META_L, style), modifier(Modifier::Command, false));
        assert_eq!(resolved(META_R, style), modifier(Modifier::Command, true));
        assert_eq!(
            resolved(CAPS_LOCK, style),
            modifier(Modifier::CapsLock, false)
        );
    }

    #[test]
    fn modifier_key_codes_and_flags() {
        assert_eq!(Modifier::Shift.keycode(false), 0x38);
        assert_eq!(Modifier::Shift.keycode(true), 0x3C);
        assert_eq!(Modifier::Control.keycode(false), 0x3B);
        assert_eq!(Modifier::Control.keycode(true), 0x3E);
        assert_eq!(Modifier::Option.keycode(false), 0x3A);
        assert_eq!(Modifier::Option.keycode(true), 0x3D);
        assert_eq!(Modifier::Command.keycode(false), 0x37);
        assert_eq!(Modifier::Command.keycode(true), 0x36);
        assert_eq!(Modifier::CapsLock.keycode(false), 0x39);
        assert_eq!(Modifier::Shift.flag(), Flags::SHIFT);
        assert_eq!(Modifier::Command.flag(), Flags::COMMAND);
        assert_eq!(Modifier::CapsLock.flag(), Flags::NONE);
    }

    #[test]
    fn flags_combine() {
        let both = Flags::SHIFT | Flags::OPTION;
        assert!(both.contains(Flags::SHIFT));
        assert!(both.contains(Flags::OPTION));
        assert!(!both.contains(Flags::COMMAND));
        assert!(both.intersects(Flags::OPTION | Flags::COMMAND));
        assert!(!both.intersects(Flags::CONTROL | Flags::COMMAND));
        let mut flags = Flags::NONE;
        flags |= Flags::CONTROL;
        assert_eq!(flags, Flags::CONTROL);
    }

    #[test]
    fn key_style_parses_its_names() {
        assert_eq!(KeyStyle::parse("command"), Some(KeyStyle::Command));
        assert_eq!(KeyStyle::parse("literal"), Some(KeyStyle::Literal));
        assert_eq!(KeyStyle::parse("Command"), None);
        assert_eq!(KeyStyle::parse(""), None);
        assert_eq!(KeyStyle::default(), KeyStyle::Command);
    }

    #[test]
    fn named_keys_use_mac_key_codes() {
        let style = KeyStyle::Command;
        assert_eq!(resolved(0xff08, style), key(0x33, Flags::NONE));
        assert_eq!(resolved(0xff09, style), key(0x30, Flags::NONE));
        assert_eq!(resolved(0xff0d, style), key(0x24, Flags::NONE));
        assert_eq!(resolved(0xff8d, style), key(0x4C, Flags::NONE));
        assert_eq!(resolved(0xff1b, style), key(0x35, Flags::NONE));
        assert_eq!(resolved(0xffff, style), key(0x75, Flags::NONE));
        assert_eq!(resolved(0xff50, style), key(0x73, Flags::NONE));
        assert_eq!(resolved(0xff51, style), key(0x7B, Flags::NONE));
        assert_eq!(resolved(0xff52, style), key(0x7E, Flags::NONE));
        assert_eq!(resolved(0xff53, style), key(0x7C, Flags::NONE));
        assert_eq!(resolved(0xff54, style), key(0x7D, Flags::NONE));
        assert_eq!(resolved(0xff55, style), key(0x74, Flags::NONE));
        assert_eq!(resolved(0xff56, style), key(0x79, Flags::NONE));
        assert_eq!(resolved(0xff57, style), key(0x77, Flags::NONE));
        assert_eq!(resolved(0xff63, style), key(0x72, Flags::NONE));
        assert_eq!(resolved(0x20, style), key(0x31, Flags::NONE));
    }

    #[test]
    fn function_keys_f1_to_f20() {
        let style = KeyStyle::Command;
        assert_eq!(resolved(0xffbe, style), key(0x7A, Flags::NONE));
        assert_eq!(resolved(0xffc0, style), key(0x63, Flags::NONE));
        assert_eq!(resolved(0xffc9, style), key(0x6F, Flags::NONE));
        assert_eq!(resolved(0xffca, style), key(0x69, Flags::NONE));
        assert_eq!(resolved(0xffcb, style), key(0x6B, Flags::NONE));
        assert_eq!(resolved(0xffd1, style), key(0x5A, Flags::NONE));
        assert_eq!(resolved(0xffd2, style), None);
    }

    #[test]
    fn keypad_keys() {
        let style = KeyStyle::Command;
        assert_eq!(resolved(0xffb0, style), key(0x52, Flags::NONE));
        assert_eq!(resolved(0xffb7, style), key(0x59, Flags::NONE));
        assert_eq!(resolved(0xffb8, style), key(0x5B, Flags::NONE));
        assert_eq!(resolved(0xffb9, style), key(0x5C, Flags::NONE));
        assert_eq!(resolved(0xffaa, style), key(0x43, Flags::NONE));
        assert_eq!(resolved(0xffab, style), key(0x45, Flags::NONE));
        assert_eq!(resolved(0xffad, style), key(0x4E, Flags::NONE));
        assert_eq!(resolved(0xffae, style), key(0x41, Flags::NONE));
        assert_eq!(resolved(0xffaf, style), key(0x4B, Flags::NONE));
        assert_eq!(resolved(0xffbd, style), key(0x51, Flags::NONE));
    }

    #[test]
    fn characters_come_with_the_shift_they_need() {
        let style = KeyStyle::Command;
        assert_eq!(resolved(0x61, style), key(0x00, Flags::NONE));
        assert_eq!(resolved(0x41, style), key(0x00, Flags::SHIFT));
        assert_eq!(resolved(0x31, style), key(0x12, Flags::NONE));
        assert_eq!(resolved(0x21, style), key(0x12, Flags::SHIFT));
        assert_eq!(resolved(0x2f, style), key(0x2C, Flags::NONE));
        assert_eq!(resolved(0x3f, style), key(0x2C, Flags::SHIFT));
        assert_eq!(resolved(0x60, style), key(0x32, Flags::NONE));
        assert_eq!(resolved(0x7e, style), key(0x32, Flags::SHIFT));
    }

    #[test]
    fn unicode_keysyms_use_the_layout() {
        let layout = Layout::from_entries([('\u{e9}', 0x28, Flags::OPTION)]);
        // Latin-1 and the Unicode keysym of the same character agree.
        assert_eq!(
            resolve(0xe9, KeyStyle::Command, &layout),
            key(0x28, Flags::OPTION)
        );
        assert_eq!(
            resolve(0x0100_00e9, KeyStyle::Command, &layout),
            key(0x28, Flags::OPTION)
        );
    }

    #[test]
    fn characters_without_a_key_are_typed_as_text() {
        let style = KeyStyle::Command;
        assert_eq!(resolved(0xe9, style), Some(Resolved::Text('\u{e9}')));
        assert_eq!(
            resolved(0x0100_20ac, style),
            Some(Resolved::Text('\u{20ac}'))
        );
        assert_eq!(
            resolved(0x0101_f600, style),
            Some(Resolved::Text('\u{1f600}'))
        );
    }

    #[test]
    fn a_later_entry_replaces_an_earlier_one() {
        let layout = Layout::from_entries([
            ('a', 0x00, Flags::NONE),
            ('a', 0x0B, Flags::OPTION),
            ('b', 0x0B, Flags::NONE),
        ]);
        assert_eq!(layout.lookup('a'), Some((0x0B, Flags::OPTION)));
        assert_eq!(layout.lookup('b'), Some((0x0B, Flags::NONE)));
        assert_eq!(layout.lookup('c'), None);
    }

    #[test]
    fn keysyms_without_a_mac_key_resolve_to_nothing() {
        let style = KeyStyle::Command;
        // Menu, Print, Pause, Scroll_Lock, Num_Lock, Begin, Shift_Lock, back, forward.
        for keysym in [
            0xff67,
            0xff61,
            0xff13,
            0xff14,
            0xff7f,
            0xff58,
            0xffe6,
            0x1008_ff26,
            0x1008_ff27,
        ] {
            assert_eq!(resolved(keysym, style), None, "{keysym:#x}");
        }
        // Control characters, unassigned values and invalid code points.
        for keysym in [
            0x0,
            0x1f,
            0x7f,
            0x80,
            0x9f,
            0x0100_0007,
            0x0100_007f,
            0x0100_d800,
            0x0111_0000,
            0xffff_ffff,
        ] {
            assert_eq!(resolved(keysym, style), None, "{keysym:#x}");
        }
    }

    #[test]
    fn ansi_us_types_every_printable_ascii_character() {
        let layout = Layout::ansi_us();
        for code in 0x20_u8..=0x7e {
            let c = char::from(code);
            assert!(layout.lookup(c).is_some(), "no key for {c:?}");
        }
        assert_eq!(layout.lookup(' '), Some((0x31, Flags::NONE)));
    }

    #[test]
    fn ansi_us_has_no_two_characters_on_the_same_key_press() {
        let layout = Layout::ansi_us();
        let mut seen = HashSet::new();
        for code in 0x20_u8..=0x7e {
            let (keycode, flags) = layout.lookup(char::from(code)).unwrap();
            assert!(
                seen.insert((keycode, flags.0)),
                "{code:#x} repeats a key press"
            );
            assert!(flags == Flags::NONE || flags == Flags::SHIFT);
            assert!(keycode <= 0x32);
        }
        assert_eq!(seen.len(), 95);
    }

    #[test]
    fn ansi_us_key_codes_match_the_mac_keyboard() {
        let layout = Layout::ansi_us();
        for (c, keycode) in [
            ('a', 0x00),
            ('s', 0x01),
            ('z', 0x06),
            ('b', 0x0B),
            ('q', 0x0C),
            ('1', 0x12),
            ('5', 0x17),
            ('6', 0x16),
            ('=', 0x18),
            ('9', 0x19),
            ('7', 0x1A),
            ('-', 0x1B),
            ('8', 0x1C),
            ('0', 0x1D),
            (']', 0x1E),
            ('o', 0x1F),
            ('[', 0x21),
            ('p', 0x23),
            ('l', 0x25),
            ('\'', 0x27),
            (';', 0x29),
            ('\\', 0x2A),
            (',', 0x2B),
            ('/', 0x2C),
            ('n', 0x2D),
            ('.', 0x2F),
            ('`', 0x32),
        ] {
            assert_eq!(layout.lookup(c), Some((keycode, Flags::NONE)), "{c:?}");
        }
        for (c, base) in [('_', '-'), ('+', '='), ('{', '['), ('}', ']'), ('|', '\\')] {
            let (keycode, _) = layout.lookup(base).unwrap();
            assert_eq!(layout.lookup(c), Some((keycode, Flags::SHIFT)), "{c:?}");
        }
    }

    /// A made-up layout of 30 keys: letters on the plain keys, capitals with Shift and, with
    /// Option, the letter of the *next* key, so the same letter is also reachable with Option
    /// from an earlier key.
    fn invented_layout(keycode: u16, flags: Flags) -> Option<char> {
        let index = u8::try_from(keycode).ok().filter(|&index| index < 30)?;
        if flags == Flags::NONE {
            Some(char::from(b'a' + index % 26))
        } else if flags == Flags::SHIFT {
            Some(char::from(b'A' + index % 26))
        } else {
            Some(char::from(b'a' + (index + 1) % 26))
        }
    }

    #[test]
    fn probing_prefers_plain_keys_over_modified_ones() {
        let layout = Layout::probe(invented_layout).unwrap();
        // 'b' is plain on key 0x01 and Option on key 0x00: the plain key wins.
        assert_eq!(layout.lookup('b'), Some((0x01, Flags::NONE)));
        // 'a' is plain on keys 0x00 and 0x1a: the lowest one wins.
        assert_eq!(layout.lookup('a'), Some((0x00, Flags::NONE)));
        assert_eq!(layout.lookup('A'), Some((0x00, Flags::SHIFT)));
        assert_eq!(layout.lookup(' '), Some((0x31, Flags::NONE)));
    }

    #[test]
    fn probing_skips_return_tab_and_control_characters() {
        let layout = Layout::probe(|keycode, flags| {
            if matches!(keycode, 0x24 | 0x30 | 0x31) {
                return Some('x');
            }
            if keycode == 0x32 {
                return Some('\u{3}');
            }
            invented_layout(keycode, flags)
        })
        .unwrap();
        assert_eq!(layout.lookup('x'), Some((0x17, Flags::NONE)));
        assert_eq!(layout.lookup('\u{3}'), None);
    }

    #[test]
    fn a_probe_that_finds_too_little_is_not_a_layout() {
        assert!(Layout::probe(|_, _| None).is_none());
        assert!(Layout::probe(|keycode, _| (keycode < 10).then_some('x')).is_none());
        let few = Layout::probe(|keycode, flags| {
            if flags == Flags::NONE && keycode < 20 {
                Some(char::from(b'a' + u8::try_from(keycode).unwrap_or(0)))
            } else {
                None
            }
        });
        assert!(few.is_none());
    }
}
