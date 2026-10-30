use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, Bounds, ClipboardItem, Context, InteractiveElement, IntoElement, ParentElement,
    SharedString, Styled, canvas, div, point, px, size,
};
use muxy_app_core::qr::{QUIET_ZONE, QrCode};
use muxy_protocol::{ListenerStatus, PairedDevice, PairingOffer, RemoteAccessState};
use muxy_ui::controls::{self, Choice};
use muxy_ui::l10n::tr_key;
use muxy_ui::tr;

use super::{Category, Change, SettingsEvent, SettingsView};

const CODE_SIZE: f32 = 196.0;

/// Where a pairing code stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Phase {
    /// Shown, and nobody has used it yet.
    Waiting,
    /// A phone paired with it, under this name.
    Paired(String),
    /// It expired or was cancelled unused.
    Ended,
}

/// A pairing offer as shown: its link, and the QR code encoded once.
#[derive(Clone)]
pub(crate) struct Pairing {
    link: String,
    code: Option<Arc<QrCode>>,
    hosts: Vec<String>,
    port: u16,
    deadline: Instant,
    pub(crate) phase: Phase,
}

impl Pairing {
    pub(crate) fn new(offer: &PairingOffer, lifetime: Duration) -> Self {
        let link = offer.invite.to_link();
        Self {
            code: QrCode::encode(&link).map(Arc::new),
            link,
            hosts: offer.invite.hosts.clone(),
            port: offer.invite.port,
            deadline: Instant::now() + lifetime,
            phase: Phase::Waiting,
        }
    }

    /// How long the code stays valid.
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// What a host in a pairing link is, when its spelling says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AddressKind {
    Tailscale,
    LocalNetwork,
    LocalName,
}

impl AddressKind {
    pub(crate) fn of(host: &str) -> Option<Self> {
        if let Ok(address) = host.parse::<Ipv4Addr>() {
            let [first, second, ..] = address.octets();
            return if first == 100 && (64..128).contains(&second) {
                Some(Self::Tailscale)
            } else if address.is_private() || address.is_link_local() {
                Some(Self::LocalNetwork)
            } else {
                None
            };
        }
        host.to_ascii_lowercase()
            .ends_with(".local")
            .then_some(Self::LocalName)
    }

    fn label(self) -> SharedString {
        match self {
            Self::Tailscale => tr!("Tailscale"),
            Self::LocalNetwork => tr!("Local network"),
            Self::LocalName => tr!("Local name"),
        }
    }
}

/// `4:07`, for a code with that long to live.
pub(crate) fn countdown(remaining: Duration) -> String {
    let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

pub(super) fn rows(pane: &SettingsView, cx: &mut Context<SettingsView>) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    if pane.snapshot.mobile_servers.len() > 1 && pane.matches(Category::Mobile, tr_key!("Server")) {
        rows.push(pane.row("mobile-server", tr_key!("Server"), servers(pane, cx)));
    }
    if !pane.snapshot.mobile_connected {
        if pane.matches(Category::Mobile, tr_key!("Server disconnected")) {
            rows.push(
                pane.note(
                    &tr!(
                        "Connect to %@ to manage phone access.",
                        &pane.snapshot.mobile_label
                    ),
                    false,
                )
                .into_any_element(),
            );
        }
        return rows;
    }
    if let Some(error) = &pane.snapshot.mobile_error
        && pane.matches(Category::Mobile, tr_key!("Mobile access error"))
    {
        rows.push(pane.note(error, true).into_any_element());
    }
    let Some(state) = &pane.snapshot.mobile else {
        if pane.matches(Category::Mobile, tr_key!("Loading mobile access")) {
            rows.push(
                pane.note(&tr!("Loading mobile access…"), false)
                    .into_any_element(),
            );
        }
        return rows;
    };
    if pane.matches(Category::Mobile, tr_key!("Allow mobile devices")) {
        rows.push(pane.row(
            "mobile-access",
            tr_key!("Allow mobile devices"),
            pane.toggle(
                "mobile-access",
                state.settings.enabled,
                Change::MobileAccess(!state.settings.enabled),
                cx,
            ),
        ));
    }
    if pane.matches(Category::Mobile, tr_key!("Port")) {
        rows.push(pane.row("mobile-port", tr_key!("Port"), pane.field("mobile-port")));
    }
    if pane.matches(Category::Mobile, tr_key!("Status")) {
        rows.push(pane.row("mobile-status", tr_key!("Status"), status(pane, state)));
    }
    if pane.matches(Category::Mobile, tr_key!("Pair a phone")) {
        rows.push(pane.row_with(
            "mobile-pair",
            tr_key!("Pair a phone"),
            pairing(pane, cx),
            true,
        ));
    }
    if pane.matches(Category::Mobile, tr_key!("Paired devices")) {
        rows.push(pane.row_with(
            "mobile-devices",
            tr_key!("Paired devices"),
            devices(pane, state, cx),
            true,
        ));
    }
    rows
}

