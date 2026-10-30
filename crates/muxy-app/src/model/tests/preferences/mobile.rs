use super::super::servers::{connect_remote, entry, work};
use super::*;
use crate::views::settings::mobile::{AddressKind, Phase, countdown};
use muxy_protocol::{
    DeviceId, ListenerStatus, PairedDevice, PairingInvite, PairingOffer, RemoteAccessSettings,
    RemoteAccessState,
};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

fn access(enabled: bool, pairing: Option<u64>, devices: Vec<PairedDevice>) -> RemoteAccessState {
    RemoteAccessState {
        revision: 1,
        settings: RemoteAccessSettings {
            enabled,
            port: 7419,
        },
        status: if enabled {
            ListenerStatus::Listening
        } else {
            ListenerStatus::Disabled
        },
        pairing_expires_at: pairing,
        devices,
    }
}

fn phone(name: &str) -> PairedDevice {
    PairedDevice {
        id: DeviceId::new(),
        name: name.into(),
        paired_at: now(),
        last_seen: None,
        connected: false,
    }
}

fn offer(expires_at: u64) -> PairingOffer {
    PairingOffer {
        invite: PairingInvite {
            hosts: vec!["192.168.1.5".into()],
            port: 7419,
            fingerprint: [1; 32],
            secret: [2; 16],
        },
        expires_at,
    }
}

fn phase(model: &AppModel, server: ServerId) -> Option<Phase> {
    model
        .servers
        .get(server)
        .and_then(|runtime| runtime.mobile.pairing.as_ref())
        .map(|pairing| pairing.phase.clone())
}

/// A settings window connected to a stub server that reports the given access.
fn connected(
    cx: &mut TestAppContext,
    state: RemoteAccessState,
) -> (
    Entity<AppModel>,
    &mut VisualTestContext,
    std::sync::mpsc::Receiver<(u64, Work)>,
) {
    let (boot, requests) = stub_boot(AppState::bootstrap().expect("state"));
    cx.update(|cx| crate::views::workspace::bind_keys(&boot.settings.keymap, cx));
    let (view, cx) = settings_window(boot, cx);
    view.update(cx, |model, cx| {
        model.servers.local.connection = ConnectionState::Ready;
        model.read_mobile(cx);
        model.receive_remote_access(ServerId::local(), Ok(state), cx);
    });
    cx.run_until_parked();
    (view, cx, requests)
}

#[gpui::test]
fn a_cancel_during_another_request_still_reaches_the_server(cx: &mut TestAppContext) {
    let (view, cx, requests) = connected(cx, access(true, None, Vec::new()));
    view.update(cx, |model, cx| {
        model.receive_pairing(ServerId::local(), Ok(offer(now() + 300)), cx);
        model.read_mobile(cx);
        assert!(model.servers.local.mobile.busy);
        model.cancel_pairing(cx);
        assert!(model.servers.local.mobile.pairing.is_none());
    });
    assert!(
        requests
            .try_iter()
            .all(|(_, work)| !matches!(work, Work::CancelPairing))
    );
    view.update(cx, |model, cx| {
        model.receive_remote_access(
            ServerId::local(),
            Ok(access(true, Some(now() + 300), Vec::new())),
            cx,
        );
    });
    assert!(
        requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::CancelPairing))
    );
}

#[gpui::test]
fn closing_settings_withdraws_a_code_still_on_screen(cx: &mut TestAppContext) {
    let (view, cx, requests) = connected(cx, access(true, None, Vec::new()));
    view.update(cx, |model, cx| {
        model.receive_pairing(ServerId::local(), Ok(offer(now() + 300)), cx);
    });
    assert!(cx.simulate_close());
    assert!(
        requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::CancelPairing))
    );
    view.read_with(cx, |model, _| {
        assert!(model.servers.local.mobile.pairing.is_none());
    });
}

#[gpui::test]
fn closing_settings_withdraws_a_pending_code(cx: &mut TestAppContext) {
    let (view, cx, requests) = connected(cx, access(true, None, Vec::new()));
    view.update(cx, AppModel::pair_phone);
    assert!(
        requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::StartPairing))
    );
    assert!(cx.simulate_close());
    view.update(cx, |model, cx| {
        assert!(model.settings_window.is_none());
        model.receive_pairing(ServerId::local(), Ok(offer(now() + 300)), cx);
        assert!(model.servers.local.mobile.pairing.is_none());
    });
    assert!(
        requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::CancelPairing))
    );
}

