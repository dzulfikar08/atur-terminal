use std::collections::BTreeSet;
use std::error::Error;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use muxy_app_core::{AppError, AppState, PaneId, ProjectId, ServerId, TabId, WindowBounds, store};
use muxy_protocol::SessionId;
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn Error>>;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let directory = std::env::temp_dir().join(format!("muxy-app-core-{}", ProjectId::new()));
        fs::create_dir(&directory)?;
        Ok(Self(directory))
    }

    fn path(&self) -> PathBuf {
        self.0.join("state.json")
    }

    fn temporary(&self) -> PathBuf {
        self.0.join("state.json.tmp")
    }

    fn write(&self, value: &Value) -> TestResult {
        fs::write(self.path(), serde_json::to_vec(value)?)?;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn populated() -> Result<AppState, Box<dyn Error>> {
    let mut state = AppState::bootstrap()?;
    let home = state.home().id;
    let first = state.open_terminal_tab(home)?;
    state.open_terminal_tab(home)?;
    state.select_tab(home, first)?;
    let pane = state.home().tabs[0].panes[0].id;
    state.set_pane_title(pane, "日本語 — editor")?;
    state.set_pane_session(pane, SessionId::new(u64::MAX))?;
    state.set_window_bounds(Some(WindowBounds {
        x: -1400.0,
        y: 40.0,
        width: 1200.0,
        height: 900.0,
    }))?;
    Ok(state)
}

#[test]
fn removed_tab_groups_load_as_ordinary_tabs_without_losing_panes() -> TestResult {
    let fixture = Fixture::new()?;
    let mut state = populated()?;
    let pane = state.home().tabs[0].panes[0].id;
    state.split_pane(pane, muxy_app_core::Direction::Down)?;
    let home = state.home().id;
    let first = state.home().tabs[0].id;
    let second = state.home().tabs[1].id;
    state.select_tab(home, second)?;
    let mut value = serde_json::to_value(&state)?;
    value["window"]["tab_layouts"] = json!({
        home.to_string(): {
            "Split": {
                "axis": "Horizontal",
                "ratio": 0.6,
                "first": {"Group": {"id": TabId::new(), "tabs": [first], "active": first}},
                "second": {"Group": {"id": TabId::new(), "tabs": [second], "active": second}}
            }
        }
    });
    fixture.write(&value)?;
    let restored = store::load(fixture.path())?;
    assert_eq!(restored, state);
    store::save(fixture.path(), &restored)?;
    let saved: Value = serde_json::from_slice(&fs::read(fixture.path())?)?;
    assert!(saved.get("window").is_none());
    let view_file = store::view_path(fixture.path(), muxy_app_core::MAIN_WINDOW)?;
    let view: Value = serde_json::from_slice(&fs::read(view_file)?)?;
    assert!(view["window"].get("tab_layouts").is_none());
    assert_eq!(store::load(fixture.path())?, state);
    Ok(())
}

#[test]
fn save_replaces_file_atomically_and_reuses_abandoned_temporary_file() -> TestResult {
    let fixture = Fixture::new()?;
    let mut state = populated()?;
    store::save(fixture.path(), &state)?;
    let old_bytes = fs::read_to_string(fixture.path())?;
    let mut old_file = File::open(fixture.path())?;
    let home = state.home().id;
    state.open_terminal_tab(home)?;
    fs::write(fixture.temporary(), "interrupted write")?;
    store::save(fixture.path(), &state)?;
    let mut still_old = String::new();
    old_file.read_to_string(&mut still_old)?;
    assert_eq!(still_old, old_bytes);
    assert_ne!(fs::read_to_string(fixture.path())?, old_bytes);
    assert_eq!(store::load(fixture.path())?, state);
    assert!(!fixture.temporary().exists());
    Ok(())
}

#[test]
fn corrupt_json_is_an_error_naming_the_path_and_is_not_overwritten() -> TestResult {
    let fixture = Fixture::new()?;
    fs::write(fixture.path(), b"{ broken json")?;
    let error = store::load(fixture.path())
        .err()
        .ok_or("corruption accepted")?;
    assert!(matches!(error, AppError::Json { .. }));
    assert!(
        error
            .to_string()
            .contains(&fixture.path().display().to_string())
    );
    assert!(error.source().is_some());
    assert_eq!(fs::read(fixture.path())?, b"{ broken json");
    Ok(())
}

#[test]
fn malformed_domain_state_is_rejected_with_the_path() -> TestResult {
    let fixture = Fixture::new()?;
    let valid = serde_json::to_value(populated()?)?;
    let mut invalid_states = Vec::new();
    for version in [0, 5] {
        let mut value = valid.clone();
        value["version"] = json!(version);
        invalid_states.push(value);
    }
    for panes in [
        json!([]),
        json!(vec![valid["projects"][0]["tabs"][0]["panes"][0].clone(); 2]),
    ] {
        let mut value = valid.clone();
        value["projects"][0]["tabs"][0]["panes"] = panes;
        invalid_states.push(value);
    }
    for (pointer, replacement) in [
        ("/window/active_pane", json!(PaneId::new())),
        (
            "/projects/0/tabs/0/id",
            valid["projects"][0]["tabs"][1]["id"].clone(),
        ),
        (
            "/projects/0/tabs/0/panes/0/content",
            json!({"type": "unknown"}),
        ),
        ("/projects/0/tabs/0/panes/0/content/session", json!(0)),
        ("/projects/0/id", json!("invalid")),
        ("/projects/0/color", json!("blue")),
        ("/window/bounds/width", json!(-1)),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(pointer).ok_or("missing fixture field")? = replacement;
        invalid_states.push(value);
    }
    let mut duplicate_pane = valid.clone();
    let pane = duplicate_pane["projects"][0]["tabs"][0]["panes"][0]["id"].clone();
    duplicate_pane["projects"][0]["tabs"][1]["active_pane"] = pane.clone();
    duplicate_pane["projects"][0]["tabs"][1]["panes"][0]["id"] = pane;
    invalid_states.push(duplicate_pane);
    for value in invalid_states {
        fixture.write(&value)?;
        let error = store::load(fixture.path())
            .err()
            .ok_or_else(|| format!("invalid state accepted: {value}"))?;
        assert!(
            error
                .to_string()
                .contains(&fixture.path().display().to_string())
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(fixture.path())?)?,
            value
        );
    }
    Ok(())
}

