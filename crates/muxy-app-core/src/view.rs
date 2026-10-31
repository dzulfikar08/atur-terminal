//! What one window shows, kept apart from the layout every window shares.
//!
//! The layout is the projects, tabs, panes, splits, tab groups, and workspaces
//! of a profile. The view is a window's own take on it: the project and tab
//! it shows, the focused pane, which panes are zoomed, which tab each group
//! selects, and where the window sits. Two windows on one layout never change
//! each other's view, and moving focus or zooming never rewrites the layout.
//!
//! The split follows the one in herdr-app (`src/persist.rs`: a shared
//! document plus a private per-screen `View`); this is a reimplementation of
//! the idea and shares no code with it.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use crate::servers::ServerState;
use crate::{
    AppError, AppState, GroupId, Pane, PaneId, Project, ServerId, TabGroups, TabId, WindowState,
    Workspace,
};

/// The view layout this build writes.
pub(crate) const VERSION: u32 = 1;

/// The window of a profile that has only one.
pub const MAIN_WINDOW: &str = "main";

const MAX_NAME: usize = 64;

/// One window's saved view of the layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct View {
    pub(crate) version: u32,
    pub(crate) window: WindowState,
    /// The zoomed pane of each tab this window zooms.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) zoomed: BTreeMap<TabId, PaneId>,
    /// The tab each tab group shows in this window.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) group_selected: BTreeMap<GroupId, TabId>,
}

/// The layout file: everything of an [`AppState`] but the window, with no tab
/// zoomed and each group showing its first tab. It writes the fields of the
/// state under the same names, which is what loading reads.
struct Shared<'a> {
    servers: &'a BTreeMap<ServerId, ServerState>,
    starting_directories: &'a BTreeMap<PaneId, muxy_protocol::ServerPath>,
    startup_commands: &'a BTreeMap<PaneId, String>,
    quick_terminal: Option<&'a Pane>,
    version: u32,
    projects: Cow<'a, [Project]>,
    workspaces: &'a [Workspace],
}

impl Serialize for Shared<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("servers", self.servers)?;
        map.serialize_entry("starting_directories", self.starting_directories)?;
        if !self.startup_commands.is_empty() {
            map.serialize_entry("startup_commands", self.startup_commands)?;
        }
        if let Some(pane) = self.quick_terminal {
            map.serialize_entry("quick_terminal", pane)?;
        }
        map.serialize_entry("version", &self.version)?;
        map.serialize_entry("projects", &self.projects)?;
        if !self.workspaces.is_empty() {
            map.serialize_entry("workspaces", self.workspaces)?;
        }
        map.end()
    }
}

/// Names a window in file names, so it can't climb out of the profile.
pub(crate) fn validate_name(window: &str) -> Result<(), AppError> {
    if window.is_empty()
        || window.len() > MAX_NAME
        || !window
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(AppError::InvalidState(
            "window names use letters, digits, - and _, up to 64 characters".into(),
        ));
    }
    Ok(())
}

impl AppState {
    /// The layout file: what every window of the profile shares.
    pub(crate) fn layout_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let viewed = self.projects.iter().any(|project| {
            project.groups.is_some() || project.tabs.iter().any(|tab| tab.zoomed.is_some())
        });
        let projects = if viewed {
            let mut projects = self.projects.clone();
            for project in &mut projects {
                for tab in &mut project.tabs {
                    tab.zoomed = None;
                }
                if let Some(groups) = &mut project.groups {
                    groups.reset_selection();
                }
            }
            Cow::Owned(projects)
        } else {
            Cow::Borrowed(self.projects.as_slice())
        };
        let shared = Shared {
            servers: &self.servers,
            starting_directories: &self.starting_directories,
            startup_commands: &self.startup_commands,
            quick_terminal: self.quick_terminal.as_ref(),
            version: self.version,
            projects,
            workspaces: &self.workspaces,
        };
        document(&shared)
    }

    /// The view file: what only this window shows.
    pub(crate) fn view_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        document(&self.view())
    }

    /// This window's view of the layout.
    pub(crate) fn view(&self) -> View {
        let zoomed = self
            .projects
            .iter()
            .flat_map(|project| &project.tabs)
            .filter_map(|tab| Some((tab.id, tab.zoomed?)))
            .collect();
        let group_selected = self
            .projects
            .iter()
            .filter_map(|project| project.groups.as_ref())
            .flat_map(TabGroups::selection)
            .collect();
        View {
            version: VERSION,
            window: self.window.clone(),
            zoomed,
            group_selected,
        }
    }
}

fn document(value: &impl Serialize) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}