/// One choice per server, since each computer has phones of its own.
fn servers(pane: &SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let choices: Vec<Choice> = pane
        .snapshot
        .mobile_servers
        .iter()
        .map(|(id, label)| Choice::new(id.to_string(), label.clone()))
        .collect();
    controls::segmented(
        pane.style(),
        "mobile-server",
        &choices,
        &pane.snapshot.mobile_server.to_string(),
        cx.listener(|_, selected: &SharedString, _, cx| {
            if let Ok(server) = selected.parse::<muxy_app_core::ServerId>() {
                cx.emit(SettingsEvent::MobileServer(server));
            }
        }),
    )
}

fn status(pane: &SettingsView, state: &RemoteAccessState) -> AnyElement {
    let (text, failed) = match (&state.status, state.settings.enabled) {
        (ListenerStatus::Listening, _) => {
            (tr!("Listening on port %lld", state.settings.port), false)
        }
        (ListenerStatus::Failed(message), _) => (tr!("Not listening: %@", message), true),
        (ListenerStatus::Disabled, true) => (tr!("Starting"), false),
        (ListenerStatus::Disabled, false) => (tr!("Off"), false),
        (ListenerStatus::Unrecognized(_), _) => {
            (tr!("Update Atur Terminal to see this status"), false)
        }
    };
    div()
        .debug_selector(|| "settings-mobile-status".into())
        .text_color(if failed {
            pane.theme.danger
        } else {
            pane.theme.fg_muted
        })
        .child(text)
        .into_any_element()
}

/// A button that asks for a pairing code, unless a request is running.
fn pair_button(
    pane: &SettingsView,
    id: &str,
    label: &SharedString,
    cx: &mut Context<SettingsView>,
) -> gpui::Stateful<gpui::Div> {
    controls::button(
        pane.style(),
        id,
        label,
        !pane.snapshot.mobile_busy,
        cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::PairPhone)),
    )
}

/// A button that dismisses the code on screen.
fn dismiss_button(
    pane: &SettingsView,
    id: &str,
    label: &SharedString,
    cx: &mut Context<SettingsView>,
) -> gpui::Stateful<gpui::Div> {
    controls::button(
        pane.style(),
        id,
        label,
        true,
        cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::CancelPairing)),
    )
}

fn pairing(pane: &SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let Some(pairing) = &pane.snapshot.mobile_pairing else {
        return pair_button(pane, "mobile-pair", &tr!("Show Pairing Code"), cx).into_any_element();
    };
    match &pairing.phase {
        Phase::Waiting => waiting(pane, pairing, cx),
        Phase::Paired(name) => div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .debug_selector(|| "settings-mobile-paired".into())
                    .text_color(pane.theme.diff_add)
                    .child(tr!("Paired as %@", name)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(dismiss_button(
                        pane,
                        "mobile-pairing-done",
                        &tr!("Done"),
                        cx,
                    ))
                    .child(pair_button(
                        pane,
                        "mobile-pair-another",
                        &tr!("Pair Another Phone"),
                        cx,
                    )),
            )
            .into_any_element(),
        Phase::Ended => div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .debug_selector(|| "settings-mobile-ended".into())
                    .text_color(pane.theme.fg_muted)
                    .child(tr!("The code expired or was cancelled.")),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(pair_button(
                        pane,
                        "mobile-pair-again",
                        &tr!("Show New Code"),
                        cx,
                    ))
                    .child(dismiss_button(
                        pane,
                        "mobile-pairing-close",
                        &tr!("Close"),
                        cx,
                    )),
            )
            .into_any_element(),
    }
}

/// The code to scan, how long it has, and where phones will look for the server.
fn waiting(pane: &SettingsView, pairing: &Pairing, cx: &mut Context<SettingsView>) -> AnyElement {
    let link = pairing.link.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(code(pairing.code.clone()))
        .child(
            div()
                .debug_selector(|| "settings-mobile-waiting".into())
                .child(tr!(
                    "Waiting for the phone… the code works once and expires in %@",
                    &countdown(pairing.remaining())
                )),
        )
        .child(div().text_color(pane.theme.fg_muted).child(tr!(
            "Scan it with the Atur Terminal mobile app, or open this link on the phone:"
        )))
        .child(
            div()
                .debug_selector(|| "settings-mobile-link".into())
                .min_w(px(0.0))
                .truncate()
                .text_color(pane.theme.fg_muted)
                .child(pairing.link.clone()),
        )
        .child(addresses(pane, pairing))
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .child(controls::button(
                    pane.style(),
                    "mobile-copy-link",
                    &tr!("Copy Link"),
                    true,
                    move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(link.clone())),
                ))
                .child(controls::button(
                    pane.style(),
                    "mobile-cancel-pairing",
                    &tr!("Cancel"),
                    true,
                    cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::CancelPairing)),
                )),
        )
        .into_any_element()
}