#[test]
fn overlapping_save_preserves_the_active_writer_and_previous_state() -> TestResult {
    let fixture = Fixture::new()?;
    let mut state = populated()?;
    store::save(fixture.path(), &state)?;
    let old_bytes = fs::read(fixture.path())?;
    let home = state.home().id;
    state.open_terminal_tab(home)?;
    let active_writer = File::create(fixture.temporary())?;
    active_writer.lock()?;
    fs::write(fixture.temporary(), "active write")?;
    let error = store::save(fixture.path(), &state)
        .err()
        .ok_or("overlapping save was accepted")?;
    assert!(
        matches!(error, AppError::Io { source, .. } if source.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert_eq!(fs::read(fixture.temporary())?, b"active write");
    assert_eq!(fs::read(fixture.path())?, old_bytes);
    drop(active_writer);
    store::save(fixture.path(), &state)?;
    assert_eq!(store::load(fixture.path())?, state);
    Ok(())
}

#[test]
fn older_files_keep_everything_and_move_server_state_to_the_local_entry() -> TestResult {
    let fixture = Fixture::new()?;
    for version in [1, 2] {
        let mut legacy: Value =
            serde_json::from_str(include_str!("fixtures/desktop-state-v2.json"))?;
        legacy["version"] = json!(version);
        fixture.write(&legacy)?;
        let state = store::load(fixture.path())?;
        assert_eq!(state.version(), 4);
        store::save(fixture.path(), &state)?;
        let saved: Value = serde_json::from_slice(&fs::read(fixture.path())?)?;

        let keys: BTreeSet<_> = saved
            .as_object()
            .ok_or("state object")?
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "projects",
                "quick_terminal",
                "servers",
                "starting_directories",
                "startup_commands",
                "version",
                "workspaces",
            ])
        );
        assert_eq!(saved["version"], 4);
        // A field added to the state must be written to the layout file too.
        let full = serde_json::to_value(&state)?;
        let mut fields: BTreeSet<&str> = full
            .as_object()
            .ok_or("state object")?
            .keys()
            .map(String::as_str)
            .collect();
        assert!(fields.remove("window"));
        assert_eq!(keys, fields);
        let servers = saved["servers"].as_object().ok_or("servers")?;
        assert_eq!(servers.len(), 1);
        let local = &servers[&ServerId::local().to_string()];
        for (old, new) in [
            ("catalog_server", "identity"),
            ("catalog_revision", "catalog_revision"),
            ("project_intents", "project_intents"),
            ("pending_cancellations", "pending_cancellations"),
            ("pending_discards", "pending_discards"),
            ("close_operations", "close_operations"),
        ] {
            assert!(!legacy[old].is_null() && legacy[old] != json!([]) && legacy[old] != json!({}));
            assert_eq!(local[new], legacy[old], "{old}");
        }
        let mut expected = legacy.clone();
        let home = std::env::home_dir().ok_or("home directory")?;
        expected["projects"][0]["directory"] = json!(home.as_os_str().as_bytes());
        let mut zoomed = serde_json::Map::new();
        for project in expected["projects"].as_array_mut().ok_or("projects")? {
            for tab in project["tabs"].as_array_mut().ok_or("tabs")? {
                let pane = tab["zoomed"].take();
                if !pane.is_null() {
                    zoomed.insert(tab["id"].as_str().ok_or("tab id")?.to_owned(), pane);
                }
            }
        }
        assert_eq!(zoomed.len(), 1);
        for key in [
            "projects",
            "quick_terminal",
            "starting_directories",
            "startup_commands",
            "workspaces",
        ] {
            assert_eq!(saved[key], expected[key], "{key}");
        }
        let view_file = store::view_path(fixture.path(), muxy_app_core::MAIN_WINDOW)?;
        let view: Value = serde_json::from_slice(&fs::read(&view_file)?)?;
        assert_eq!(view["window"], expected["window"]);
        assert_eq!(view["zoomed"], Value::Object(zoomed));

        let reloaded = store::load(fixture.path())?;
        assert_eq!(reloaded, state);
        store::save(fixture.path(), &reloaded)?;
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(fixture.path())?)?,
            saved
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&view_file)?)?,
            view
        );
    }
    Ok(())
}

