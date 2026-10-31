//! Which attached client sizes a session.
//!
//! Every attached client tells the server the grid its view has. Only the client
//! whose keyboard the session follows may make the session that big: the one that
//! typed last, once the previous typist has been quiet for `HANDOVER`. A viewer that
//! never typed only watches; its size is kept for the moment it starts typing.
//!
//! Until somebody types, clients that are not watching keep the earlier behaviour:
//! the last size told wins. Phones attach as watchers (`join` with `watching`).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use muxy_protocol::Size;

use super::AttachmentId;

/// How long the typist must be quiet before another client's typing takes the
/// session's size. Two people typing at once do not make the pane flip between sizes.
pub(super) const HANDOVER: Duration = Duration::from_millis(800);

#[derive(Debug)]
struct Viewer {
    /// The last grid this client told the server about, if any.
    size: Option<Size>,
    /// Set until the client types: it never sizes the session before.
    watching: bool,
}

#[derive(Debug, Default)]
pub(super) struct Sizing {
    viewers: HashMap<AttachmentId, Viewer>,
    /// The client the keyboard follows, and when it last typed.
    keyboard: Option<(AttachmentId, Instant)>,
}

impl Sizing {
    pub(super) fn join(&mut self, id: AttachmentId, watching: bool, size: Option<Size>) {
        self.viewers.insert(id, Viewer { size, watching });
    }

    pub(super) fn leave(&mut self, id: AttachmentId) {
        self.viewers.remove(&id);
        if self.keyboard.is_some_and(|(owner, _)| owner == id) {
            self.keyboard = None;
        }
    }

