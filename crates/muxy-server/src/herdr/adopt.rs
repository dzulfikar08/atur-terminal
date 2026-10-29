//! Taking back the sessions herdr kept alive.
//!
//! Tabs made by this server carry no label of their own, so herdr numbers them like
//! the user's. A session is found again by what survives a restart of this server,
//! of herdr, or a live handoff of herdr to a new binary. herdr re-mints terminal ids
//! on a handoff and keeps pane ids and processes, so in this order:
//!
//! 1. the saved terminal id, then the saved pane id (`herdr.json`);
//! 2. the shell's own `ATUR_TERM_SESSION_ID` (`MUXY_SESSION_ID` before the rename),
//!    read from `/proc` of the pane's shell: needs no saved state and also finds
//!    sessions after `herdr.json` is deleted;
//! 3. the tab label `atur-<id>` or `muxy-<id>` that earlier builds gave their tabs.
//!
//! A pane that a hint points at but whose shell names another session, or none,
//! is somebody else's terminal and is not adopted: ids are reused after a restart,
//! and ending a session closes its pane.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use muxy_protocol::{ProjectId, SessionId, Size};
use serde_json::{Value, json};

use super::api::Api;
use super::store::Entry;
use super::{Herdr, HerdrBacking};

/// Whose terminal a pane holds, from the environment its shell started with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Owner {
    Session(SessionId),
    /// The environment is readable and names no session: not one of ours.
    Foreign,
    /// The environment could not be read (another host or OS, a vanished process).
    Unknown,
}

/// The variables that hold the session id: the current name, then the one builds
/// before the rename set.
const SESSION_VARIABLES: [&[u8]; 2] = [b"ATUR_TERM_SESSION_ID=", b"MUXY_SESSION_ID="];

fn owner_of_environment(environment: &[u8]) -> Owner {
    if environment.is_empty() {
        return Owner::Unknown;
    }
    environment
        .split(|byte| *byte == 0)
        .find_map(|entry| {
            let value = SESSION_VARIABLES
                .iter()
                .find_map(|name| entry.strip_prefix(*name))?;
            SessionId::new(std::str::from_utf8(value).ok()?.parse().ok()?)
        })
        .map_or(Owner::Foreign, Owner::Session)
}

#[cfg(target_os = "linux")]
fn owner_of_process(pid: u32) -> Owner {
    std::fs::read(format!("/proc/{pid}/environ")).map_or(Owner::Unknown, |environment| {
        owner_of_environment(&environment)
    })
}

#[cfg(not(target_os = "linux"))]
fn owner_of_process(_pid: u32) -> Owner {
    Owner::Unknown
}

/// The labels earlier builds gave a session's tab.
fn legacy_tab_labels(session: SessionId) -> [String; 2] {
    let id = session.get() >> 32;
    [format!("atur-{id:08x}"), format!("muxy-{id:08x}")]
}

/// Reads each pane's owner once.
struct Owners<'a> {
    api: &'a Api,
    seen: BTreeMap<String, Owner>,
}

impl Owners<'_> {
    fn of(&mut self, pane: &str) -> Owner {
        if let Some(owner) = self.seen.get(pane) {
            return *owner;
        }
        let owner = self
            .api
            .call("pane.process_info", &json!({"pane_id": pane}))
            .ok()
            .and_then(|info| u32::try_from(info["process_info"]["shell_pid"].as_u64()?).ok())
            .map_or(Owner::Unknown, owner_of_process);
        self.seen.insert(pane.to_owned(), owner);
        owner
    }
}

/// herdr's panes and tabs, and which pane each session has taken.
struct Adoption<'a> {
    panes: &'a [Value],
    tabs: &'a [Value],
    owners: Owners<'a>,
    claimed: BTreeSet<&'a str>,
}

impl<'a> Adoption<'a> {
    /// Whether `pane` may hold `session`. A label names its session exactly, so a
    /// pane under one is taken even when its shell's environment names no session.
    fn fits(&mut self, session: SessionId, pane: &'a Value, by_label: bool) -> bool {
        let Some(id) = pane["pane_id"].as_str() else {
            return false;
        };
        if self.claimed.contains(id) {
            return false;
        }
        match self.owners.of(id) {
            Owner::Session(owner) => owner == session,
            Owner::Foreign => by_label,
            Owner::Unknown => true,
        }
    }

