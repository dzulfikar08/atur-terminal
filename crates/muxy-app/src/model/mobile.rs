use std::time::Duration;

use gpui::{AnyWindowHandle, Context, Task};
use muxy_app_core::ServerId;
use muxy_protocol::{
    DeviceId, ListenerStatus, PairingOffer, RemoteAccessSettings, RemoteAccessState,
};
use muxy_ui::tr;

use super::{AppModel, Quitting};
use crate::boot::Work;
use crate::views::settings::mobile::{Pairing, Phase};
use crate::views::settings::{Change, SettingsView};

/// How long a server keeps a pairing code. It is counted from the reply, so a
/// server whose clock differs from this one's still shows the right countdown.
const PAIRING_SECONDS: u64 = 5 * 60;

/// Phone access to one server as Settings shows it; the server owns the real state.
#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "The running request and the follow-ups it owes"
)]
pub(super) struct MobileAccess {
    pub(super) state: Option<RemoteAccessState>,
    /// The pairing code on screen, until it is dismissed.
    pub(super) pairing: Option<Pairing>,
    pub(super) busy: bool,
    /// The running request creates a pairing code.
    starting: bool,
    /// The server reported a change while a request was running.
    stale: bool,
    /// The user cancelled pairing while a request was running.
    cancel: bool,
    /// A pairing code was asked for: turn phone access on if it is off, then show one.
    want_pairing: bool,
    /// The devices paired when the code was made, to tell which one used it.
    known: Vec<DeviceId>,
    pub(super) error: Option<String>,
    expiry: Option<Task<()>>,
}

impl MobileAccess {
    /// Works out what became of a waiting code from the server's state: a
    /// device paired with it, or it ended unused.
    fn settle(&mut self, state: &RemoteAccessState) {
        let Some(pairing) = self.pairing.as_mut() else {
            return;
        };
        if state.pairing_expires_at.is_some() || matches!(pairing.phase, Phase::Paired(_)) {
            return;
        }
        self.expiry = None;
        pairing.phase = match state
            .devices
            .iter()
            .find(|device| !self.known.contains(&device.id))
        {
            Some(device) => Phase::Paired(device.name.clone()),
            None => Phase::Ended,
        };
    }

    fn waiting(&self) -> bool {
        self.pairing
            .as_ref()
            .is_some_and(|pairing| pairing.phase == Phase::Waiting)
    }
}

/// What a requested pairing code needs next.
enum Next {
    Start,
    Enable(RemoteAccessSettings),
    Wait,
    Stop,
}

impl AppModel {
    /// The server whose phone access Settings shows: the chosen one, or this
    /// computer's when the chosen one is no longer listed.
    pub(super) fn mobile_target(&self) -> ServerId {
        if self.servers.get(self.mobile_server).is_some() {
            self.mobile_server
        } else {
            ServerId::local()
        }
    }

    fn mobile_available(&self, server: ServerId) -> bool {
        self.quitting == Quitting::Idle && self.ready(server)
    }

    fn mobile_mut(&mut self, server: ServerId) -> Option<&mut MobileAccess> {
        self.servers
            .get_mut(server)
            .map(|runtime| &mut runtime.mobile)
    }

    /// Sends a request that settles the state, and holds the next one until it replies.
    fn send_mobile(&mut self, server: ServerId, work: Work, cx: &mut Context<Self>) -> bool {
        let sent = self.send(server, work, cx);
        if let Some(mobile) = self.mobile_mut(server) {
            mobile.busy = sent;
        }
        sent
    }

    /// Reads the state of the server Settings shows.
    pub(crate) fn read_mobile(&mut self, cx: &mut Context<Self>) {
        self.read_remote_access(self.mobile_target(), cx);
    }

    pub(crate) fn read_remote_access(&mut self, server: ServerId, cx: &mut Context<Self>) {
        if !self.mobile_available(server) {
            return;
        }
        let Some(mobile) = self.mobile_mut(server) else {
            return;
        };
        if mobile.busy {
            mobile.stale = true;
            return;
        }
        self.send_mobile(server, Work::ReadRemoteAccess, cx);
        self.sync_preferences(cx);
    }

    /// Paired devices connected, disconnected, or changed; refresh only while
    /// Settings shows the server.
    pub(super) fn remote_access_changed(&mut self, server: ServerId, cx: &mut Context<Self>) {
        if self.settings_window.is_some() && server == self.mobile_target() {
            self.read_remote_access(server, cx);
        }
    }

    /// Settings now shows another server's phone access.
    pub(crate) fn select_mobile_server(&mut self, server: ServerId, cx: &mut Context<Self>) {
        let current = self.mobile_target();
        if server == current || self.servers.get(server).is_none() {
            return;
        }
        self.withdraw_pairing(current, cx);
        self.mobile_server = server;
        self.read_remote_access(server, cx);
        self.sync_preferences(cx);
        cx.notify();
    }