/// The addresses in the link, as the phone tries them.
fn addresses(pane: &SettingsView, pairing: &Pairing) -> AnyElement {
    div()
        .debug_selector(|| "settings-mobile-addresses".into())
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .text_color(pane.theme.fg_muted)
                .child(tr!("The phone looks for the server at:")),
        )
        .children(pairing.hosts.iter().map(|host| {
            div()
                .flex()
                .gap(px(8.0))
                .child(format!("{host}:{}", pairing.port))
                .children(
                    AddressKind::of(host)
                        .map(|kind| div().text_color(pane.theme.fg_muted).child(kind.label())),
                )
        }))
        .into_any_element()
}

/// Dark modules on a light square in every theme, merged into one quad per run.
fn code(code: Option<Arc<QrCode>>) -> AnyElement {
    let Some(code) = code else {
        return div()
            .child(tr!("The pairing code is too long to show."))
            .into_any_element();
    };
    let painted = canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            window.paint_quad(gpui::fill(bounds, gpui::white()));
            let span = code.size() + 2 * QUIET_ZONE;
            let module = (bounds.size.width / units(span)).floor();
            let inset = (bounds.size.width - module * units(span)) / 2.0;
            let at = |x: usize, y: usize| {
                point(
                    bounds.origin.x + inset + module * units(x + QUIET_ZONE),
                    bounds.origin.y + inset + module * units(y + QUIET_ZONE),
                )
            };
            for y in 0..code.size() {
                let mut x = 0;
                while x < code.size() {
                    if !code.dark(x, y) {
                        x += 1;
                        continue;
                    }
                    let start = x;
                    while code.dark(x, y) {
                        x += 1;
                    }
                    let run = size(module * units(x - start), module);
                    window.paint_quad(gpui::fill(Bounds::new(at(start, y), run), gpui::black()));
                }
            }
        },
    )
    .size(px(CODE_SIZE));
    div()
        .debug_selector(|| "settings-mobile-code".into())
        .child(painted)
        .into_any_element()
}

fn devices(
    pane: &SettingsView,
    state: &RemoteAccessState,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    if state.devices.is_empty() {
        return div()
            .text_color(pane.theme.fg_muted)
            .child(tr!("No paired devices yet."))
            .into_any_element();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .children(state.devices.iter().map(|device| {
            let id = device.id;
            let screen = device.screen;
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .debug_selector(move || format!("settings-mobile-device-{id}"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w(px(0.0))
                        .child(device.name.clone())
                        .child(
                            div()
                                .text_color(pane.theme.fg_muted)
                                .child(presence(device, now)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(tr!("Screen"))
                                .child(controls::toggle(
                                    pane.style(),
                                    &format!("mobile-screen-{id}"),
                                    screen,
                                    cx.listener(move |_, _, _, cx| {
                                        cx.emit(SettingsEvent::DeviceScreen(id, !screen));
                                    }),
                                )),
                        )
                        .child(controls::button(
                            pane.style(),
                            &format!("mobile-revoke-{id}"),
                            &tr!("Revoke"),
                            !pane.snapshot.mobile_busy,
                            cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::RevokeDevice(id));
                            }),
                        )),
                )
        }))
        .child(div().text_color(pane.theme.fg_muted).child(tr!(
            "Screen lets a phone view and control this computer's screen."
        )))
        .into_any_element()
}

fn presence(device: &PairedDevice, now: u64) -> SharedString {
    if device.connected {
        return tr!("Connected");
    }
    let Some(seen) = device.last_seen else {
        return tr!("Never connected");
    };
    match now.saturating_sub(seen) {
        0..60 => tr!("Last seen just now"),
        seconds @ 60..3_600 => tr!("Last seen %lld min ago", seconds / 60),
        seconds @ 3_600..86_400 => tr!("Last seen %lld h ago", seconds / 3_600),
        seconds => tr!("Last seen %lld d ago", seconds / 86_400),
    }
}

fn units(value: usize) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}