    fn claim(&mut self, pane: &'a Value) -> &'a Value {
        if let Some(id) = pane["pane_id"].as_str() {
            self.claimed.insert(id);
        }
        pane
    }

    fn find(&mut self, session: SessionId, saved: Option<&Entry>) -> Option<&'a Value> {
        let panes = self.panes;
        let hinted = saved.into_iter().flat_map(move |saved| {
            let by_terminal = panes
                .iter()
                .find(|pane| pane["terminal_id"].as_str() == Some(saved.terminal.as_str()));
            let by_pane = saved.pane.as_deref().and_then(|id| {
                panes
                    .iter()
                    .find(|pane| pane["pane_id"].as_str() == Some(id))
            });
            [by_terminal, by_pane]
        });
        for pane in hinted.flatten() {
            if self.fits(session, pane, false) {
                return Some(self.claim(pane));
            }
        }
        for pane in panes {
            let Some(id) = pane["pane_id"].as_str() else {
                continue;
            };
            if !self.claimed.contains(id) && self.owners.of(id) == Owner::Session(session) {
                return Some(self.claim(pane));
            }
        }
        let labels = legacy_tab_labels(session);
        let tabs = self.tabs;
        for tab in tabs {
            let Some(tab_id) = tab["tab_id"].as_str() else {
                continue;
            };
            if !tab["label"]
                .as_str()
                .is_some_and(|label| labels.iter().any(|candidate| candidate == label))
            {
                continue;
            }
            for pane in panes
                .iter()
                .filter(|pane| pane["tab_id"].as_str() == Some(tab_id))
            {
                if self.fits(session, pane, true) {
                    return Some(self.claim(pane));
                }
            }
        }
        None
    }
}