fn inode(path: impl AsRef<std::path::Path>) -> Result<u64, Box<dyn Error>> {
    Ok(std::os::unix::fs::MetadataExt::ino(&fs::metadata(path)?))
}

fn selected_in_group_of(state: &AppState, tab: TabId) -> Result<TabId, Box<dyn Error>> {
    Ok(state
        .home()
        .groups()
        .ok_or("tab groups")?
        .group_of(tab)
        .ok_or("group")?
        .selected())
}

#[test]
fn windows_on_one_layout_keep_their_own_focus_and_zoom() -> TestResult {
    let fixture = Fixture::new()?;
    let mut main = AppState::bootstrap()?;
    let home = main.home().id;
    let first = main.open_terminal_tab(home)?;
    let second = main.open_terminal_tab(home)?;
    let pane = main.home().tabs[0].panes[0].id;
    main.split_pane(pane, muxy_app_core::Direction::Right)?;
    main.select_tab(home, first)?;
    main.toggle_zoom(pane)?;
    store::save_window(fixture.path(), "main", &main)?;

    let mut other = store::load_window(fixture.path(), "other")?;
    assert_eq!(other.home().tabs.len(), 2);
    assert_eq!(other.home().tabs[0].zoomed, None);
    assert_eq!(other.window().selected_tab[&home], first);
    other.select_tab(home, second)?;
    let layout = fs::read(fixture.path())?;
    store::save_window(fixture.path(), "other", &other)?;
    assert_eq!(fs::read(fixture.path())?, layout);

    let main_again = store::load_window(fixture.path(), "main")?;
    assert_eq!(main_again, main);
    assert_eq!(main_again.home().tabs[0].zoomed, Some(pane));
    assert_eq!(main_again.window().selected_tab[&home], first);
    let other_again = store::load_window(fixture.path(), "other")?;
    assert_eq!(other_again, other);
    assert_eq!(other_again.window().selected_tab[&home], second);
    assert_eq!(other_again.home().tabs[0].zoomed, None);
    Ok(())
}

#[test]
fn moving_focus_or_zooming_never_rewrites_the_layout_file() -> TestResult {
    let fixture = Fixture::new()?;
    let mut state = AppState::bootstrap()?;
    let home = state.home().id;
    let first = state.open_terminal_tab(home)?;
    state.open_terminal_tab(home)?;
    let pane = state.home().tabs[0].panes[0].id;
    let side = state.split_pane(pane, muxy_app_core::Direction::Down)?;
    store::save(fixture.path(), &state)?;
    let layout = fs::read(fixture.path())?;
    let written = inode(fixture.path())?;

    state.select_tab(home, first)?;
    state.focus_pane(pane)?;
    state.toggle_zoom(side)?;
    state.set_window_bounds(Some(WindowBounds {
        x: 5.0,
        y: 6.0,
        width: 700.0,
        height: 500.0,
    }))?;
    store::save(fixture.path(), &state)?;
    assert_eq!(fs::read(fixture.path())?, layout);
    assert_eq!(inode(fixture.path())?, written);
    assert_eq!(store::load(fixture.path())?, state);

    state.open_terminal_tab(home)?;
    store::save(fixture.path(), &state)?;
    assert_ne!(fs::read(fixture.path())?, layout);
    Ok(())
}

