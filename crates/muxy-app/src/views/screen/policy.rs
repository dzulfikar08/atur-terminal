//! When the Screen view is offered, and what may cross the clipboard.

/// The most clipboard text sent in either direction.
pub(super) const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// Whether a server can be viewed: it is another computer's, the app is connected
/// to it, and it can share its screen. This computer's own screen is never offered.
pub(crate) fn offered(remote: bool, connected: bool, supported: bool) -> bool {
    remote && connected && supported
}

/// Whether `text` is worth copying to the other computer's clipboard.
pub(super) fn clipboard_allowed(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_connected_remote_server_that_shares_its_screen_is_offered() {
        for remote in [false, true] {
            for connected in [false, true] {
                for supported in [false, true] {
                    assert_eq!(
                        offered(remote, connected, supported),
                        remote && connected && supported,
                        "{remote} {connected} {supported}"
                    );
                }
            }
        }
        assert!(offered(true, true, true));
        assert!(
            !offered(false, true, true),
            "this computer is never offered"
        );
    }

    #[test]
    fn the_clipboard_carries_nonempty_text_up_to_a_mebibyte() {
        assert!(!clipboard_allowed(""));
        assert!(clipboard_allowed("a"));
        assert!(clipboard_allowed(&"a".repeat(MAX_CLIPBOARD_BYTES)));
        assert!(!clipboard_allowed(&"a".repeat(MAX_CLIPBOARD_BYTES + 1)));
    }

    #[test]
    fn the_limit_counts_bytes_not_characters() {
        let wide = "é".repeat(MAX_CLIPBOARD_BYTES / 2 + 1);
        assert!(wide.chars().count() < MAX_CLIPBOARD_BYTES);
        assert!(!clipboard_allowed(&wide));
    }
}
