use std::error::Error;
use std::fs;

use muxy_app_core::{AppState, ProjectId, ServerId, restore, store};
use muxy_protocol::{ServerPath, SessionId, SessionInfo};

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn closing_offline_persists_cleanup_without_restoring_the_tab() -> TestResult {
    let path = std::env::temp_dir().join(format!("muxy-close-{}.json", ProjectId::new()));
    let mut state = AppState::bootstrap()?;
    let tab = state.open_terminal_tab(state.home().id)?;
    let session = SessionId::new(42).ok_or("zero ID")?;
    state.set_pane_session(state.home().tabs[0].panes[0].id, Some(session))?;
    state.queue_discard(ServerId::local(), session);
    state.queue_discard(ServerId::local(), session);
    state.close_tab(state.home().id, tab)?;
    store::save(&path, &state)?;
    let mut loaded = store::load(&path)?;
    assert!(loaded.home().tabs.is_empty());
    assert_eq!(loaded.pending_discards(ServerId::local()), &[session]);
    assert_eq!(
        restore::plan(&loaded, ServerId::local(), &[]),
        restore::RestorePlan::default()
    );
    loaded.complete_discard(ServerId::local(), session);
    store::save(&path, &loaded)?;
    assert!(
        store::load(&path)?
            .pending_discards(ServerId::local())
            .is_empty()
    );
    fs::remove_file(path)?;
    Ok(())
}

#[test]
fn mixed_restore_identifies_dead_panes_without_mutating_selection() -> TestResult {
    let mut state = AppState::bootstrap()?;
    let home = state.home().id;
    for _ in 0..3 {
        state.open_terminal_tab(home)?;
    }
    let panes: Vec<_> = state
        .home()
        .tabs
        .iter()
        .map(|tab| tab.panes[0].id)
        .collect();
    let live = SessionId::new(11).ok_or("zero ID")?;
    let missing = SessionId::new(22).ok_or("zero ID")?;
    state.set_pane_session(panes[0], Some(live))?;
    state.set_pane_session(panes[1], Some(missing))?;
    state.select_tab(home, state.home().tabs[1].id)?;
    let before = state.clone();
    let plan = restore::plan(
        &state,
        ServerId::local(),
        &[SessionInfo {
            project: state.home().id,
            id: live,
            directory: ServerPath(b"/tmp".to_vec()),
        }],
    );
    assert_eq!(plan.attach, vec![(panes[0], live)]);
    assert_eq!(plan.close, vec![(panes[1], missing)]);
    assert_eq!(plan.create, vec![panes[2]]);
    assert_eq!(state, before);
    let all_missing = restore::plan(&state, ServerId::local(), &[]);
    assert_eq!(
        all_missing.close,
        vec![(panes[0], live), (panes[1], missing)]
    );
    assert!(all_missing.attach.is_empty());
    assert_eq!(state.home().tabs.len(), 3);
    Ok(())
}

fn live(project: ProjectId, id: u64) -> Result<SessionInfo, Box<dyn Error>> {
    Ok(SessionInfo {
        project,
        id: SessionId::new(id).ok_or("zero ID")?,
        directory: ServerPath(b"/tmp".to_vec()),
    })
}

#[test]
fn fresh_client_opens_one_tab_per_live_session_once() -> TestResult {
    let mut state = AppState::bootstrap()?;
    let project = state.add_project(ServerId::local(), std::env::temp_dir())?;
    let sessions = [live(project, 7)?, live(project, 3)?];
    let current = state.window().current_project;
    assert_eq!(state.adopt_live_sessions(ServerId::local(), &sessions)?, 2);
    let tabs = &state.project(project).ok_or("project")?.tabs;
    let shown: Vec<_> = tabs
        .iter()
        .map(|tab| match tab.panes[0].content {
            muxy_app_core::PaneContent::Terminal { session } => session,
            _ => None,
        })
        .collect();
    assert_eq!(shown, vec![Some(sessions[1].id), Some(sessions[0].id)]);
    assert_eq!(state.window().selected_tab.get(&project), Some(&tabs[0].id));
    assert_eq!(state.window().current_project, current);
    assert_eq!(state.adopt_live_sessions(ServerId::local(), &sessions)?, 0);
    assert_eq!(state.project(project).ok_or("project")?.tabs.len(), 2);
    Ok(())
}

#[test]
fn sessions_already_in_a_tab_are_not_duplicated() -> TestResult {
    let mut state = AppState::bootstrap()?;
    let tabbed = state.add_project(ServerId::local(), std::env::temp_dir())?;
    let empty = state.add_project(ServerId::local(), std::env::temp_dir().join("."))?;
    state.open_terminal_tab(tabbed)?;
    let pane = state.project(tabbed).ok_or("project")?.tabs[0].panes[0].id;
    let shown = live(tabbed, 1)?;
    state.set_pane_session(pane, Some(shown.id))?;
    let sessions = [shown, live(tabbed, 2)?, live(empty, 3)?];
    assert_eq!(state.adopt_live_sessions(ServerId::local(), &sessions)?, 1);
    assert_eq!(state.project(tabbed).ok_or("project")?.tabs.len(), 1);
    assert_eq!(state.project(empty).ok_or("project")?.tabs.len(), 1);
    Ok(())
}

#[test]
fn projects_with_tabs_and_other_servers_are_untouched() -> TestResult {
    let mut state = AppState::bootstrap()?;
    let project = state.add_project(ServerId::local(), std::env::temp_dir())?;
    let sessions = [live(project, 5)?];
    state.open_terminal_tab(project)?;
    assert_eq!(state.adopt_live_sessions(ServerId::local(), &sessions)?, 0);
    assert_eq!(state.project(project).ok_or("project")?.tabs.len(), 1);
    let other = ServerId::new();
    assert_eq!(state.adopt_live_sessions(other, &sessions)?, 0);
    Ok(())
}
