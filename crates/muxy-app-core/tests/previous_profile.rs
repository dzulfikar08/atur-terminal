#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::Path;

use muxy_app_core::previous_profile::{self, Imported};
use muxy_app_core::settings::{ServerEntry, Settings};
use muxy_app_core::{
    AppState, Project, ProjectId, ServerId, TabId, WindowBounds, WorkspaceId, store,
};
use muxy_protocol::{CatalogPage, ProjectDescriptor, ServerIdentity, ServerPath};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

/// The `Muxy 2` profile of a Mac: this computer, a build box, and a staging
/// server, each with projects and tabs, one workspace, and a window.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/muxy-2");

const LOCAL: ServerId = ServerId::local();
const OLD_BUILD: &str = "b4c7e0a2f5d94e2f8a61c3d90b7e1f42";
const OLD_STAGING: &str = "e91f0c6d7a2b4d3f95a8b1c4e7d02f63";
const NEW_BUILD: &str = "7c0d3e9a1b2f4a58b6e4d1c7a93f0e25";
const NEW_STAGING: &str = "2b8f6a4d0c1e4793a5d7e3b9f1c60d84";

const LOCAL_HOME: &str = "b92ae28c2c5f45629e57b96d892ed444";
const APP: &str = "67d7da59e5a94f658cc157234c82c83a";
const BUILD_HOME: &str = "3d9b7f1a5c2e4b80a6f4d1e9c7b30a52";
const API: &str = "c5a1e2f4b7d84f0a9b3c6e1d2a8f7054";
const STAGING_HOME: &str = "8e2a6c4f0b1d4a73b9e5f7c1d3a80b64";
const WEB: &str = "f06b3d8a2c7e41d9a4b5e0c9f2a71d38";
const API_TAB: &str = "9a8b7c6d5e4f4a3b2c1d0e9f8a7b6c5d";
const WORKSPACE: &str = "c10c95a4df8c47afae2878ddf1a2a4e7";

const BUILD_IDENTITY: u128 = 0x5f1c_2a7e_9d3b_4c68_a0e4_d7b1_c2f3_9a85;
const STAGING_IDENTITY: u128 = 0xa3e8_b2d4_c6f1_4970_b5d1_e2a7_c8f4_0b96;

const STATE: &str = "desktop-state.json";
const SETTINGS: &str = "settings.toml";
const VIEW: &str = "desktop-state.view.json";
const CHECKED: &str = "previous-profile-import-checked";

fn server(id: &str) -> ServerId {
    id.parse().expect("server ID")
}

fn project_id(id: &str) -> ProjectId {
    id.parse().expect("project ID")
}

/// The folder Muxy 2 left, with the files its server owns beside the desktop
/// state and settings.
fn muxy_2() -> tempfile::TempDir {
    let directory = tempfile::Builder::new().prefix("Muxy 2").tempdir().unwrap();
    for file in [STATE, SETTINGS] {
        fs::copy(Path::new(FIXTURE).join(file), directory.path().join(file)).unwrap();
    }
    fs::write(
        directory.path().join("server.toml"),
        "history_budget_bytes = 1048576\n",
    )
    .unwrap();
    directory
}

/// Every file in `directory` with its bytes, to see that nothing changed.
fn snapshot(directory: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn names(directory: &Path) -> Vec<String> {
    snapshot(directory).into_keys().collect()
}

fn load(profile: &Path) -> AppState {
    store::load(profile.join(STATE)).unwrap()
}

fn servers(profile: &Path) -> Vec<ServerEntry> {
    Settings::servers_in(&profile.join(SETTINGS)).unwrap()
}

fn project<'a>(state: &'a AppState, id: &str) -> &'a Project {
    state.project(project_id(id)).expect("project")
}

fn tabs(state: &AppState, id: &str) -> usize {
    project(state, id).tabs.len()
}

fn home_of(state: &AppState, server: ServerId) -> &Project {
    state
        .projects()
        .iter()
        .find(|project| project.home && project.server_id == server)
        .expect("Home")
}

fn entry(id: &str, name: &str, ssh: &str) -> ServerEntry {
    ServerEntry {
        id: server(id),
        ..ServerEntry::new(name.into(), ssh.into())
    }
}

