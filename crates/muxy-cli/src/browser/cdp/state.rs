//! The tabs an agent opened, and the note the desktop app reads to show that
//! an agent is at work. Both live in one small file in the profile folder, so
//! separate `atur-term browser` commands share them.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use muxy_app_core::browser::BrowserEngine;
use muxy_app_core::browser::agent::state_path;
use serde::{Deserialize, Serialize};

const LOCK_FILE: &str = "helium-agent.lock";

/// What the file holds. `engine`, `endpoint`, `active_at_ms` and `tabs` are
/// read by the desktop app.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct State {
    pub(super) engine: String,
    #[serde(default)]
    pub(super) endpoint: String,
    #[serde(default)]
    pub(super) active_at_ms: u64,
    /// The tabs agents opened and may drive.
    #[serde(default)]
    pub(super) tabs: Vec<String>,
}

impl State {
    pub(super) fn owns(&self, tab: &str) -> bool {
        self.tabs.iter().any(|owned| owned == tab)
    }

    /// Forgets tabs the browser no longer has.
    pub(super) fn keep_only(&mut self, live: &[String]) {
        self.tabs.retain(|tab| live.contains(tab));
    }

    pub(super) fn add(&mut self, tab: &str) {
        if !self.owns(tab) {
            self.tabs.push(tab.to_owned());
        }
    }

    pub(super) fn remove(&mut self, tab: &str) {
        self.tabs.retain(|owned| owned != tab);
    }
}

#[derive(Debug)]
pub(super) struct Store {
    directory: PathBuf,
}

impl Store {
    pub(super) fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_owned(),
        }
    }

    /// The saved state, or an empty one if there is none or it is unreadable.
    pub(super) fn load(&self) -> State {
        fs::read_to_string(state_path(&self.directory))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Changes the state while holding the lock, and stamps it as active now
    /// at `endpoint`. A different browser than last time starts with no
    /// tabs.
    pub(super) fn update(
        &self,
        engine: BrowserEngine,
        endpoint: &str,
        change: impl FnOnce(&mut State),
    ) -> io::Result<State> {
        let lock = self.lock()?;
        let mut state = self.load();
        if state.endpoint != endpoint || state.engine != engine.id() {
            state.tabs.clear();
        }
        engine.id().clone_into(&mut state.engine);
        endpoint.clone_into(&mut state.endpoint);
        change(&mut state);
        state.active_at_ms = now_ms();
        self.save(&state)?;
        drop(lock);
        Ok(state)
    }

    fn lock(&self) -> io::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.directory.join(LOCK_FILE))?;
        file.lock()?;
        Ok(file)
    }

    fn save(&self, state: &State) -> io::Result<()> {
        let path = state_path(&self.directory);
        let temporary = path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(serde_json::to_string(state)?.as_bytes())?;
        file.flush()?;
        fs::rename(temporary, path)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_are_remembered_between_commands() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::new(directory.path());
        store.update(BrowserEngine::Helium, "127.0.0.1:9222", |state| {
            state.add("A");
        })?;
        let state = store.update(BrowserEngine::Helium, "127.0.0.1:9222", |state| {
            state.add("B");
            state.add("A");
        })?;
        assert_eq!(state.tabs, ["A", "B"]);
        assert!(store.load().owns("B"));
        assert!(store.load().active_at_ms > 0);
        Ok(())
    }

    #[test]
    fn a_different_browser_starts_clean() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::new(directory.path());
        store.update(BrowserEngine::Helium, "127.0.0.1:9222", |state| {
            state.add("A");
        })?;
        let state = store.update(BrowserEngine::Helium, "127.0.0.1:9333", |_| {})?;
        assert!(state.tabs.is_empty());
        let state = store.update(BrowserEngine::Chrome, "127.0.0.1:9333", |state| {
            state.add("C");
        })?;
        assert_eq!(
            (state.engine.as_str(), state.tabs.as_slice()),
            ("chrome", ["C".to_owned()].as_slice())
        );
        Ok(())
    }

    #[test]
    fn closed_and_vanished_tabs_are_forgotten() {
        let mut state = State::default();
        for tab in ["A", "B", "C"] {
            state.add(tab);
        }
        state.remove("A");
        state.keep_only(&["C".to_owned()]);
        assert_eq!(state.tabs, ["C"]);
    }

    #[test]
    fn the_note_is_what_the_app_reads() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::new(directory.path());
        store.update(BrowserEngine::Helium, "127.0.0.1:9222", |state| {
            state.add("A");
        })?;
        let text = fs::read_to_string(state_path(directory.path()))?;
        let driving = muxy_app_core::browser::agent::active(&text, now_ms());
        assert_eq!(
            driving.map(|driving| driving.tabs),
            Some(vec!["A".to_owned()])
        );
        Ok(())
    }

    #[test]
    fn unreadable_state_is_empty() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(state_path(directory.path()), "not json")?;
        assert_eq!(Store::new(directory.path()).load(), State::default());
        Ok(())
    }
}
