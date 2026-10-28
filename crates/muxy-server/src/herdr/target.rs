//! Which herdr terminal a session drives. herdr gives its terminals new ids when
//! it hands them to a new server, so the id is followed through the pane that
//! holds the terminal.

use std::sync::{Mutex, MutexGuard, PoisonError};

use super::panes::PaneState;

#[derive(Debug)]
pub(super) struct Target {
    terminal: Mutex<String>,
    pane: Mutex<Option<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Target {
    pub(super) fn new(terminal: String, pane: Option<String>) -> Self {
        Self {
            terminal: Mutex::new(terminal),
            pane: Mutex::new(pane),
        }
    }

    pub(super) fn terminal(&self) -> String {
        lock(&self.terminal).clone()
    }

    pub(super) fn pane(&self) -> Option<String> {
        lock(&self.pane).clone()
    }

    /// Finds the terminal in herdr's pane list, by id or else by the pane that
    /// held it, and follows a changed id. `None` when it is no longer there.
    pub(super) fn locate<'a>(&self, panes: &'a [PaneState]) -> Option<&'a PaneState> {
        let terminal = self.terminal();
        let hint = self.pane();
        let found = panes
            .iter()
            .find(|pane| pane.terminal == terminal)
            .or_else(|| {
                let hint = hint.as_deref()?;
                panes.iter().find(|pane| pane.pane == hint)
            })?;
        *lock(&self.pane) = Some(found.pane.clone());
        if found.terminal != terminal {
            log::info!("herdr terminal {terminal} is now {}", found.terminal);
            found.terminal.clone_into(&mut lock(&self.terminal));
        }
        Some(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(pane: &str, terminal: &str) -> PaneState {
        PaneState {
            pane: pane.into(),
            terminal: terminal.into(),
            ..PaneState::default()
        }
    }

    #[test]
    fn a_terminal_is_followed_through_its_pane_when_herdr_renumbers_it() {
        let target = Target::new("term_a".into(), None);
        assert!(target.locate(&[pane("w1:p2", "term_a")]).is_some());
        let after = [pane("w1:p1", "term_x"), pane("w1:p2", "term_b")];
        assert_eq!(
            target.locate(&after).map(|pane| pane.pane.as_str()),
            Some("w1:p2")
        );
        assert_eq!(target.terminal(), "term_b");
        assert!(target.locate(&[pane("w1:p1", "term_x")]).is_none());
    }
}