fn descriptor(id: ProjectId, home: bool, name: &str, directory: &str) -> ProjectDescriptor {
    ProjectDescriptor {
        id,
        home,
        name: name.into(),
        icon: None,
        logo: None,
        color: "#808080".into(),
        directory: ServerPath(directory.as_bytes().to_vec()),
        kind: None,
        parent_id: None,
    }
}

fn catalog(identity: u128, projects: Vec<ProjectDescriptor>, home: ProjectId) -> CatalogPage {
    CatalogPage {
        server: ServerIdentity::from_u128(identity),
        home,
        revision: 3,
        projects,
        next: None,
        legacy_home: None,
    }
}

/// What the Atur Terminal profile of that Mac held after its servers
/// answered: the projects of each catalog and no tabs. The build box is
/// listed under a new ID, with the project IDs the server gives it, but a
/// Home ID of its own.
fn fresh_state(staging: bool) -> AppState {
    let mut state = AppState::bootstrap().unwrap();
    let home = state.home().descriptor();
    let page = catalog(
        0x5eed,
        vec![
            home.clone(),
            descriptor(project_id(APP), false, "App", "/Users/me/code/app"),
        ],
        home.id,
    );
    state.apply_catalog(LOCAL, &page).unwrap();
    let build_home = ProjectId::from_u128(0xb01d);
    let page = catalog(
        BUILD_IDENTITY,
        vec![
            descriptor(build_home, true, "Home", "/home/dev"),
            descriptor(project_id(API), false, "api", "/home/dev/api"),
        ],
        build_home,
    );
    state.apply_catalog(server(NEW_BUILD), &page).unwrap();
    if staging {
        let staging_home = ProjectId::from_u128(0x57a6);
        let page = catalog(
            STAGING_IDENTITY,
            vec![
                descriptor(staging_home, true, "Home", "/home/deploy"),
                descriptor(project_id(WEB), false, "web", "/home/deploy/web"),
            ],
            staging_home,
        );
        state.apply_catalog(server(NEW_STAGING), &page).unwrap();
    }
    state
}

fn save(profile: &Path, state: &AppState) {
    store::save(profile.join(STATE), state).unwrap();
}

#[test]
fn first_run_takes_the_previous_state_and_servers() -> TestResult {
    let previous = muxy_2();
    let before = snapshot(previous.path());
    let profile = tempfile::tempdir()?;

    let imported = previous_profile::import(previous.path(), profile.path())?;

    assert_eq!(
        imported,
        Some(Imported {
            tabs: 7,
            workspaces: 1,
            servers: 2
        })
    );
    let state = load(profile.path());
    assert_eq!(state.version(), 4);
    assert_eq!(tabs(&state, LOCAL_HOME), 1);
    assert_eq!(tabs(&state, APP), 2);
    assert_eq!(tabs(&state, BUILD_HOME), 1);
    assert_eq!(tabs(&state, API), 2);
    assert_eq!(tabs(&state, STAGING_HOME), 0);
    assert_eq!(tabs(&state, WEB), 1);
    let api = project(&state, API);
    assert_eq!(api.server_id, server(OLD_BUILD));
    assert_eq!(api.tabs[0].custom_title.as_deref(), Some("logs"));
    assert!(project(&state, LOCAL_HOME).tabs[0].pinned);
    assert_eq!(project(&state, WEB).server_id, server(OLD_STAGING));
    assert_eq!(
        state.server_identity(server(OLD_BUILD)),
        Some(ServerIdentity::from_u128(BUILD_IDENTITY))
    );
    assert_eq!(state.catalog_revision(server(OLD_BUILD)), 7);
    assert_eq!(state.catalog_revision(LOCAL), 12);

    assert_eq!(state.workspaces().len(), 1);
    assert_eq!(state.workspaces()[0].name, "Work");
    assert_eq!(state.workspaces()[0].projects.len(), 3);
    let window = state.window();
    assert_eq!(window.current_project, project_id(API));
    assert_eq!(window.workspace, Some(WORKSPACE.parse::<WorkspaceId>()?));
    assert_eq!(
        window.selected_tab[&project_id(API)],
        API_TAB.parse::<TabId>()?
    );
    assert_eq!(
        window.bounds,
        Some(WindowBounds {
            x: 120.0,
            y: 40.0,
            width: 1200.0,
            height: 900.0
        })
    );
    let saved = serde_json::to_value(&state)?;
    assert_eq!(
        saved["startup_commands"]["05db02346af1442d8ea33d4606d93398"],
        "npm run dev"
    );

    let listed = servers(profile.path());
    assert_eq!(
        listed.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        [server(OLD_BUILD), server(OLD_STAGING)]
    );
    assert_eq!(listed[0].name, "Build box");
    assert_eq!(listed[0].ssh, "dev@build.example.com");
    assert!(listed[0].udp);
    assert_eq!(
        listed[1].identity_file.as_deref(),
        Some("~/.ssh/staging_ed25519")
    );
    // The new profile gets the files it needs and no temporary leftovers.
    assert_eq!(names(profile.path()), [STATE, VIEW, CHECKED, SETTINGS]);
    assert_eq!(snapshot(previous.path()), before);
    Ok(())
}