    /// Forgets the viewers `keep` does not name, such as clients whose connection ended.
    pub(super) fn retain(&mut self, keep: &HashSet<AttachmentId>) {
        let gone: Vec<AttachmentId> = self
            .viewers
            .keys()
            .filter(|id| !keep.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.leave(id);
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.viewers.is_empty()
    }

    /// Whether the client is a watcher that has not typed. A client this does not
    /// know is not one.
    pub(super) fn watching(&self, id: AttachmentId) -> bool {
        self.viewers.get(&id).is_some_and(|viewer| viewer.watching)
    }

    /// Records the grid a client reports. True when the session must take that size now.
    pub(super) fn told(&mut self, id: AttachmentId, size: Size) -> bool {
        let Some(viewer) = self.viewers.get_mut(&id) else {
            return true;
        };
        viewer.size = Some(size);
        match self.keyboard {
            Some((owner, _)) => owner == id,
            None => !viewer.watching,
        }
    }

    /// A client typed. Returns the size the session must take when this moved the
    /// keyboard to a client that had told one.
    pub(super) fn typed(&mut self, id: AttachmentId, now: Instant) -> Option<Size> {
        let viewer = self.viewers.get_mut(&id)?;
        match self.keyboard {
            Some((owner, _)) if owner == id => {
                self.keyboard = Some((id, now));
                None
            }
            Some((_, at)) if now.saturating_duration_since(at) < HANDOVER => None,
            _ => {
                viewer.watching = false;
                self.keyboard = Some((id, now));
                viewer.size
            }
        }
    }

    /// Whether some client may size the session: the keyboard's owner, or before
    /// anyone typed a client that is not watching. When none may, the terminal's
    /// own host keeps its size.
    pub(super) fn sizes(&self) -> bool {
        self.keyboard.is_some() || self.viewers.values().any(|viewer| !viewer.watching)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOP: AttachmentId = AttachmentId(1);
    const TUI: AttachmentId = AttachmentId(2);
    const PHONE: AttachmentId = AttachmentId(3);

    const fn size(cols: u16, rows: u16) -> Size {
        Size { cols, rows }
    }

    fn room() -> Sizing {
        let mut sizing = Sizing::default();
        sizing.join(DESKTOP, false, Some(size(120, 40)));
        sizing.join(TUI, false, Some(size(90, 30)));
        sizing.join(PHONE, true, Some(size(40, 20)));
        sizing
    }

    #[test]
    fn a_watcher_never_sizes_before_it_types() {
        let mut sizing = room();
        assert!(!sizing.told(PHONE, size(41, 20)));
        assert!(sizing.told(DESKTOP, size(121, 40)));
        assert!(sizing.told(TUI, size(91, 30)));
    }

    #[test]
    fn the_keyboard_owner_alone_sizes_the_session() {
        let mut sizing = room();
        let start = Instant::now();
        assert_eq!(sizing.typed(DESKTOP, start), Some(size(120, 40)));
        assert!(sizing.told(DESKTOP, size(100, 30)));
        assert!(!sizing.told(TUI, size(80, 24)));
        assert!(!sizing.told(PHONE, size(30, 10)));
        assert_eq!(sizing.typed(DESKTOP, start), None);
    }

    #[test]
    fn typing_hands_over_the_keyboard_with_the_size_the_typist_told() {
        let mut sizing = room();
        let start = Instant::now();
        sizing.typed(DESKTOP, start);
        assert!(!sizing.told(TUI, size(80, 24)));
        assert_eq!(sizing.typed(TUI, start + HANDOVER), Some(size(80, 24)));
        assert!(sizing.told(TUI, size(81, 24)));
        assert!(!sizing.told(DESKTOP, size(100, 30)));
        let later = start + HANDOVER * 3;
        assert_eq!(sizing.typed(PHONE, later), Some(size(40, 20)));
        assert!(!sizing.watching(PHONE));
        assert!(sizing.told(PHONE, size(41, 20)));
    }

    #[test]
    fn typing_at_once_does_not_flip_the_size() {
        let mut sizing = room();
        let start = Instant::now();
        sizing.typed(DESKTOP, start);
        let soon = start + HANDOVER / 2;
        assert_eq!(sizing.typed(PHONE, soon), None);
        assert!(sizing.watching(PHONE));
        assert!(sizing.told(DESKTOP, size(100, 30)));
        // The owner's own typing keeps the keyboard fresh.
        sizing.typed(DESKTOP, soon);
        assert_eq!(sizing.typed(TUI, soon + HANDOVER / 2), None);
        assert_eq!(
            sizing.typed(TUI, soon + HANDOVER),
            Some(size(90, 30)),
            "the typist has been quiet for the whole handover"
        );
    }

    #[test]
    fn a_client_that_never_told_a_size_takes_the_keyboard_without_resizing() {
        let mut sizing = Sizing::default();
        sizing.join(PHONE, true, None);
        assert_eq!(sizing.typed(PHONE, Instant::now()), None);
        assert!(sizing.sizes());
    }

    #[test]
    fn the_session_is_released_when_only_watchers_are_left() {
        let mut sizing = room();
        sizing.typed(TUI, Instant::now());
        assert!(sizing.sizes());
        sizing.leave(TUI);
        assert!(sizing.sizes(), "the desktop has not typed but may size");
        sizing.leave(DESKTOP);
        assert!(!sizing.sizes(), "the phone only watches");
        assert!(!sizing.told(PHONE, size(30, 10)));
        sizing.typed(PHONE, Instant::now());
        assert!(sizing.sizes());
        sizing.leave(PHONE);
        assert!(sizing.is_empty());
        assert!(!sizing.sizes());
    }

    #[test]
    fn a_departed_owner_leaves_the_session_to_the_clients_that_are_not_watching() {
        let mut sizing = room();
        sizing.typed(PHONE, Instant::now());
        assert!(!sizing.told(DESKTOP, size(100, 30)));
        sizing.leave(PHONE);
        assert!(sizing.told(DESKTOP, size(100, 30)));
        assert!(sizing.told(TUI, size(80, 24)));
    }

    #[test]
    fn retain_forgets_clients_that_are_gone() {
        let mut sizing = room();
        sizing.typed(PHONE, Instant::now());
        sizing.retain(&HashSet::from([DESKTOP, TUI]));
        assert!(sizing.told(DESKTOP, size(100, 30)));
        assert!(!sizing.watching(PHONE));
        assert!(!sizing.viewers.contains_key(&PHONE));
    }
}
