use std::collections::HashSet;

use muxy_protocol::{SessionId, SessionInfo};

use crate::{AppError, AppState, PaneContent, PaneId, ProjectId, ProjectStatus, ServerId, Tab};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RestorePlan {
    pub attach: Vec<(PaneId, SessionId)>,
    pub create: Vec<PaneId>,
    pub close: Vec<(PaneId, SessionId)>,
}

/// Plans `server`'s panes against the sessions that server listed.
pub fn plan(state: &AppState, server: ServerId, sessions: &[SessionInfo]) -> RestorePlan {
    let live: HashSet<_> = sessions
        .iter()
        .map(|session| (session.project, session.id))
        .collect();
    let mut plan = RestorePlan::default();
    for (project, pane) in state
        .projects()
        .iter()
        .filter(|project| project.server_id == server)
        .flat_map(|project| {
            project
                .tabs
                .iter()
                .flat_map(move |tab| tab.panes.iter().map(move |pane| (project, pane)))
        })
    {
        match pane.content {
            PaneContent::Terminal { session: Some(id) } if live.contains(&(project.id, id)) => {
                plan.attach.push((pane.id, id));
            }
            PaneContent::Terminal { session: Some(id) } => plan.close.push((pane.id, id)),
            PaneContent::Terminal { session: None }
                if project.status() == ProjectStatus::Available =>
            {
                plan.create.push(pane.id);
            }
            PaneContent::Terminal { session: None }
            | PaneContent::Settings
            | PaneContent::Webview(_)
            | PaneContent::Browser(_) => {}
        }
    }
    plan
}

impl AppState {
    /// Opens one tab per live session of `server` that no pane shows, in
    /// projects with no tabs at all. The caller says when the client has
    /// never had a layout for `server` (its first catalog), so a project
    /// whose tabs the user closed is never refilled. Sessions already shown,
    /// or belonging to projects that have tabs, are left alone. Selection and
    /// focus stay where they were, apart from picking a tab for a project
    /// that had none. Returns how many tabs it opened.
    pub fn adopt_live_sessions(
        &mut self,
        server: ServerId,
        sessions: &[SessionInfo],
    ) -> Result<usize, AppError> {
        let shown: HashSet<_> = self.session_references(server).into_iter().collect();
        let empty: HashSet<ProjectId> = self
            .projects()
            .iter()
            .filter(|project| {
                project.server_id == server
                    && project.tabs.is_empty()
                    && project.status() == ProjectStatus::Available
            })
            .map(|project| project.id)
            .collect();
        let mut adoptable: Vec<_> = sessions
            .iter()
            .filter(|session| empty.contains(&session.project) && !shown.contains(&session.id))
            .collect();
        adoptable.sort_by_key(|session| session.id);
        adoptable.dedup_by_key(|session| session.id);
        for session in &adoptable {
            let mut tab = Tab::terminal();
            if let Some(pane) = tab.panes.first_mut() {
                pane.content = PaneContent::Terminal {
                    session: Some(session.id),
                };
            }
            let id = tab.id;
            self.add_tab(session.project, tab, None)?;
            self.window
                .selected_tab
                .entry(session.project)
                .or_insert(id);
            self.sync_groups(session.project);
        }
        if !adoptable.is_empty() && self.window.active_pane.is_none() {
            self.focus_selected_tab();
        }
        Ok(adoptable.len())
    }
}