#[test]
fn a_server_entered_again_takes_the_tabs_of_the_server_with_its_identity() -> TestResult {
    let previous = muxy_2();
    let before = snapshot(previous.path());
    let profile = tempfile::tempdir()?;
    // The same build box, under a new ID and another way to reach it.
    Settings::add_servers(
        &profile.path().join(SETTINGS),
        vec![entry(NEW_BUILD, "Build (new)", "buildbox")],
    )?;
    save(profile.path(), &fresh_state(false));
    assert_eq!(tabs(&load(profile.path()), API), 0);

    let imported = previous_profile::import(previous.path(), profile.path())?;

    // Staging is not listed here, so only it is added.
    assert_eq!(
        imported,
        Some(Imported {
            tabs: 7,
            workspaces: 1,
            servers: 1
        })
    );
    let state = load(profile.path());
    let api = project(&state, API);
    assert_eq!(api.server_id, server(NEW_BUILD));
    assert_eq!(api.tabs.len(), 2);
    assert_eq!(api.tabs[0].custom_title.as_deref(), Some("logs"));
    let build_home = home_of(&state, server(NEW_BUILD));
    assert_eq!(build_home.id, ProjectId::from_u128(0xb01d));
    assert_eq!(build_home.tabs.len(), 1);
    assert!(
        state
            .projects()
            .iter()
            .all(|project| project.server_id != server(OLD_BUILD))
    );
    assert_eq!(state.server_identity(server(OLD_BUILD)), None);
    assert_eq!(
        state.server_identity(server(NEW_BUILD)),
        Some(ServerIdentity::from_u128(BUILD_IDENTITY))
    );
    // This computer's projects take their tabs, Home by being the Home.
    assert_eq!(tabs(&state, APP), 2);
    assert_eq!(home_of(&state, LOCAL).tabs.len(), 1);
    // Staging came from the old profile whole.
    assert_eq!(project(&state, WEB).server_id, server(OLD_STAGING));
    assert_eq!(tabs(&state, WEB), 1);
    assert_eq!(
        home_of(&state, server(OLD_STAGING)).id,
        project_id(STAGING_HOME)
    );

    let window = state.window();
    assert_eq!(window.current_project, project_id(API));
    assert_eq!(
        window.selected_tab[&project_id(API)],
        API_TAB.parse::<TabId>()?
    );
    assert_eq!(window.bounds.map(|bounds| bounds.width), Some(1200.0));
    assert_eq!(state.workspaces().len(), 1);
    assert_eq!(
        state.active_workspace().map(|w| w.name.as_str()),
        Some("Work")
    );

    let listed = servers(profile.path());
    assert_eq!(
        listed.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        [server(NEW_BUILD), server(OLD_STAGING)]
    );
    assert_eq!(listed[0].name, "Build (new)");
    assert_eq!(listed[0].ssh, "buildbox");
    assert_eq!(snapshot(previous.path()), before);
    Ok(())
}

