//! The note `atur-term browser` leaves while it drives an external browser, so
//! the app can show that an agent is at work there.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::BrowserEngine;

/// The file, inside the app's data directory, the CLI rewrites on every command.
pub const STATE_FILE: &str = "helium-agent.json";

/// How long after its last command an agent still counts as driving.
pub const ACTIVE_WINDOW_MS: u64 = 30_000;

pub fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// An agent that recently ran a command in an external browser.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct AgentDriving {
    pub engine: BrowserEngine,
    #[serde(default)]
    pub endpoint: String,
    pub active_at_ms: u64,
    /// The debugging target IDs of the tabs the agent has used.
    #[serde(default)]
    pub tabs: Vec<String>,
}

/// The agent `json` describes, if it ran a command within the last
/// [`ACTIVE_WINDOW_MS`] of `now_ms` and drives an external browser.
pub fn active(json: &str, now_ms: u64) -> Option<AgentDriving> {
    let driving: AgentDriving = serde_json::from_str(json).ok()?;
    (driving.engine.is_external() && now_ms.abs_diff(driving.active_at_ms) < ACTIVE_WINDOW_MS)
        .then_some(driving)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn note(engine: &str, at: u64) -> String {
        format!(
            r#"{{"engine":"{engine}","endpoint":"127.0.0.1:9333","active_at_ms":{at},"tabs":["A1","B2"]}}"#
        )
    }

    #[test]
    fn a_recent_command_is_active() {
        let driving = active(&note("helium", NOW - 1_000), NOW).expect("active");
        assert_eq!(driving.engine, BrowserEngine::Helium);
        assert_eq!(driving.endpoint, "127.0.0.1:9333");
        assert_eq!(driving.active_at_ms, NOW - 1_000);
        assert_eq!(driving.tabs, ["A1", "B2"]);
    }

    #[test]
    fn the_window_ends_after_thirty_seconds() {
        assert!(active(&note("chrome", NOW - 29_999), NOW).is_some());
        assert!(active(&note("chrome", NOW - 30_000), NOW).is_none());
        assert!(active(&note("chrome", NOW - 600_000), NOW).is_none());
    }

    #[test]
    fn a_clock_a_little_ahead_still_counts() {
        assert!(active(&note("chromium", NOW + 2_000), NOW).is_some());
        assert!(active(&note("chromium", NOW + 60_000), NOW).is_none());
    }

    #[test]
    fn malformed_notes_are_inactive() {
        for json in [
            "",
            "not json",
            "{",
            "[]",
            "null",
            r#"{"engine":"helium","active_at_ms":"soon"}"#,
            r#"{"engine":"helium","active_at_ms":-5}"#,
            r#"{"engine":"netscape","active_at_ms":1700000000000}"#,
        ] {
            assert!(active(json, NOW).is_none(), "{json}");
        }
    }

    #[test]
    fn notes_missing_the_engine_or_time_are_inactive() {
        assert!(active(r#"{"active_at_ms":1700000000000}"#, NOW).is_none());
        assert!(active(r#"{"engine":"helium"}"#, NOW).is_none());
    }

    #[test]
    fn endpoint_and_tabs_are_optional() {
        let json = format!(r#"{{"engine":"helium","active_at_ms":{NOW}}}"#);
        let driving = active(&json, NOW).expect("active");
        assert!(driving.endpoint.is_empty());
        assert!(driving.tabs.is_empty());
    }

    #[test]
    fn the_built_in_browser_is_not_an_external_agent() {
        assert!(active(&note("builtin", NOW), NOW).is_none());
    }

    #[test]
    fn the_note_lives_in_the_data_directory() {
        assert_eq!(
            state_path(Path::new("/data")),
            Path::new("/data/helium-agent.json")
        );
    }
}