    pub(super) fn write_mobile_preference(
        &mut self,
        change: Change,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let server = self.mobile_target();
        if !self.mobile_available(server) {
            return Err(tr!("Connect to the server before changing mobile access").to_string());
        }
        let Some(mobile) = self.servers.get(server).map(|runtime| &runtime.mobile) else {
            return Err(tr!("Connect to the server before changing mobile access").to_string());
        };
        if mobile.busy {
            return Err(tr!("Wait for the current mobile access change to finish").to_string());
        }
        let current = mobile
            .state
            .as_ref()
            .map(|state| state.settings)
            .ok_or_else(|| tr!("Load mobile access first").to_string())?;
        let settings = match change {
            Change::MobileAccess(enabled) => RemoteAccessSettings { enabled, ..current },
            Change::Field("mobile-port", port) => RemoteAccessSettings {
                port: port
                    .parse()
                    .map_err(|_| tr!("Use a port from 1024 to 65535").to_string())?,
                ..current
            },
            _ => return Err(tr!("Unknown mobile access setting").to_string()),
        };
        settings
            .validate()
            .map_err(|_| tr!("Use a port from 1024 to 65535").to_string())?;
        if settings != current && !self.send_mobile(server, Work::WriteRemoteAccess(settings), cx) {
            return Err(tr!("Could not send the change to the server").to_string());
        }
        Ok(())
    }

    /// Opens Settings at Phones for `server` and shows a pairing code there,
    /// turning phone access on first when it is off.
    pub(crate) fn pair_phone_on(&mut self, server: ServerId, cx: &mut Context<Self>) {
        if self.servers.get(server).is_none() {
            return;
        }
        self.show_settings(cx);
        if self.settings_window.is_none() {
            return;
        }
        self.select_mobile_server(server, cx);
        if let Some(settings) = &self.settings_window {
            settings.view.update(cx, SettingsView::show_mobile);
        }
        self.pair_phone(cx);
    }

    /// Shows a pairing code for the server Settings shows.
    pub(crate) fn pair_phone(&mut self, cx: &mut Context<Self>) {
        let server = self.mobile_target();
        if !self.mobile_available(server) {
            return;
        }
        let Some(mobile) = self.mobile_mut(server) else {
            return;
        };
        mobile.error = None;
        mobile.want_pairing = true;
        self.continue_pairing(server, cx);
        self.sync_preferences(cx);
    }

    /// Takes the next step toward a requested code, once no request is running.
    fn continue_pairing(&mut self, server: ServerId, cx: &mut Context<Self>) {
        let Some(mobile) = self.mobile_mut(server) else {
            return;
        };
        if !mobile.want_pairing || mobile.busy {
            return;
        }
        let Some(state) = &mobile.state else {
            self.read_remote_access(server, cx);
            return;
        };
        let next = match (&state.status, state.settings.enabled) {
            (ListenerStatus::Listening, _) => Next::Start,
            (ListenerStatus::Disabled, false) => Next::Enable(RemoteAccessSettings {
                enabled: true,
                ..state.settings
            }),
            (ListenerStatus::Disabled, true) => Next::Wait,
            (ListenerStatus::Failed(_) | ListenerStatus::Unrecognized(_), _) => Next::Stop,
        };
        match next {
            Next::Start => {
                mobile.want_pairing = false;
                mobile.error = None;
                let sent = self.send_mobile(server, Work::StartPairing, cx);
                if let Some(mobile) = self.mobile_mut(server) {
                    mobile.starting = sent;
                }
            }
            Next::Enable(settings) => {
                self.send_mobile(server, Work::WriteRemoteAccess(settings), cx);
            }
            Next::Wait => {}
            Next::Stop => mobile.want_pairing = false,
        }
    }

    /// Dismisses the code of the server Settings shows.
    pub(crate) fn cancel_pairing(&mut self, cx: &mut Context<Self>) {
        let server = self.mobile_target();
        self.cancel_server_pairing(server, cx);
    }

    fn cancel_server_pairing(&mut self, server: ServerId, cx: &mut Context<Self>) {
        let available = self.mobile_available(server);
        let Some(mobile) = self.mobile_mut(server) else {
            return;
        };
        let waiting = mobile.waiting();
        mobile.pairing = None;
        mobile.expiry = None;
        mobile.want_pairing = false;
        if waiting || mobile.starting {
            if mobile.busy {
                mobile.cancel = true;
            } else if available {
                self.send_mobile(server, Work::CancelPairing, cx);
            }
        }
        self.sync_preferences(cx);
    }

    /// A code must not stay valid on the server after Settings closes or moves on.
    fn withdraw_pairing(&mut self, server: ServerId, cx: &mut Context<Self>) {
        if self.mobile_mut(server).is_some_and(|mobile| {
            mobile.pairing.is_some() || mobile.starting || mobile.want_pairing
        }) {
            self.cancel_server_pairing(server, cx);
        }
    }

    /// Settings closed: no server keeps a code that nobody can scan.
    pub(super) fn withdraw_pairings(&mut self, cx: &mut Context<Self>) {
        for server in self.servers.ids() {
            self.withdraw_pairing(server, cx);
        }
    }

