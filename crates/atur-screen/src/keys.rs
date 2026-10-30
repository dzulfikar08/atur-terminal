//! X11 keysyms for GPUI keystrokes.

/// Left Shift.
pub const SHIFT_L: u32 = 0xffe1;
/// Left Control.
pub const CONTROL_L: u32 = 0xffe3;
/// Left Alt.
pub const ALT_L: u32 = 0xffe9;
/// Left Super (Windows / Command key).
pub const SUPER_L: u32 = 0xffeb;

/// Keysyms for code points above Latin-1 are the code point with this bit set.
const UNICODE_FLAG: u32 = 0x0100_0000;
const KEYSYM_F1: u32 = 0xffbe;
const MAX_FUNCTION_KEY: u32 = 35;

/// X11 keysym for a GPUI keystroke: `key` is GPUI's key name (`"a"`, `"enter"`,
/// `"escape"`, `"tab"`, `"backspace"`, `"up"`, `"f5"`, `"space"`, ...), `key_char`
/// the typed text if any, `shift` whether Shift is held. `None` when there is no mapping.
pub fn keysym(key: &str, key_char: Option<&str>, shift: bool) -> Option<u32> {
    if let Some(sym) = named_key(key) {
        return Some(sym);
    }
    if let Some(sym) = key_char.and_then(single_char).and_then(char_keysym) {
        return Some(sym);
    }
    let c = single_char(key)?;
    let c = if shift { shifted(c) } else { c };
    char_keysym(c)
}

fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(first)
}

fn shifted(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

fn char_keysym(c: char) -> Option<u32> {
    let code = u32::from(c);
    match code {
        0x20..=0x7e | 0xa0..=0xff => Some(code),
        0x7f => Some(0xffff),
        _ if c.is_control() => None,
        _ => Some(UNICODE_FLAG | code),
    }
}

fn named_key(key: &str) -> Option<u32> {
    let sym = match key {
        "space" => 0x0020,
        "backspace" => 0xff08,
        "tab" => 0xff09,
        "enter" => 0xff0d,
        "pause" => 0xff13,
        "scroll_lock" | "scrolllock" => 0xff14,
        "escape" => 0xff1b,
        "home" => 0xff50,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "pageup" | "prior" => 0xff55,
        "pagedown" | "next" => 0xff56,
        "end" => 0xff57,
        "begin" => 0xff58,
        "print" | "printscreen" => 0xff61,
        "insert" => 0xff63,
        "menu" => 0xff67,
        "num_lock" | "numlock" => 0xff7f,
        "multiply" => 0xffaa,
        "add" => 0xffab,
        "separator" => 0xffac,
        "subtract" => 0xffad,
        "decimal" => 0xffae,
        "divide" => 0xffaf,
        "equal" => 0xffbd,
        "capslock" | "caps_lock" => 0xffe5,
        "delete" => 0xffff,
        "back" => 0x1008_ff26,
        "forward" => 0x1008_ff27,
        _ => return function_key(key),
    };
    Some(sym)
}

fn function_key(key: &str) -> Option<u32> {
    let number: u32 = key.strip_prefix('f')?.parse().ok()?;
    if (1..=MAX_FUNCTION_KEY).contains(&number) {
        Some(KEYSYM_F1 + number - 1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_follow_key_char_then_shift() {
        assert_eq!(keysym("a", Some("a"), false), Some(0x61));
        assert_eq!(keysym("a", Some("A"), true), Some(0x41));
        assert_eq!(keysym("a", None, false), Some(0x61));
        assert_eq!(keysym("a", None, true), Some(0x41));
    }

    #[test]
    fn digits_and_punctuation() {
        assert_eq!(keysym("1", Some("1"), false), Some(0x31));
        assert_eq!(keysym("1", Some("!"), true), Some(0x21));
        assert_eq!(keysym(";", Some(";"), false), Some(0x3b));
        assert_eq!(keysym("/", None, false), Some(0x2f));
        assert_eq!(keysym("`", Some("`"), false), Some(0x60));
    }

    #[test]
    fn latin1_and_unicode() {
        assert_eq!(keysym("e", Some("\u{e9}"), false), Some(0xe9));
        assert_eq!(keysym("s", Some("\u{df}"), false), Some(0xdf));
        assert_eq!(keysym("q", Some("\u{20ac}"), false), Some(0x0100_20ac));
        assert_eq!(keysym("q", Some("\u{0e46}"), false), Some(0x0100_0e46));
        assert_eq!(keysym("\u{20ac}", None, false), Some(0x0100_20ac));
        assert_eq!(keysym("a", Some("\u{1f600}"), false), Some(0x0101_f600));
    }

    #[test]
    fn named_keys() {
        assert_eq!(keysym("enter", Some("\n"), false), Some(0xff0d));
        assert_eq!(keysym("escape", None, false), Some(0xff1b));
        assert_eq!(keysym("tab", Some("\t"), false), Some(0xff09));
        assert_eq!(keysym("backspace", None, false), Some(0xff08));
        assert_eq!(keysym("delete", None, false), Some(0xffff));
        assert_eq!(keysym("insert", None, false), Some(0xff63));
        assert_eq!(keysym("home", None, false), Some(0xff50));
        assert_eq!(keysym("end", None, false), Some(0xff57));
        assert_eq!(keysym("pageup", None, false), Some(0xff55));
        assert_eq!(keysym("pagedown", None, false), Some(0xff56));
        assert_eq!(keysym("left", None, false), Some(0xff51));
        assert_eq!(keysym("up", None, false), Some(0xff52));
        assert_eq!(keysym("right", None, false), Some(0xff53));
        assert_eq!(keysym("down", None, false), Some(0xff54));
        assert_eq!(keysym("space", Some(" "), false), Some(0x20));
        assert_eq!(keysym("capslock", None, false), Some(0xffe5));
    }

    #[test]
    fn function_keys() {
        assert_eq!(keysym("f1", None, false), Some(0xffbe));
        assert_eq!(keysym("f5", None, false), Some(0xffc2));
        assert_eq!(keysym("f12", None, false), Some(0xffc9));
        assert_eq!(keysym("f24", None, false), Some(0xffd5));
        assert_eq!(keysym("f35", None, false), Some(0xffe0));
        assert_eq!(keysym("f36", None, false), None);
        assert_eq!(keysym("f0", None, false), None);
        assert_eq!(keysym("f", None, false), Some(0x66));
    }

    #[test]
    fn numpad_names() {
        assert_eq!(keysym("add", None, false), Some(0xffab));
        assert_eq!(keysym("subtract", None, false), Some(0xffad));
        assert_eq!(keysym("multiply", None, false), Some(0xffaa));
        assert_eq!(keysym("divide", None, false), Some(0xffaf));
        assert_eq!(keysym("decimal", None, false), Some(0xffae));
    }

    #[test]
    fn unknown_keys_have_no_mapping() {
        assert_eq!(keysym("nonsense", None, false), None);
        assert_eq!(keysym("", None, false), None);
        assert_eq!(keysym("ab", Some("ab"), false), None);
    }

    #[test]
    fn control_characters_fall_back_to_the_key_name() {
        assert_eq!(keysym("c", Some("\u{3}"), false), Some(0x63));
        assert_eq!(keysym("x", None, true), Some(0x58));
    }
}