#[test]
fn a_view_naming_closed_tabs_and_panes_loses_only_those_references() -> TestResult {
    let fixture = Fixture::new()?;
    let mut late = AppState::bootstrap()?;
    let home = late.home().id;
    let first = late.open_terminal_tab(home)?;
    let second = late.open_terminal_tab(home)?;
    let pane = late.home().tabs[1].panes[0].id;
    let side = late.split_pane(pane, muxy_app_core::Direction::Right)?;
    late.toggle_zoom(side)?;
    assert_eq!(late.window().active_pane, Some(side));
    store::save_window(fixture.path(), "late", &late)?;

    let mut main = store::load_window(fixture.path(), "main")?;
    main.close_tab(home, second)?;
    store::save_window(fixture.path(), "main", &main)?;

    let loaded = store::load_window(fixture.path(), "late")?;
    assert_eq!(loaded.home().tabs.len(), 1);
    assert_eq!(loaded.window().selected_tab[&home], first);
    assert_eq!(
        loaded.window().active_pane,
        Some(loaded.home().tabs[0].panes[0].id)
    );
    assert_eq!(loaded.home().tabs[0].zoomed, None);

    // Closing only the zoomed pane keeps the tab selected and drops the zoom.
    let mut late = AppState::bootstrap()?;
    let home = late.home().id;
    late.open_terminal_tab(home)?;
    let pane = late.home().tabs[0].panes[0].id;
    let side = late.split_pane(pane, muxy_app_core::Direction::Right)?;
    late.toggle_zoom(side)?;
    store::save_window(fixture.path(), "late", &late)?;
    let mut main = store::load_window(fixture.path(), "main")?;
    main.close_pane(side)?;
    store::save_window(fixture.path(), "main", &main)?;
    let loaded = store::load_window(fixture.path(), "late")?;
    assert_eq!(loaded.home().tabs[0].zoomed, None);
    assert_eq!(loaded.window().active_pane, Some(pane));
    Ok(())
}

#[test]
fn a_missing_or_unreadable_view_starts_on_the_first_tab() -> TestResult {
    let fixture = Fixture::new()?;
    let mut state = AppState::bootstrap()?;
    let home = state.home().id;
    let first = state.open_terminal_tab(home)?;
    state.open_terminal_tab(home)?;
    state.toggle_zoom(state.home().tabs[1].panes[0].id)?;
    store::save(fixture.path(), &state)?;
    let view = store::view_path(fixture.path(), muxy_app_core::MAIN_WINDOW)?;

    let mut newer: Value = serde_json::from_slice(&fs::read(&view)?)?;
    newer["version"] = json!(99);
    for contents in [
        None,
        Some(b"{ not json".to_vec()),
        Some(b"{}".to_vec()),
        Some(serde_json::to_vec(&newer)?),
    ] {
        match contents {
            Some(contents) => fs::write(&view, contents)?,
            None => fs::remove_file(&view)?,
        }
        let loaded = store::load(fixture.path())?;
        assert_eq!(loaded.home().tabs.len(), 2);
        assert_eq!(loaded.window().selected_tab[&home], first);
        assert_eq!(loaded.home().tabs[1].zoomed, None);
        assert_eq!(
            loaded.window().active_pane,
            Some(loaded.home().tabs[0].panes[0].id)
        );
        store::save(fixture.path(), &loaded)?;
        assert_eq!(store::load(fixture.path())?, loaded);
    }
    Ok(())
}