    pub(crate) fn confirm_revoke(
        &mut self,
        device: DeviceId,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let server = self.mobile_target();
        let Some(name) = self
            .servers
            .get(server)
            .and_then(|runtime| runtime.mobile.state.as_ref())
            .and_then(|state| {
                state
                    .devices
                    .iter()
                    .find(|paired| paired.id == device)
                    .map(|paired| paired.name.clone())
            })
        else {
            return;
        };
        if self.close_prompt.is_some() {
            return;
        }
        let generation = self.generation(server);
        self.close_prompt = Some(cx.spawn(async move |model, cx| {
            let response = crate::views::confirm::prompt_revoke(window, &name, cx).await;
            let _ = model.update(cx, |model, cx| {
                model.finish_confirmation(cx);
                match response {
                    Ok(true)
                        if generation == model.generation(server)
                            && model.mobile_available(server) =>
                    {
                        if model.mobile_mut(server).is_some_and(|mobile| mobile.busy) {
                            model.set_mobile_error(
                                server,
                                tr!("Wait for the current mobile access change to finish")
                                    .to_string(),
                            );
                        } else {
                            model.send_mobile(server, Work::RevokeDevice(device), cx);
                        }
                    }
                    Err(error) => model.set_mobile_error(server, error),
                    Ok(_) => {}
                }
                model.sync_preferences(cx);
                cx.notify();
            });
        }));
    }

    /// Lets a paired phone view and control the screen of the server Settings
    /// shows, or stops it.
    pub(crate) fn set_device_screen(
        &mut self,
        device: DeviceId,
        allowed: bool,
        cx: &mut Context<Self>,
    ) {
        let server = self.mobile_target();
        if !self.mobile_available(server) {
            return;
        }
        if self.mobile_mut(server).is_some_and(|mobile| mobile.busy) {
            self.set_mobile_error(
                server,
                tr!("Wait for the current mobile access change to finish").to_string(),
            );
        } else {
            self.send_mobile(server, Work::SetDeviceScreen(device, allowed), cx);
        }
        self.sync_preferences(cx);
        cx.notify();
    }

    fn set_mobile_error(&mut self, server: ServerId, error: String) {
        if let Some(mobile) = self.mobile_mut(server) {
            mobile.error = Some(error);
        }
    }

    pub(super) fn receive_remote_access(
        &mut self,
        server: ServerId,
        result: Result<RemoteAccessState, muxy_client::ClientError>,
        cx: &mut Context<Self>,
    ) {
        let Some(mobile) = self.mobile_mut(server) else {
            return;
        };
        mobile.busy = false;
        match result {
            Ok(state) => {
                mobile.settle(&state);
                mobile.error = None;
                mobile.state = Some(state);
            }
            Err(error) => {
                mobile.error = Some(error.to_string());
                mobile.want_pairing = false;
            }
        }
        self.finish_mobile_request(server, cx);
    }

    pub(super) fn receive_pairing(
        &mut self,
        server: ServerId,
        result: Result<PairingOffer, muxy_client::ClientError>,
        cx: &mut Context<Self>,
    ) {
        let settings_open = self.settings_window.is_some();
        let generation = self.generation(server);
        let Some(mobile) = self.mobile_mut(server) else {
            return;
        };
        mobile.busy = false;
        mobile.starting = false;
        match result {
            Ok(_) if mobile.cancel || !settings_open => mobile.cancel = true,
            Ok(offer) => {
                let pairing = Pairing::new(&offer, Duration::from_secs(PAIRING_SECONDS));
                let lifetime = pairing.remaining();
                mobile.known = mobile
                    .state
                    .iter()
                    .flat_map(|state| state.devices.iter().map(|device| device.id))
                    .collect();
                mobile.pairing = Some(pairing);
                mobile.error = None;
                mobile.expiry = Some(cx.spawn(async move |model, cx| {
                    cx.background_executor().timer(lifetime).await;
                    let _ = model.update(cx, |model, cx| {
                        model.pairing_expired(server, generation, cx);
                    });
                }));
            }
            Err(error) => mobile.error = Some(error.to_string()),
        }
        self.finish_mobile_request(server, cx);
    }

    /// The code ran out. The server may not have noticed yet, so ask it, in
    /// case the phone paired in the last moment.
    fn pairing_expired(&mut self, server: ServerId, generation: u64, cx: &mut Context<Self>) {
        if self.generation(server) != generation {
            return;
        }
        if let Some(pairing) = self
            .mobile_mut(server)
            .and_then(|mobile| mobile.pairing.as_mut())
            && pairing.phase == Phase::Waiting
        {
            pairing.phase = Phase::Ended;
        }
        self.read_remote_access(server, cx);
        self.sync_preferences(cx);
        cx.notify();
    }

    fn finish_mobile_request(&mut self, server: ServerId, cx: &mut Context<Self>) {
        let available = self.mobile_available(server);
        let Some(mobile) = self.mobile_mut(server) else {
            return;
        };
        let cancel = std::mem::take(&mut mobile.cancel);
        let stale = std::mem::take(&mut mobile.stale);
        if cancel && available {
            self.send_mobile(server, Work::CancelPairing, cx);
        }
        if stale {
            self.read_remote_access(server, cx);
        }
        self.continue_pairing(server, cx);
        self.sync_preferences(cx);
        cx.notify();
    }
}