#[test]
fn what_the_new_profile_has_is_kept() -> TestResult {
    let previous = muxy_2();
    let profile = tempfile::tempdir()?;
    // Staging is listed under the ID the old profile used, with its own name,
    // and the build box under a new one, already with a tab and a workspace.
    Settings::add_servers(
        &profile.path().join(SETTINGS),
        vec![
            entry(OLD_STAGING, "Staging (renamed)", "staging.internal"),
            entry(NEW_BUILD, "Build (new)", "buildbox"),
        ],
    )?;
    let mut current = fresh_state(false);
    let own_tab = current.open_terminal_tab(project_id(API))?;
    let own_workspace = current.create_workspace("work")?;
    save(profile.path(), &current);

    let imported = previous_profile::import(previous.path(), profile.path())?;

    assert_eq!(imported.map(|imported| imported.servers), Some(0));
    let state = load(profile.path());
    let api = project(&state, API);
    assert_eq!(api.server_id, server(NEW_BUILD));
    assert_eq!(api.tabs.len(), 1);
    assert_eq!(api.tabs[0].id, own_tab);
    // The workspace with the same name gains the members; it is not repeated.
    assert_eq!(state.workspaces().len(), 1);
    assert_eq!(state.workspaces()[0].id, own_workspace);
    assert!(state.workspaces()[0].projects.contains(&project_id(APP)));
    assert!(state.workspaces()[0].projects.contains(&project_id(WEB)));
    // Same ID, not the same entry: nothing is renamed, repeated, or retargeted.
    let listed = servers(profile.path());
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].name, "Staging (renamed)");
    assert_eq!(listed[0].ssh, "staging.internal");
    assert_eq!(listed[1].name, "Build (new)");
    assert_eq!(project(&state, WEB).server_id, server(OLD_STAGING));
    Ok(())
}

#[test]
fn a_server_that_never_answered_is_found_by_its_address() -> TestResult {
    let previous = muxy_2();
    let profile = tempfile::tempdir()?;
    Settings::add_servers(
        &profile.path().join(SETTINGS),
        vec![entry(NEW_STAGING, "Staging (new)", "staging")],
    )?;

    let imported = previous_profile::import(previous.path(), profile.path())?;

    // Only the build box is added; staging is the entry listed here.
    assert_eq!(
        imported,
        Some(Imported {
            tabs: 7,
            workspaces: 1,
            servers: 1
        })
    );
    let state = load(profile.path());
    assert_eq!(project(&state, WEB).server_id, server(NEW_STAGING));
    assert_eq!(
        home_of(&state, server(NEW_STAGING)).id,
        project_id(STAGING_HOME)
    );
    assert_eq!(
        state.server_identity(server(NEW_STAGING)),
        Some(ServerIdentity::from_u128(STAGING_IDENTITY))
    );
    assert_eq!(state.server_identity(server(OLD_STAGING)), None);
    assert!(
        state
            .projects()
            .iter()
            .all(|project| project.server_id != server(OLD_STAGING))
    );
    let listed = servers(profile.path());
    assert_eq!(
        listed.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        [server(NEW_STAGING), server(OLD_BUILD)]
    );
    assert_eq!(listed[0].name, "Staging (new)");
    Ok(())
}

#[test]
fn servers_the_old_settings_no_longer_list_leave_their_projects_behind() -> TestResult {
    let previous = muxy_2();
    let settings = fs::read_to_string(previous.path().join(SETTINGS))?;
    let (build_only, _) = settings
        .split_once("[[servers]]\nid = \"e91f")
        .expect("staging entry");
    fs::write(previous.path().join(SETTINGS), build_only)?;
    let profile = tempfile::tempdir()?;

    let imported = previous_profile::import(previous.path(), profile.path())?;

    assert_eq!(
        imported,
        Some(Imported {
            tabs: 6,
            workspaces: 1,
            servers: 1
        })
    );
    let state = load(profile.path());
    assert!(state.project(project_id(WEB)).is_none());
    assert!(state.project(project_id(STAGING_HOME)).is_none());
    assert_eq!(state.server_identity(server(OLD_STAGING)), None);
    assert_eq!(tabs(&state, API), 2);
    assert_eq!(state.workspaces()[0].projects.len(), 2);
    Ok(())
}

#[test]
fn servers_come_over_without_a_desktop_state() -> TestResult {
    let previous = muxy_2();
    fs::remove_file(previous.path().join(STATE))?;
    let profile = tempfile::tempdir()?;

    let imported = previous_profile::import(previous.path(), profile.path())?;

    assert_eq!(
        imported,
        Some(Imported {
            tabs: 0,
            workspaces: 0,
            servers: 2
        })
    );
    assert_eq!(servers(profile.path()).len(), 2);
    assert!(!profile.path().join(STATE).exists());
    assert!(!previous.path().join(STATE).exists());
    Ok(())
}

