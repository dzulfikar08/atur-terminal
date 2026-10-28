//! A short-lived copy of herdr's pane list, so the owners of many sessions share
//! one request instead of each polling for their own title and scrollback size.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::api::Api;

const TTL: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct PaneState {
    pub(super) pane: String,
    pub(super) terminal: String,
    /// The title the application set with an escape sequence.
    pub(super) title: Option<String>,
    /// Rows of scrollback above the screen.
    pub(super) history_rows: usize,
    pub(super) viewport_rows: usize,
    /// herdr's own reading of which agent runs there and what it is doing.
    pub(super) agent: Option<String>,
    pub(super) agent_status: Option<String>,
}

impl PaneState {
    fn parse(pane: &Value) -> Option<Self> {
        let count = |value: &Value| {
            value
                .as_u64()
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(0)
        };
        let text = |value: &Value| {
            value
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        Some(Self {
            pane: pane["pane_id"].as_str()?.to_owned(),
            terminal: pane["terminal_id"].as_str()?.to_owned(),
            title: text(&pane["terminal_title_stripped"]).or_else(|| text(&pane["terminal_title"])),
            history_rows: count(&pane["scroll"]["max_offset_from_bottom"]),
            viewport_rows: count(&pane["scroll"]["viewport_rows"]),
            agent: text(&pane["agent"]),
            agent_status: text(&pane["agent_status"]),
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct Panes {
    cache: Mutex<Option<(Instant, Vec<PaneState>)>>,
}

impl Panes {
    /// The state of `terminal`; `None` when herdr no longer lists it or does not answer.
    /// A terminal missing from a cached list may be new, so the list is read once more.
    pub(super) fn get(&self, api: &Api, terminal: &str) -> Option<PaneState> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut fresh = false;
        loop {
            if cache.as_ref().is_none_or(|(at, _)| at.elapsed() > TTL) {
                let list = api.call("pane.list", &json!({})).ok()?;
                let panes = list["panes"]
                    .as_array()?
                    .iter()
                    .filter_map(PaneState::parse)
                    .collect();
                *cache = Some((Instant::now(), panes));
                fresh = true;
            }
            let found = cache
                .as_ref()?
                .1
                .iter()
                .find(|pane| pane.terminal == terminal)
                .cloned();
            if found.is_some() || fresh {
                return found;
            }
            *cache = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_scrollback_title_and_agent_from_a_pane() {
        let pane = json!({
            "pane_id": "w1:p2", "terminal_id": "term_a", "agent": "claude", "agent_status": "working",
            "terminal_title": "raw", "terminal_title_stripped": "clean",
            "scroll": {"offset_from_bottom": 0, "max_offset_from_bottom": 262, "viewport_rows": 40}
        });
        let state = PaneState::parse(&pane).expect("pane");
        assert_eq!(state.pane, "w1:p2");
        assert_eq!(state.title.as_deref(), Some("clean"));
        assert_eq!((state.history_rows, state.viewport_rows), (262, 40));
        assert_eq!(state.agent.as_deref(), Some("claude"));
        assert!(PaneState::parse(&json!({"pane_id": "w1:p1"})).is_none());
    }
}
