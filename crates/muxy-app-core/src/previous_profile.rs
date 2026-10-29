//! The tabs, workspaces, window, and servers of the profile the desktop app
//! used before it was renamed to Atur Terminal.
//!
//! The rename gave the app a new profile directory. The server's files are
//! carried over by `muxy_core::dirs`, but the desktop app keeps its tabs and
//! workspaces client-side in `desktop-state.json`, and its servers in the
//! `[[servers]]` of `settings.toml`, so a fresh profile showed remote
//! projects with no tabs. Both are imported here, once.
//!
//! Tabs are keyed by project ID, and a remote server's project IDs come from
//! the server, so they stay as they are. What changes is the server a project
//! belongs to: a server entry created again gets a new ID, and the server that
//! answers for it is recognised by its identity (or, before it has answered,
//! by its address). The old server's projects move onto that entry.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::backup::Result;
use crate::settings::{ServerEntry, Settings};
use crate::{AppError, AppState, Project, ProjectId, ServerId, Workspace, WorkspaceId, store};

/// Marks a profile whose one import of the previous profile was decided.
const CHECKED: &str = "previous-profile-import-checked";
const STATE: &str = "desktop-state.json";
/// What the desktop state was called before `desktop-state.json`.
const LEGACY_STATE: &str = "state.json";
const SETTINGS: &str = "settings.toml";

/// What an import brought over.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Imported {
    pub tabs: usize,
    pub workspaces: usize,
    /// Servers added to the settings.
    pub servers: usize,
}

impl Imported {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// Imports the desktop state and servers of `previous`, the profile this app
/// used before the rename, into `profile`, once. The files of `previous` are
/// only read.
///
/// - A `profile` with no desktop state takes the previous one whole.
/// - A `profile` that has one gains the previous tabs of the projects it lists
///   without tabs, and the projects, workspaces, and window choices it lacks.
///   Nothing it already has is replaced.
/// - Servers listed in the previous settings are added unless `profile` lists
///   the same ID, the same server (by identity), or the same address.
///
/// Returns `None` when this was done before or `previous` holds nothing. A
/// profile with files the import cannot use is marked done and the reason is
/// returned once. A failure to read or write a file leaves it to the next
/// start.
///
/// # Errors
///
/// When the previous or current files are invalid, or a file cannot be read
/// or written.
pub fn import(previous: &Path, profile: &Path) -> Result<Option<Imported>> {
    let checked = profile.join(CHECKED);
    if checked.try_exists()? || same_directory(previous, profile) {
        return Ok(None);
    }
    let has_state = previous.join(STATE).is_file() || previous.join(LEGACY_STATE).is_file();
    if !has_state && !previous.join(SETTINGS).is_file() {
        return Ok(None);
    }
    let plan = match plan(previous, profile, has_state) {
        Ok(plan) => plan,
        Err(error) => {
            // Content that is wrong stays wrong; a file that could not be
            // read may be readable next time.
            let unreadable = matches!(error.downcast_ref::<AppError>(), Some(AppError::Io { .. }));
            if !unreadable {
                fs::write(checked, b"")?;
            }
            return Err(error);
        }
    };
    if !plan.servers.is_empty() {
        Settings::add_servers(&profile.join(SETTINGS), plan.servers)?;
    }
    if let Some(state) = &plan.state {
        store::save(profile.join(STATE), state)?;
    }
    fs::write(checked, b"")?;
    Ok(Some(plan.imported).filter(|imported| !imported.is_empty()))
}

struct Plan {
    servers: Vec<ServerEntry>,
    state: Option<AppState>,
    imported: Imported,
}

fn plan(previous: &Path, profile: &Path, has_state: bool) -> Result<Plan> {
    let previous_state = has_state
        .then(|| store::load(previous.join(STATE)))
        .transpose()?;
    let previous_servers = Settings::servers_in(&previous.join(SETTINGS))?;
    let current_servers = Settings::servers_in(&profile.join(SETTINGS))?;
    let current = (profile.join(STATE).is_file() || profile.join(LEGACY_STATE).is_file())
        .then(|| store::load(profile.join(STATE)))
        .transpose()?;
    let (mapping, servers) = map_servers(
        previous_state.as_ref(),
        &previous_servers,
        current.as_ref(),
        &current_servers,
    );
    let mut imported = Imported {
        servers: servers.len(),
        ..Imported::default()
    };
    let state = match (previous_state, current) {
        (None, _) => None,
        (Some(previous), None) => {
            let state = previous.rekeyed(&mapping);
            state.validate()?;
            imported.tabs = tab_count(state.projects.iter());
            imported.workspaces = state.workspaces.len();
            Some(state)
        }
        (Some(previous), Some(current)) => {
            current.with_previous(&previous.rekeyed(&mapping), &mut imported)?
        }
    };
    Ok(Plan {
        servers,
        state,
        imported,
    })
}

/// Which current server each previous one is, and the previous entries to
/// add because the current profile has no such server.
///
/// A server is the same when the current settings list its ID, when the
/// current state holds the identity the previous state saw answering for it,
/// or, for a server that never answered, when both settings give the same
/// SSH destination. Two previous servers never land on one.
fn map_servers(
    previous: Option<&AppState>,
    previous_entries: &[ServerEntry],
    current: Option<&AppState>,
    current_entries: &[ServerEntry],
) -> (BTreeMap<ServerId, ServerId>, Vec<ServerEntry>) {
    let mut previous_ids: BTreeSet<ServerId> =
        previous_entries.iter().map(|entry| entry.id).collect();
    if let Some(previous) = previous {
        previous_ids.extend(
            previous
                .projects
                .iter()
                .map(|project| project.server_id)
                .chain(previous.servers.keys().copied()),
        );
    }
    previous_ids.remove(&ServerId::local());
    let listed = |id: ServerId| current_entries.iter().any(|entry| entry.id == id);
    let mut mapping = BTreeMap::new();
    let mut claimed = BTreeSet::new();
    let mut additions = Vec::new();
    for id in previous_ids {
        let entry = previous_entries.iter().find(|entry| entry.id == id);
        let same_identity = previous
            .and_then(|previous| previous.server_identity(id))
            .and_then(|identity| {
                current?
                    .servers
                    .iter()
                    .find(|(candidate, state)| {
                        !candidate.is_local() && state.identity == Some(identity)
                    })
                    .map(|(candidate, _)| *candidate)
            });
        let same_address = entry.and_then(|entry| {
            current_entries
                .iter()
                .find(|candidate| candidate.destination().trim() == entry.destination().trim())
                .map(|candidate| candidate.id)
        });
        let target = if listed(id) {
            Some(id)
        } else {
            same_identity.or(same_address).or(entry.map(|_| id))
        };
        let Some(target) = target else { continue };
        if !claimed.insert(target) {
            continue;
        }
        mapping.insert(id, target);
        if let Some(entry) = entry
            && !listed(target)
        {
            additions.push(ServerEntry {
                id: target,
                ..entry.clone()
            });
        }
    }
    (mapping, additions)
}

fn same_directory(first: &Path, second: &Path) -> bool {
    matches!(
        (fs::canonicalize(first), fs::canonicalize(second)),
        (Ok(first), Ok(second)) if first == second
    )
}

fn tab_count<'a>(projects: impl Iterator<Item = &'a Project>) -> usize {
    projects.map(|project| project.tabs.len()).sum()
}