#[gpui::test]
fn a_code_shows_the_phone_that_used_it(cx: &mut TestAppContext) {
    let known = phone("Old phone");
    let (view, cx, _requests) = connected(cx, access(true, None, vec![known.clone()]));
    view.update(cx, |model, cx| {
        model.receive_pairing(ServerId::local(), Ok(offer(now() + 300)), cx);
        assert_eq!(phase(model, ServerId::local()), Some(Phase::Waiting));
        // The code is still on offer, and a phone already paired changes nothing.
        model.receive_remote_access(
            ServerId::local(),
            Ok(access(true, Some(now() + 300), vec![known.clone()])),
            cx,
        );
        assert_eq!(phase(model, ServerId::local()), Some(Phase::Waiting));
        model.receive_remote_access(
            ServerId::local(),
            Ok(access(true, None, vec![known, phone("Pixel")])),
            cx,
        );
        assert_eq!(
            phase(model, ServerId::local()),
            Some(Phase::Paired("Pixel".into()))
        );
    });
}

#[gpui::test]
fn a_code_that_ends_unused_says_so_and_a_new_one_can_follow(cx: &mut TestAppContext) {
    let (view, cx, requests) = connected(cx, access(true, None, Vec::new()));
    view.update(cx, |model, cx| {
        model.receive_pairing(ServerId::local(), Ok(offer(now() + 300)), cx);
        model.receive_remote_access(ServerId::local(), Ok(access(true, None, Vec::new())), cx);
        assert_eq!(phase(model, ServerId::local()), Some(Phase::Ended));
        model.pair_phone(cx);
    });
    assert!(
        requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::StartPairing))
    );
    assert!(
        !requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::CancelPairing)),
        "an ended code has nothing left to cancel"
    );
}

#[gpui::test]
fn a_code_ends_when_its_five_minutes_run_out(cx: &mut TestAppContext) {
    let (view, cx, requests) = connected(cx, access(true, None, Vec::new()));
    view.update(cx, |model, cx| {
        model.receive_pairing(ServerId::local(), Ok(offer(now() + 300)), cx);
    });
    cx.executor().advance_clock(Duration::from_secs(299));
    cx.run_until_parked();
    view.read_with(cx, |model, _| {
        assert_eq!(phase(model, ServerId::local()), Some(Phase::Waiting));
    });
    requests.try_iter().for_each(drop);
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    view.read_with(cx, |model, _| {
        assert_eq!(phase(model, ServerId::local()), Some(Phase::Ended));
    });
    assert!(
        requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::ReadRemoteAccess)),
        "the server is asked in case the phone paired at the last moment"
    );
}

#[gpui::test]
fn asking_to_pair_turns_phone_access_on_first(cx: &mut TestAppContext) {
    let (view, cx, requests) = connected(cx, access(false, None, Vec::new()));
    view.update(cx, AppModel::pair_phone);
    let sent: Vec<_> = requests.try_iter().map(|(_, work)| work).collect();
    assert!(
        sent.iter()
            .any(|work| matches!(work, Work::WriteRemoteAccess(settings) if settings.enabled)),
        "turned on"
    );
    assert!(
        !sent.iter().any(|work| matches!(work, Work::StartPairing)),
        "nothing to pair with before the listener is up"
    );
    view.update(cx, |model, cx| {
        model.receive_remote_access(ServerId::local(), Ok(access(true, None, Vec::new())), cx);
    });
    assert!(
        requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::StartPairing)),
        "a code follows once it listens"
    );
}

#[gpui::test]
fn a_listener_that_cannot_start_asks_for_no_code(cx: &mut TestAppContext) {
    let (view, cx, requests) = connected(cx, access(false, None, Vec::new()));
    view.update(cx, AppModel::pair_phone);
    requests.try_iter().for_each(drop);
    let mut failed = access(true, None, Vec::new());
    failed.status = ListenerStatus::Failed("address in use".into());
    view.update(cx, |model, cx| {
        model.receive_remote_access(ServerId::local(), Ok(failed), cx);
    });
    assert!(
        requests
            .try_iter()
            .all(|(_, work)| !matches!(work, Work::StartPairing))
    );
}

#[gpui::test]
fn pair_a_phone_opens_the_phones_page_and_shows_the_code_with_its_countdown(
    cx: &mut TestAppContext,
) {
    let (view, cx, requests) = connected(cx, access(true, None, Vec::new()));
    view.update(cx, |model, cx| model.pair_phone_on(ServerId::local(), cx));
    assert!(
        requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::StartPairing))
    );
    view.update(cx, |model, cx| {
        model.receive_pairing(ServerId::local(), Ok(offer(now() + 300)), cx);
    });
    cx.run_until_parked();
    for shown in [
        "settings-mobile-code",
        "settings-mobile-waiting",
        "settings-mobile-link",
    ] {
        assert!(cx.debug_bounds(shown).is_some(), "{shown}");
    }
    view.update(cx, |model, cx| {
        model.receive_remote_access(
            ServerId::local(),
            Ok(access(true, None, vec![phone("Pixel")])),
            cx,
        );
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("settings-mobile-paired").is_some());
    assert!(cx.debug_bounds("settings-mobile-code").is_none());
}