impl Herdr {
    /// The sessions that herdr still runs, with their size. `known` are the
    /// sessions the catalog remembers with their projects; `herdr.json` is a hint
    /// that can be missing or stale, see the module documentation.
    pub(crate) fn survivors(
        self: &Arc<Self>,
        known: &[(SessionId, ProjectId)],
    ) -> Vec<(SessionId, HerdrBacking)> {
        let panes = match self.api.call("pane.list", &json!({})) {
            Ok(list) => list["panes"].as_array().cloned().unwrap_or_default(),
            Err(error) => {
                log::warn!("herdr sessions were not adopted: {error}");
                return Vec::new();
            }
        };
        let tabs = self
            .api
            .call("tab.list", &json!({}))
            .ok()
            .and_then(|list| list["tabs"].as_array().cloned())
            .unwrap_or_default();
        let mut adoption = Adoption {
            panes: &panes,
            tabs: &tabs,
            owners: Owners {
                api: &self.api,
                seen: BTreeMap::new(),
            },
            claimed: BTreeSet::new(),
        };
        let mut candidates: BTreeMap<SessionId, (ProjectId, Option<Entry>)> = known
            .iter()
            .map(|(session, project)| (*session, (*project, None)))
            .collect();
        for (session, entry) in self.store.sessions() {
            candidates.insert(session, (entry.project, Some(entry)));
        }
        let mut found = Vec::new();
        for (session, (project, saved)) in candidates {
            let Some(pane) = adoption.find(session, saved.as_ref()) else {
                continue;
            };
            let (Some(terminal), Some(pane_id)) =
                (pane["terminal_id"].as_str(), pane["pane_id"].as_str())
            else {
                continue;
            };
            let size = self.pane_size(pane_id).unwrap_or(Size {
                cols: 120,
                rows: 40,
            });
            let unchanged = saved.as_ref().is_some_and(|saved| {
                saved.terminal == terminal && saved.pane.as_deref() == Some(pane_id)
            });
            if !unchanged {
                self.store
                    .set_session(session, project, terminal, Some(pane_id));
            }
            if self.store.workspace(project).is_none()
                && let Some(workspace) = pane["workspace_id"].as_str()
            {
                self.store.set_workspace(project, workspace);
            }
            found.push((
                session,
                HerdrBacking::new(
                    Arc::clone(self),
                    terminal.to_owned(),
                    Some(pane_id.to_owned()),
                    size,
                ),
            ));
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::HerdrConfig;
    use crate::herdr::fake::Fake;

    fn session(id: u64) -> SessionId {
        SessionId::new(id).expect("id")
    }

    fn environment(entries: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in entries {
            bytes.extend_from_slice(entry.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn the_session_comes_from_the_current_or_the_pre_rename_variable() {
        let id = session(77);
        assert_eq!(
            owner_of_environment(&environment(&["A=1", "ATUR_TERM_SESSION_ID=77"])),
            Owner::Session(id)
        );
        assert_eq!(
            owner_of_environment(&environment(&["MUXY_SESSION_ID=77"])),
            Owner::Session(id)
        );
        assert_eq!(
            owner_of_environment(&environment(&["HOME=/root", "ATUR_TERM_SESSION_ID=x"])),
            Owner::Foreign
        );
        assert_eq!(owner_of_environment(b""), Owner::Unknown);
    }

    #[test]
    fn legacy_labels_carry_the_top_half_of_the_id() {
        assert_eq!(
            legacy_tab_labels(session(0xabcd_ef01_2345_6789)),
            ["atur-abcdef01", "muxy-abcdef01"]
        );
    }

    struct Setup {
        fake: Fake,
        state: std::path::PathBuf,
        project: ProjectId,
    }

    impl Setup {
        fn new() -> Self {
            let fake = Fake::start();
            let state = fake.directory().join("state");
            std::fs::create_dir_all(&state).expect("state");
            Self {
                fake,
                state,
                project: ProjectId::from_u128(5),
            }
        }

        /// What a server start builds: the saved mapping is read anew.
        fn herdr(&self) -> Arc<Herdr> {
            Herdr::open(
                &HerdrConfig {
                    bin: None,
                    socket: Some(self.fake.socket().to_path_buf()),
                },
                &self.state,
            )
            .expect("open")
        }

        fn adopted(&self, sessions: &[SessionId]) -> BTreeMap<SessionId, String> {
            let known: Vec<_> = sessions.iter().map(|id| (*id, self.project)).collect();
            self.herdr()
                .survivors(&known)
                .into_iter()
                .map(|(id, backing)| (id, backing.target.pane().expect("pane")))
                .collect()
        }
    }

    #[test]
    fn tabs_labelled_by_earlier_builds_are_still_adopted_without_readable_environment() {
        let setup = Setup::new();
        let first = session(0x1111_2222_0000_0001);
        let second = session(0x3333_4444_0000_0002);
        setup.fake.add_tab("w1", Some("muxy-11112222"), None);
        setup.fake.add_tab("w1", Some("atur-33334444"), None);
        setup.fake.add_tab("w1", Some("muxy-55556666"), None);
        let adopted = setup.adopted(&[second, first]);
        assert_eq!(adopted.len(), 2);
        assert_eq!(adopted[&first], "w1:p1");
        assert_eq!(adopted[&second], "w1:p2");
        // Adoption leaves every tab's label as it is.
        assert_eq!(
            setup.fake.labels(),
            ["muxy-11112222", "atur-33334444", "muxy-55556666"]
        );
        // And the pane ids are saved for the next start.
        let saved = setup.herdr().store.sessions();
        assert_eq!(saved.len(), 2);
        assert!(saved.iter().all(|(_, entry)| entry.pane.is_some()));
    }

    #[test]
    fn two_sessions_never_take_the_same_pane() {
        let setup = Setup::new();
        setup.fake.add_tab("w1", Some("muxy-00000001"), None);
        let a = session(0x0000_0001_0000_0001);
        let b = session(0x0000_0001_0000_0002);
        // Both ids carry the label's top half, and only one pane exists.
        assert_eq!(setup.adopted(&[a, b]).len(), 1);
    }

    #[test]
    fn a_session_without_a_tab_is_not_adopted() {
        let setup = Setup::new();
        setup.fake.add_tab("w1", None, None);
        assert!(setup.adopted(&[session(1)]).is_empty());
    }

    /// Adoption by the environment reads `/proc`.
    #[cfg(target_os = "linux")]
    mod environment {
        use super::*;
        use crate::herdr::fake::Shell;

        fn unlabelled(setup: &Setup, sessions: &[u64]) -> Vec<Shell> {
            let shells: Vec<_> = sessions
                .iter()
                .map(|id| {
                    let id = id.to_string();
                    Shell::spawn(&[("ATUR_TERM_SESSION_ID", id.as_str())])
                })
                .collect();
            for shell in &shells {
                setup.fake.add_tab("w1", None, Some(shell));
            }
            shells
        }

        #[test]
        fn unlabelled_tabs_are_found_by_the_environment_without_saved_state() {
            let setup = Setup::new();
            let _shells = unlabelled(&setup, &[10, 11, 12]);
            let user = Shell::spawn(&[("HOME", "/x")]);
            setup.fake.add_tab("w1", Some("notes"), Some(&user));
            // The order of the tabs is not the order the sessions are asked for.
            let adopted = setup.adopted(&[session(12), session(10), session(11)]);
            assert_eq!(adopted.len(), 3);
            assert_eq!(adopted[&session(10)], "w1:p1");
            assert_eq!(adopted[&session(11)], "w1:p2");
            assert_eq!(adopted[&session(12)], "w1:p3");
            assert_eq!(setup.fake.labels(), ["1", "2", "3", "notes"]);
        }

        #[test]
        fn a_shell_of_the_pre_rename_variable_is_found_without_a_label() {
            let setup = Setup::new();
            let shell = Shell::spawn(&[("MUXY_SESSION_ID", "99")]);
            setup.fake.add_tab("w1", None, Some(&shell));
            assert_eq!(setup.adopted(&[session(99)])[&session(99)], "w1:p1");
        }

        #[test]
        fn a_restart_adopts_by_the_saved_ids_and_a_handoff_keeps_the_pane() {
            let setup = Setup::new();
            let _shells = unlabelled(&setup, &[20, 21]);
            let known = [(session(20), setup.project), (session(21), setup.project)];
            // Adopted once, so the mapping is saved with pane ids.
            assert_eq!(setup.herdr().survivors(&known).len(), 2);
            let before = setup.herdr().store.sessions();
            assert_eq!(before.len(), 2);
            assert!(before.iter().all(|(_, entry)| entry.pane.is_some()));
            // A restart of this server alone: the terminal ids still match.
            let adopted = setup.adopted(&[session(20), session(21)]);
            assert_eq!(adopted[&session(20)], "w1:p1");
            assert_eq!(adopted[&session(21)], "w1:p2");
            // herdr hands over: terminal ids are new, panes and processes are the same.
            setup.fake.hand_off();
            let adopted = setup.adopted(&[session(20), session(21)]);
            assert_eq!(adopted[&session(20)], "w1:p1");
            assert_eq!(adopted[&session(21)], "w1:p2");
            let after = setup.herdr().store.sessions();
            assert_eq!(after.len(), 2);
            for ((_, before), (_, after)) in before.iter().zip(&after) {
                assert_ne!(before.terminal, after.terminal);
                assert_eq!(before.pane, after.pane);
            }
        }

        #[test]
        fn a_handoff_is_survived_with_the_saved_state_deleted_too() {
            let setup = Setup::new();
            let _shells = unlabelled(&setup, &[30, 31]);
            setup.fake.hand_off();
            let adopted = setup.adopted(&[session(31), session(30)]);
            assert_eq!(adopted[&session(30)], "w1:p1");
            assert_eq!(adopted[&session(31)], "w1:p2");
        }

        #[test]
        fn saved_ids_that_another_session_now_holds_are_not_taken() {
            let setup = Setup::new();
            let _shells = unlabelled(&setup, &[40, 41]);
            let known = [(session(40), setup.project), (session(41), setup.project)];
            assert_eq!(setup.herdr().survivors(&known).len(), 2);
            // herdr numbers anew after a restart, in another order.
            setup.fake.swap_processes("w1:p1", "w1:p2");
            let adopted = setup.adopted(&[session(40), session(41)]);
            assert_eq!(adopted[&session(40)], "w1:p2");
            assert_eq!(adopted[&session(41)], "w1:p1");
        }

        #[test]
        fn a_saved_pane_that_a_users_terminal_now_holds_is_not_adopted() {
            let setup = Setup::new();
            let _shells = unlabelled(&setup, &[50]);
            let known = [(session(50), setup.project)];
            assert_eq!(setup.herdr().survivors(&known).len(), 1);
            // The session's terminal ended; the user opened another under the same ids.
            setup.fake.clear();
            let user = Shell::spawn(&[("HOME", "/x")]);
            setup.fake.add_tab("w1", None, Some(&user));
            assert!(setup.adopted(&[session(50)]).is_empty());
        }
    }
}
