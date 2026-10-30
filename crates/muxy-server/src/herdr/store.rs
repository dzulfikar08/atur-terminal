//! Which herdr terminals and workspaces belong to this server's sessions and projects.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use muxy_protocol::{ProjectId, SessionId};
use serde::{Deserialize, Serialize};

use crate::session::SavedMark;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Entry {
    pub(super) terminal: String,
    /// The pane that holds the terminal. herdr keeps pane ids when it hands its
    /// terminals to a new server and re-mints terminal ids, so this is the
    /// durable key. Files from older builds have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) pane: Option<String>,
    pub(super) project: ProjectId,
    /// The prompt marks the session owner found, so a restart keeps them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) marks: Vec<SavedMark>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Data {
    version: u32,
    /// Session id, as a decimal string, to its terminal.
    sessions: BTreeMap<String, Entry>,
    workspaces: BTreeMap<ProjectId, String>,
}

#[derive(Debug)]
pub(super) struct Store {
    path: PathBuf,
    data: Mutex<Data>,
}

impl Store {
    /// Opens `<state>/herdr.json`; a missing file is an empty store.
    pub(super) fn open(state: &Path) -> io::Result<Self> {
        let path = state.join("herdr.json");
        let data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Data::default(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            data: Mutex::new(data),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A failed save keeps the in-memory state; the next change retries it.
    fn save(&self, data: &mut Data) {
        data.version = 1;
        let temporary = self.path.with_extension("json.tmp");
        let result = serde_json::to_vec_pretty(data)
            .map_err(io::Error::other)
            .and_then(|bytes| fs::write(&temporary, bytes))
            .and_then(|()| fs::rename(&temporary, &self.path));
        if let Err(error) = result {
            log::error!("herdr state was not saved: {error}");
        }
    }

    pub(super) fn sessions(&self) -> Vec<(SessionId, Entry)> {
        self.lock()
            .sessions
            .iter()
            .filter_map(|(id, entry)| Some((SessionId::new(id.parse().ok()?)?, entry.clone())))
            .collect()
    }

    pub(super) fn set_session(
        &self,
        session: SessionId,
        project: ProjectId,
        terminal: &str,
        pane: Option<&str>,
    ) {
        let mut data = self.lock();
        let key = session.get().to_string();
        // Adoption rewrites the ids of a session that keeps its marks.
        let marks = data
            .sessions
            .get(&key)
            .map(|entry| entry.marks.clone())
            .unwrap_or_default();
        data.sessions.insert(
            key,
            Entry {
                terminal: terminal.to_owned(),
                pane: pane.map(str::to_owned),
                project,
                marks,
            },
        );
        self.save(&mut data);
    }

    pub(super) fn marks(&self, session: SessionId) -> Vec<SavedMark> {
        self.lock()
            .sessions
            .get(&session.get().to_string())
            .map(|entry| entry.marks.clone())
            .unwrap_or_default()
    }

    /// Stores the marks of a known session; an unknown one has no entry to hold them.
    pub(super) fn set_marks(&self, session: SessionId, marks: &[SavedMark]) {
        let mut data = self.lock();
        let Some(entry) = data.sessions.get_mut(&session.get().to_string()) else {
            return;
        };
        if entry.marks != marks {
            entry.marks = marks.to_vec();
            self.save(&mut data);
        }
    }

    pub(super) fn remove_session(&self, session: SessionId) {
        let mut data = self.lock();
        if data.sessions.remove(&session.get().to_string()).is_some() {
            self.save(&mut data);
        }
    }

    pub(super) fn workspace(&self, project: ProjectId) -> Option<String> {
        self.lock().workspaces.get(&project).cloned()
    }

    pub(super) fn set_workspace(&self, project: ProjectId, workspace: &str) {
        let mut data = self.lock();
        data.workspaces.insert(project, workspace.to_owned());
        self.save(&mut data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mappings_survive_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "atur-herdr-store-{}",
            getrandom::u64().unwrap_or(1)
        ));
        fs::create_dir_all(&dir).expect("directory");
        let session = SessionId::new(7).expect("id");
        let project = ProjectId::from_u128(9);
        let store = Store::open(&dir).expect("open");
        store.set_session(session, project, "term_a", Some("w3:p1"));
        store.set_workspace(project, "w3");
        let marks = vec![SavedMark::new(41, "fix the login", "fix the l", 1)];
        store.set_marks(session, &marks);
        // New ids keep the marks of the session.
        store.set_session(session, project, "term_a", Some("w3:p1"));
        drop(store);
        let store = Store::open(&dir).expect("reopen");
        assert_eq!(
            store.sessions(),
            vec![(
                session,
                Entry {
                    terminal: "term_a".into(),
                    pane: Some("w3:p1".into()),
                    project,
                    marks: marks.clone(),
                }
            )]
        );
        assert_eq!(store.workspace(project).as_deref(), Some("w3"));
        store.remove_session(session);
        assert!(Store::open(&dir).expect("reopen").sessions().is_empty());
    }

    #[test]
    fn a_file_from_a_build_that_saved_no_pane_is_still_read() {
        let dir = std::env::temp_dir().join(format!(
            "atur-herdr-store-{}",
            getrandom::u64().unwrap_or(1)
        ));
        fs::create_dir_all(&dir).expect("directory");
        let project = ProjectId::from_u128(9);
        fs::write(
            dir.join("herdr.json"),
            format!(r#"{{"version":1,"sessions":{{"7":{{"terminal":"term_a","project":"{project}"}}}},"workspaces":{{}}}}"#),
        )
        .expect("write");
        let store = Store::open(&dir).expect("open");
        let sessions = store.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].1.pane, None);
        assert_eq!(sessions[0].1.project, project);
    }
}