#[test]
fn tab_groups_are_shared_but_each_window_selects_its_own_tab() -> TestResult {
    let fixture = Fixture::new()?;
    let mut state = AppState::bootstrap()?;
    let home = state.home().id;
    let first = state.open_terminal_tab(home)?;
    let second = state.open_terminal_tab(home)?;
    let third = state.open_terminal_tab(home)?;
    state.split_tab(third, first, muxy_app_core::Direction::Right)?;
    state.select_tab(home, second)?;
    assert_eq!(selected_in_group_of(&state, first)?, second);
    store::save_window(fixture.path(), "a", &state)?;
    let layout = fs::read(fixture.path())?;

    let mut other = store::load_window(fixture.path(), "b")?;
    assert_eq!(selected_in_group_of(&other, second)?, first);
    other.select_tab(home, third)?;
    store::save_window(fixture.path(), "b", &other)?;
    assert_eq!(fs::read(fixture.path())?, layout);

    assert_eq!(store::load_window(fixture.path(), "a")?, state);
    assert_eq!(store::load_window(fixture.path(), "b")?, other);
    Ok(())
}

#[test]
fn sync_adopts_another_windows_layout_and_keeps_this_windows_view() -> TestResult {
    let fixture = Fixture::new()?;
    let mut a = AppState::bootstrap()?;
    let home = a.home().id;
    let first = a.open_terminal_tab(home)?;
    store::save_window(fixture.path(), "a", &a)?;
    let mut b = store::load_window(fixture.path(), "b")?;
    let pane = b.home().tabs[0].panes[0].id;
    b.toggle_zoom(pane)?;
    assert!(!store::sync_window(fixture.path(), "b", &mut b)?);

    let added = a.open_terminal_tab(home)?;
    store::save_window(fixture.path(), "a", &a)?;
    assert!(store::sync_window(fixture.path(), "b", &mut b)?);
    assert_eq!(b.home().tabs.len(), 2);
    assert!(b.home().tabs.iter().any(|tab| tab.id == added));
    assert_eq!(b.window().selected_tab[&home], first);
    assert_eq!(b.home().tabs[0].zoomed, Some(pane));
    assert_eq!(b.window().active_pane, Some(pane));
    assert!(!store::sync_window(fixture.path(), "b", &mut b)?);

    a.close_tab(home, first)?;
    store::save_window(fixture.path(), "a", &a)?;
    assert!(store::sync_window(fixture.path(), "b", &mut b)?);
    assert_eq!(b.home().tabs.len(), 1);
    assert_eq!(b.window().selected_tab[&home], added);
    assert_eq!(b.home().tabs[0].zoomed, None);
    Ok(())
}

#[test]
fn window_names_cannot_leave_the_profile() -> TestResult {
    let fixture = Fixture::new()?;
    let state = AppState::bootstrap()?;
    let long = "x".repeat(65);
    for name in ["", "../escape", "a/b", "a.b", "tab name", long.as_str()] {
        assert!(store::view_path(fixture.path(), name).is_err(), "{name:?}");
        assert!(store::save_window(fixture.path(), name, &state).is_err());
        assert!(store::load_window(fixture.path(), name).is_err());
    }
    assert!(!fixture.path().exists());
    assert_eq!(
        store::view_path(fixture.path(), "second-1_b")?,
        fixture.0.join("state.view-second-1_b.json")
    );
    assert_eq!(
        store::view_path(fixture.path(), muxy_app_core::MAIN_WINDOW)?,
        fixture.0.join("state.view.json")
    );
    Ok(())
}

#[test]
fn a_version_3_file_moves_its_window_into_the_view_file_once() -> TestResult {
    let fixture = Fixture::new()?;
    let legacy = include_str!("fixtures/muxy-2/desktop-state.json");
    fs::write(fixture.path(), legacy)?;
    let view = store::view_path(fixture.path(), muxy_app_core::MAIN_WINDOW)?;
    // A view from an earlier run must not override the window the file holds.
    let mut earlier = store::load(fixture.path())?;
    earlier.set_window_bounds(Some(WindowBounds {
        x: 1.0,
        y: 2.0,
        width: 300.0,
        height: 200.0,
    }))?;
    let other = fixture.0.join("other.json");
    store::save(&other, &earlier)?;
    fs::copy(store::view_path(&other, muxy_app_core::MAIN_WINDOW)?, &view)?;

    let state = store::load(fixture.path())?;
    assert_ne!(state, earlier);
    assert_eq!(state.version(), 4);
    store::save(fixture.path(), &state)?;
    let saved: Value = serde_json::from_slice(&fs::read(fixture.path())?)?;
    assert_eq!(saved["version"], 4);
    assert!(saved.get("window").is_none());
    let saved_view: Value = serde_json::from_slice(&fs::read(&view)?)?;
    let original: Value = serde_json::from_str(legacy)?;
    assert_eq!(saved_view["window"]["bounds"], original["window"]["bounds"]);
    assert_eq!(store::load(fixture.path())?, state);
    Ok(())
}
