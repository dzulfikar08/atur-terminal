use std::time::{Duration, Instant};

/// A session saves this long after its last change, so a finished burst of
/// output reaches disk promptly.
const QUIET: Duration = Duration::from_secs(2);
/// Output that never pauses, such as an agent redrawing a spinner while it
/// works, is saved this long after the first unsaved change.
const MAX_DELAY: Duration = Duration::from_secs(30);
/// A checkpoint walks and serializes every retained row on the session thread,
/// so its cost grows with the history. Saves are spaced to keep that thread at
/// most 1% busy with them.
const DUTY_CYCLE: u32 = 100;

/// When the owner of a running session next saves its screen and history.
#[derive(Debug)]
pub(super) struct Checkpoint {
    dirty_since: Option<Instant>,
    due: Option<Instant>,
    max_delay: Duration,
}

impl Checkpoint {
    /// A session saves once after starting, so a quiet one is listed with its
    /// size.
    pub(super) fn started(now: Instant) -> Self {
        let mut checkpoint = Self::idle();
        checkpoint.changed(now);
        checkpoint
    }

    pub(super) fn idle() -> Self {
        Self {
            dirty_since: None,
            due: None,
            max_delay: MAX_DELAY,
        }
    }

    pub(super) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Output or a resize changed what a save would contain.
    pub(super) fn changed(&mut self, now: Instant) {
        let since = *self.dirty_since.get_or_insert(now);
        self.due = Some((now + QUIET).min(since + self.max_delay));
    }

    /// The saved content is stale or invalid right now, such as after clearing
    /// the screen.
    pub(super) fn immediately(&mut self, now: Instant) {
        self.dirty_since.get_or_insert(now);
        self.due = Some(now);
    }

    /// A save that took `cost` of the session thread's time was submitted.
    pub(super) fn saved(&mut self, cost: Duration) {
        self.dirty_since = None;
        self.due = None;
        self.max_delay = MAX_DELAY.max(cost.saturating_mul(DUTY_CYCLE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn a_session_saves_once_after_starting_even_if_it_stays_silent() {
        let start = Instant::now();
        assert_eq!(Checkpoint::idle().due(), None);
        assert_eq!(Checkpoint::started(start).due(), Some(start + QUIET));
    }

    #[test]
    fn a_burst_is_saved_when_output_goes_quiet() {
        let start = Instant::now();
        let mut checkpoint = Checkpoint::idle();
        checkpoint.changed(start);
        assert_eq!(checkpoint.due(), Some(start + QUIET));
        checkpoint.changed(after(start, 700));
        assert_eq!(checkpoint.due(), Some(after(start, 700) + QUIET));
        checkpoint.changed(after(start, 1200));
        assert_eq!(checkpoint.due(), Some(after(start, 1200) + QUIET));
    }

    #[test]
    fn output_that_never_pauses_is_saved_once_per_max_delay() {
        let start = Instant::now();
        let mut checkpoint = Checkpoint::idle();
        let mut saves = Vec::new();
        for step in 0..=1200 {
            let now = after(start, step * 100);
            checkpoint.changed(now);
            if checkpoint.due().is_some_and(|due| due <= now) {
                saves.push(step * 100);
                checkpoint.saved(Duration::from_millis(200));
            }
        }
        assert_eq!(saves, [30_000, 60_100, 90_200]);
    }

    #[test]
    fn the_first_change_after_a_save_starts_a_new_window() {
        let start = Instant::now();
        let mut checkpoint = Checkpoint::idle();
        checkpoint.changed(start);
        checkpoint.saved(Duration::ZERO);
        assert_eq!(checkpoint.due(), None);
        let later = start + Duration::from_secs(3600);
        checkpoint.changed(later);
        assert_eq!(checkpoint.due(), Some(later + QUIET));
        checkpoint.changed(later + MAX_DELAY);
        assert_eq!(checkpoint.due(), Some(later + MAX_DELAY));
    }

    #[test]
    fn clearing_the_screen_saves_at_once_and_keeps_the_window() {
        let start = Instant::now();
        let mut checkpoint = Checkpoint::idle();
        checkpoint.changed(start);
        checkpoint.immediately(after(start, 5));
        assert_eq!(checkpoint.due(), Some(after(start, 5)));
        checkpoint.saved(Duration::ZERO);
        assert_eq!(checkpoint.due(), None);
        checkpoint.immediately(after(start, 9));
        assert_eq!(checkpoint.due(), Some(after(start, 9)));
    }

    #[test]
    fn expensive_saves_stretch_the_window_to_a_one_percent_duty_cycle() {
        let start = Instant::now();
        let mut checkpoint = Checkpoint::idle();
        checkpoint.saved(Duration::from_millis(500));
        checkpoint.changed(start);
        checkpoint.changed(after(start, 49_000));
        assert_eq!(checkpoint.due(), Some(start + Duration::from_secs(50)));
        checkpoint.saved(Duration::from_millis(500));
        checkpoint.saved(Duration::from_millis(5));
        checkpoint.changed(start);
        checkpoint.changed(after(start, 29_000));
        assert_eq!(checkpoint.due(), Some(start + MAX_DELAY));
    }
}