impl AppState {
    /// This state with each remote server's projects moved onto the server
    /// `servers` names for it. Servers it does not name are forgotten.
    fn rekeyed(&self, servers: &BTreeMap<ServerId, ServerId>) -> Self {
        let mut state = self.clone();
        let unnamed: BTreeSet<_> = state
            .projects
            .iter()
            .map(|project| project.server_id)
            .chain(state.servers.keys().copied())
            .filter(|id| !id.is_local() && !servers.contains_key(id))
            .collect();
        for server in unnamed {
            state.remove_remote_projects(server);
            state.servers.remove(&server);
        }
        for project in &mut state.projects {
            if let Some(server) = servers.get(&project.server_id) {
                project.server_id = *server;
            }
        }
        state.servers = std::mem::take(&mut state.servers)
            .into_iter()
            .map(|(id, server)| (servers.get(&id).copied().unwrap_or(id), server))
            .collect();
        state
    }

    /// This state plus what `previous` kept that this one lacks, or `None`
    /// when that is nothing. `previous` has its servers named as this
    /// state names them. A project here keeps what it has, and takes the
    /// previous tabs only when it has none. A remote project that is not
    /// here yet is added with its tabs, as its server's catalog is what
    /// removes it if it is gone. This computer's own projects are never
    /// added, for the server's catalog is theirs.
    fn with_previous(&self, previous: &Self, imported: &mut Imported) -> Result<Option<Self>> {
        let mut merged = self.clone();
        let gained = merged.take_previous_projects(previous);
        merged.take_previous_panes(previous, &gained, imported);
        let workspaces = merged.take_previous_workspaces(previous, imported);
        if merged.window.bounds.is_none() {
            merged.window.bounds = previous.window.bounds;
        }
        let current_project = gained
            .iter()
            .find(|(before, _)| *before == previous.window.current_project)
            .map_or(previous.window.current_project, |(_, id)| *id);
        // A window that already has a pane focused was used since the rename.
        let undecided = merged.window.current_project == merged.home().id
            && merged.window.active_pane.is_none()
            && merged.window.workspace.is_none()
            && !merged.window.remote_servers;
        if undecided && merged.project(current_project).is_some() {
            merged.window.current_project = current_project;
            merged.window.workspace = previous
                .window
                .workspace
                .and_then(|id| workspaces.get(&id).copied());
            merged.window.remote_servers = previous.window.remote_servers
                && merged.projects.iter().any(Project::is_remote_home);
        }
        merged.retain_workspace_members();
        merged.ensure_home()?;
        if merged.window.active_pane.is_none() {
            merged.focus_selected_tab();
        }
        merged.validate()?;
        Ok((merged != *self).then_some(merged))
    }