#[gpui::test]
fn another_servers_phones_are_managed_through_its_own_worker(cx: &mut TestAppContext) {
    let remote = ServerId::new();
    let (boot, local, remotes) = remote_boot(
        AppState::bootstrap().expect("state"),
        vec![entry(remote, "box")],
    );
    let path = boot.state_path.with_file_name("settings.toml");
    std::fs::create_dir_all(path.parent().expect("profile")).expect("profile");
    let mut file = muxy_app_core::settings::Settings::default();
    for server in &boot.settings.servers {
        file.add_server(server.clone(), &path).expect("listed");
    }
    cx.update(|cx| crate::views::workspace::bind_keys(&boot.settings.keymap, cx));
    let (view, cx) = settings_window(boot, cx);
    view.update(cx, |model, cx| {
        model.receive((ServerId::local(), 1, Update::Connected(Vec::new())), cx);
        acknowledge_catalog(model, cx);
        connect_remote(model, remote, Vec::new(), &[], cx);
        model.select_mobile_server(remote, cx);
    });
    let remote_work = remotes.borrow_mut().remove(&remote).expect("worker");
    assert!(
        work(&remote_work)
            .iter()
            .any(|work| matches!(work, Work::ReadRemoteAccess))
    );

    view.update(cx, |model, cx| {
        model.receive_remote_access(remote, Ok(access(true, None, Vec::new())), cx);
        model.pair_phone(cx);
    });
    assert!(
        work(&remote_work)
            .iter()
            .any(|work| matches!(work, Work::StartPairing))
    );
    view.update(cx, |model, cx| {
        model.receive_pairing(remote, Ok(offer(now() + 300)), cx);
        assert_eq!(phase(model, remote), Some(Phase::Waiting));
        assert_eq!(phase(model, ServerId::local()), None);
    });
    // Moving to this computer's phones withdraws the code from the other server.
    view.update(cx, |model, cx| {
        model.select_mobile_server(ServerId::local(), cx);
    });
    assert!(
        work(&remote_work)
            .iter()
            .any(|work| matches!(work, Work::CancelPairing))
    );
    view.read_with(cx, |model, _| assert_eq!(phase(model, remote), None));
    assert!(
        local
            .try_iter()
            .all(|(_, work)| !matches!(work, Work::StartPairing | Work::WriteRemoteAccess(_))),
        "this computer's server was not asked for anything"
    );
}

#[test]
fn addresses_are_labelled_by_the_network_they_are_on() {
    assert_eq!(
        AddressKind::of("100.101.102.103"),
        Some(AddressKind::Tailscale)
    );
    assert_eq!(AddressKind::of("100.200.0.1"), None);
    assert_eq!(
        AddressKind::of("192.168.1.5"),
        Some(AddressKind::LocalNetwork)
    );
    assert_eq!(AddressKind::of("10.0.0.7"), Some(AddressKind::LocalNetwork));
    assert_eq!(AddressKind::of("203.0.113.7"), None);
    assert_eq!(
        AddressKind::of("Laptop.LOCAL"),
        Some(AddressKind::LocalName)
    );
    assert_eq!(AddressKind::of("box.example.com"), None);
}

#[test]
fn the_countdown_rounds_up_so_it_never_shows_zero_early() {
    assert_eq!(countdown(Duration::from_secs(300)), "5:00");
    assert_eq!(countdown(Duration::from_millis(61_200)), "1:02");
    assert_eq!(countdown(Duration::from_secs(9)), "0:09");
    assert_eq!(countdown(Duration::ZERO), "0:00");
}

#[gpui::test]
fn the_palette_offers_to_pair_a_phone_on_each_server(cx: &mut TestAppContext) {
    let remote = ServerId::new();
    let (boot, _local, _remotes) = remote_boot(
        AppState::bootstrap().expect("state"),
        vec![entry(remote, "box")],
    );
    let (view, cx) = cx.add_window_view(|window, cx| AppModel::new(boot, window, cx));
    view.update(cx, |model, cx| {
        let registry = model.command_registry(true, cx);
        assert!(
            registry
                .command(&format!("pair-phone-{}", ServerId::local()))
                .is_some()
        );
        assert!(registry.command(&format!("pair-phone-{remote}")).is_some());
    });
}