#[test]
fn it_runs_once_and_never_brings_back_what_was_closed() -> TestResult {
    let previous = muxy_2();
    let profile = tempfile::tempdir()?;
    previous_profile::import(previous.path(), profile.path())?;
    let imported = snapshot(profile.path());

    assert_eq!(
        previous_profile::import(previous.path(), profile.path())?,
        None
    );
    assert_eq!(snapshot(profile.path()), imported);

    // Closing every tab is a choice the next start keeps.
    let mut state = load(profile.path());
    for tab in project(&state, API).tabs.clone() {
        state.close_tab(project_id(API), tab.id)?;
    }
    save(profile.path(), &state);
    let closed = snapshot(profile.path());
    assert_eq!(
        previous_profile::import(previous.path(), profile.path())?,
        None
    );
    assert_eq!(snapshot(profile.path()), closed);
    assert_eq!(tabs(&load(profile.path()), API), 0);

    // A server removed on purpose is not listed again.
    let mut settings = Settings::load(&profile.path().join(SETTINGS))?;
    settings.remove_server(server(OLD_BUILD), &profile.path().join(SETTINGS))?;
    assert_eq!(
        previous_profile::import(previous.path(), profile.path())?,
        None
    );
    assert_eq!(servers(profile.path()).len(), 1);
    Ok(())
}

#[test]
fn a_crash_before_the_marker_is_repeated_without_harm() -> TestResult {
    let previous = muxy_2();
    let profile = tempfile::tempdir()?;
    previous_profile::import(previous.path(), profile.path())?;
    let done = snapshot(profile.path());
    // What a crash between the last write and the marker leaves.
    fs::remove_file(profile.path().join(CHECKED))?;

    assert_eq!(
        previous_profile::import(previous.path(), profile.path())?,
        None
    );

    assert_eq!(snapshot(profile.path()), done);
    Ok(())
}

#[test]
fn nothing_to_import_changes_nothing_and_is_not_marked() -> TestResult {
    let profile = tempfile::tempdir()?;
    let parent = tempfile::tempdir()?;

    let missing = parent.path().join("Muxy 2");
    assert_eq!(previous_profile::import(&missing, profile.path())?, None);
    assert!(!missing.exists());

    let empty = parent.path().join("empty");
    fs::create_dir(&empty)?;
    assert_eq!(previous_profile::import(&empty, profile.path())?, None);

    // Only what the server owns.
    let server_only = parent.path().join("server-only");
    fs::create_dir(&server_only)?;
    fs::write(server_only.join("server.toml"), "")?;
    assert_eq!(
        previous_profile::import(&server_only, profile.path())?,
        None
    );

    assert!(names(profile.path()).is_empty());
    assert_eq!(names(&empty), Vec::<String>::new());
    assert_eq!(names(&server_only), ["server.toml"]);

    // A profile that appears later is still imported.
    let previous = muxy_2();
    assert!(previous_profile::import(previous.path(), profile.path())?.is_some());
    Ok(())
}

#[test]
fn a_previous_profile_never_imports_into_itself() -> TestResult {
    let previous = muxy_2();
    let before = snapshot(previous.path());
    assert_eq!(
        previous_profile::import(previous.path(), previous.path())?,
        None
    );
    assert_eq!(snapshot(previous.path()), before);
    Ok(())
}

#[test]
fn an_unusable_previous_state_is_reported_once_and_changes_nothing() -> TestResult {
    let previous = muxy_2();
    fs::write(previous.path().join(STATE), "{ not the desktop state")?;
    let before = snapshot(previous.path());
    let profile = tempfile::tempdir()?;

    assert!(previous_profile::import(previous.path(), profile.path()).is_err());

    assert_eq!(names(profile.path()), [CHECKED]);
    assert_eq!(
        previous_profile::import(previous.path(), profile.path())?,
        None
    );
    assert_eq!(names(profile.path()), [CHECKED]);
    assert_eq!(snapshot(previous.path()), before);
    Ok(())
}

#[test]
fn the_old_files_stay_as_they_were_in_every_case() -> TestResult {
    for case in 0..3 {
        let previous = muxy_2();
        let before = snapshot(previous.path());
        let profile = tempfile::tempdir()?;
        if case > 0 {
            Settings::add_servers(
                &profile.path().join(SETTINGS),
                vec![entry(NEW_BUILD, "Build (new)", "buildbox")],
            )?;
            save(profile.path(), &fresh_state(case == 2));
        }
        previous_profile::import(previous.path(), profile.path())?;
        assert_eq!(snapshot(previous.path()), before);
    }
    Ok(())
}
