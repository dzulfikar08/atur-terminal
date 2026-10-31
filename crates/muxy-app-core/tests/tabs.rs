use muxy_app_core::{AppState, TabCloseScope, TabId};

type Result = std::result::Result<(), Box<dyn std::error::Error>>;

#[test]
fn bulk_close_targets_follow_project_order_and_skip_pinned_tabs() -> Result {
    let mut state = AppState::bootstrap()?;
    let home = state.home().id;
    let first = state.open_terminal_tab(home)?;
    let second = state.open_terminal_tab(home)?;
    let third = state.open_terminal_tab(home)?;
    let fourth = state.open_terminal_tab(home)?;
    state.toggle_tab_pin(first)?;
    assert_eq!(
        state.home().closable_tabs(third, TabCloseScope::Other),
        [second, fourth]
    );
    assert_eq!(
        state.home().closable_tabs(third, TabCloseScope::Left),
        [second]
    );
    assert_eq!(
        state.home().closable_tabs(third, TabCloseScope::Right),
        [fourth]
    );
    assert!(
        state
            .home()
            .closable_tabs(first, TabCloseScope::Left)
            .is_empty()
    );
    assert!(
        state
            .home()
            .closable_tabs(TabId::new(), TabCloseScope::Other)
            .is_empty()
    );
    Ok(())
}

#[test]
fn moving_a_pane_to_another_tab_keeps_its_session_and_closes_a_tab_it_empties() -> Result {
    let mut state = AppState::bootstrap()?;
    let home = state.home().id;
    let first = state.open_terminal_tab(home)?;
    let second = state.open_terminal_tab(home)?;
    let lone = state.home().tabs[1].panes[0].id;
    let wide = state.home().tabs[0].panes[0].id;
    let split = state.split_pane(wide, muxy_app_core::Direction::Right)?;

    state.move_pane_to_tab(split, second)?;
    let tabs = &state.home().tabs;
    assert_eq!(tabs[0].panes.len(), 1);
    assert_eq!(tabs[0].layout.leaves(), [wide]);
    assert_eq!(tabs[1].layout.leaves(), [lone, split]);

    assert!(state.move_pane_to_tab(wide, first).is_err());

    state.move_pane_to_tab(wide, second)?;
    let tabs = &state.home().tabs;
    assert_eq!(tabs.len(), 1, "the emptied tab closes");
    assert_eq!(tabs[0].id, second);
    assert_eq!(tabs[0].layout.leaves(), [lone, split, wide]);
    Ok(())
}