    /// Gives the projects here the tabs `previous` had for them and adds the
    /// remote projects missing here. Returns each project of `previous` that
    /// came over, with the ID it has here.
    fn take_previous_projects(&mut self, previous: &Self) -> Vec<(ProjectId, ProjectId)> {
        let mut gained = Vec::new();
        let ordered = previous
            .projects
            .iter()
            .filter(|project| project.parent_id.is_none())
            .chain(
                previous
                    .projects
                    .iter()
                    .filter(|project| project.parent_id.is_some()),
            );
        for project in ordered {
            // A server's Home is the same project even if a catalog named it
            // differently.
            if let Some(existing) = self.projects.iter_mut().find(|existing| {
                existing.id == project.id
                    || (project.home && existing.home && existing.server_id == project.server_id)
            }) {
                if existing.server_id == project.server_id
                    && existing.tabs.is_empty()
                    && !project.tabs.is_empty()
                {
                    existing.tabs.clone_from(&project.tabs);
                    existing.groups.clone_from(&project.groups);
                    gained.push((project.id, existing.id));
                }
                continue;
            }
            let has_home = self
                .projects
                .iter()
                .any(|existing| existing.server_id == project.server_id && existing.home);
            let parent_fits = project.parent_id.is_none_or(|parent| {
                self.project(parent).is_some_and(|parent| {
                    parent.server_id == project.server_id
                        && !parent.home
                        && parent.kind.is_none()
                        && parent.parent_id.is_none()
                })
            });
            if project.server_id.is_local() || project.home == has_home || !parent_fits {
                continue;
            }
            self.projects.push(project.clone());
            if project.home {
                self.place_home(self.projects.len() - 1);
            }
            gained.push((project.id, project.id));
        }
        gained
    }

    /// What `previous` kept for the panes and selected tabs of the projects
    /// in `gained`.
    fn take_previous_panes(
        &mut self,
        previous: &Self,
        gained: &[(ProjectId, ProjectId)],
        imported: &mut Imported,
    ) {
        for (before, id) in gained {
            let Some(project) = previous.project(*before) else {
                continue;
            };
            imported.tabs += project.tabs.len();
            if let Some(tab) = previous.window.selected_tab.get(before) {
                self.window.selected_tab.insert(*id, *tab);
            }
            for pane in project.tabs.iter().flat_map(|tab| &tab.panes) {
                if let Some(directory) = previous.starting_directories.get(&pane.id) {
                    self.starting_directories
                        .entry(pane.id)
                        .or_insert_with(|| directory.clone());
                }
                if let Some(command) = previous.startup_commands.get(&pane.id) {
                    self.startup_commands
                        .entry(pane.id)
                        .or_insert_with(|| command.clone());
                }
            }
        }
    }

    /// Adds the workspaces of `previous`; one here with the same ID or name
    /// gains their members. Returns which workspace here each one of
    /// `previous` is.
    fn take_previous_workspaces(
        &mut self,
        previous: &Self,
        imported: &mut Imported,
    ) -> BTreeMap<WorkspaceId, WorkspaceId> {
        let mut workspaces = BTreeMap::new();
        for workspace in &previous.workspaces {
            let members: BTreeSet<_> = workspace
                .projects
                .iter()
                .copied()
                .filter(|id| self.project(*id).is_some())
                .collect();
            let name = workspace.name.trim().to_lowercase();
            if let Some(existing) = self.workspaces.iter_mut().find(|existing| {
                existing.id == workspace.id || existing.name.trim().to_lowercase() == name
            }) {
                workspaces.insert(workspace.id, existing.id);
                let before = existing.projects.len();
                existing.projects.extend(members);
                if existing.projects.len() > before {
                    imported.workspaces += 1;
                }
            } else {
                workspaces.insert(workspace.id, workspace.id);
                self.workspaces.push(Workspace {
                    id: workspace.id,
                    name: workspace.name.clone(),
                    projects: members,
                });
                imported.workspaces += 1;
            }
        }
        workspaces
    }
}
