//! The Screen view: another computer's desktop in a full-window overlay. It
//! carries the mouse, the keyboard, and the text clipboard to that computer.
//! Nothing about it is saved: closing it, or the server's connection ending,
//! ends it.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use atur_screen::{
    PointerButtons, Screen, ScreenOptions, ScreenState, ScreenStats, ScreenTransport,
};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Bounds, ClipboardItem, Context, CursorStyle, EventEmitter,
    FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    Render, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Task, Window, canvas, div, point, px, size,
};
use muxy_app_core::ServerId;
use muxy_ui::components::Tooltip;
use muxy_ui::theme::{Metrics, Theme};
use muxy_ui::tr;

mod geometry;
mod keyboard;
mod policy;

pub(crate) use policy::offered;

use geometry::Fit;

/// What the view asks of the app that shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScreenViewEvent {
    /// Leave the view.
    Close,
    /// Connect again, after a failure or when the stream ended.
    Retry,
}

/// The server's side of one screen stream, as the session reads and writes it.
struct Transport {
    link: muxy_client::ScreenLink,
    /// Why the server ended the stream, when it said.
    reason: Arc<Mutex<Option<String>>>,
}

impl ScreenTransport for Transport {
    fn recv(&self) -> Option<Vec<u8>> {
        match self.link.recv()? {
            muxy_client::ScreenEvent::Data(bytes) => Some(bytes),
            muxy_client::ScreenEvent::Closed(reason) => {
                if !reason.is_empty() {
                    *self.reason.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
                }
                None
            }
        }
    }

    fn send(&self, bytes: &[u8]) -> Result<(), String> {
        self.link.send(bytes).map_err(|error| error.to_string())
    }

    fn processed(&self, bytes: usize) {
        self.link.processed(bytes);
    }

    fn close(&self) {
        self.link.close();
    }
}

enum Phase {
    /// Waiting for the server to start the capture.
    Connecting,
    /// No stream could be opened; why, as the server or the connection said.
    Failed(String),
    Live(Screen),
}

/// The newest picture, ready to draw.
#[derive(Clone)]
struct Shown {
    frame: atur_screen::Frame,
    /// The pixels as a texture; macOS draws its hardware-decoded pictures directly.
    image: Option<Arc<RenderImage>>,
}

/// What the view shows right now.
enum Display {
    Connecting,
    Waiting,
    Picture,
    Failed(String),
}

pub(crate) struct ScreenView {
    pub(crate) server: ServerId,
    /// The server's name, for what the view says about it.
    pub(crate) name: String,
    theme: Theme,
    metrics: Metrics,
    focus: FocusHandle,
    phase: Phase,
    /// Opens the stream, then follows the session's wake-ups.
    session: Option<Task<()>>,
    /// Refreshes the statistics while the picture is idle.
    ticker: Option<Task<()>>,
    remote_reason: Arc<Mutex<Option<String>>>,
    shown: Option<Shown>,
    fit: Fit,
    /// Where the pointer is in the view, which slides a large picture shown at its own size.
    pan: (f32, f32),
    view_only: bool,
    toolbar: bool,
    /// Where the picture area was last drawn, in the window.
    viewport: Rc<Cell<Bounds<Pixels>>>,
    buttons: PointerButtons,
    held: keyboard::Held,
    /// The remote keysym each key down here pressed, so its release matches it.
    pressed: HashMap<String, u32>,
    wheel: (f32, f32),
    /// The text last sent to or received from the remote clipboard.
    clipboard: Option<String>,
    stats: ScreenStats,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ScreenViewEvent> for ScreenView {}

impl Focusable for ScreenView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ScreenView {
    pub(crate) fn new(
        server: ServerId,
        name: String,
        theme: Theme,
        metrics: Metrics,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let weak = cx.weak_entity();
        // The app binds keys that the remote computer wants too, and a binding wins
        // before the view's own key handlers run. Keys are taken first, here.
        let keys = cx.intercept_keystrokes(move |event, window, cx| {
            let _ = weak.update(cx, |view, cx| {
                if view.focus.contains_focused(window, cx) {
                    view.key_down(&event.keystroke, cx);
                    cx.stop_propagation();
                }
            });
        });
        let focus_in = cx.on_focus_in(&focus, window, |view, _, cx| view.sync_clipboard_out(cx));
        let focus_out = cx.on_focus_out(&focus, window, |view, _, _, _| view.release_input());
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if !window.is_window_active() {
                view.release_input();
            } else if view.focus.contains_focused(window, cx) {
                view.sync_clipboard_out(cx);
            }
        });
        cx.on_release(Self::release).detach();
        Self {
            server,
            name,
            theme,
            metrics,
            focus,
            phase: Phase::Connecting,
            session: None,
            ticker: None,
            remote_reason: Arc::default(),
            shown: None,
            fit: Fit::default(),
            pan: (0.5, 0.5),
            view_only: false,
            toolbar: true,
            viewport: Rc::default(),
            buttons: PointerButtons::default(),
            held: keyboard::Held::default(),
            pressed: HashMap::new(),
            wheel: (0.0, 0.0),
            clipboard: None,
            stats: ScreenStats::default(),
            _subscriptions: vec![keys, focus_in, focus_out, activation],
        }
    }

    /// Opens a screen stream on `client` without waiting for it, replacing the
    /// one the view had. Opening the stream can take the server several
    /// seconds, so it happens on its own thread.
    pub(crate) fn connect(&mut self, client: muxy_client::Client, cx: &mut Context<Self>) {
        self.reset(cx);
        let reason: Arc<Mutex<Option<String>>> = Arc::default();
        self.remote_reason = Arc::clone(&reason);
        let (wake_sender, wake) = async_channel::bounded::<()>(1);
        let (done_sender, done) = async_channel::bounded::<Result<Screen, String>>(1);
        let options = ScreenOptions {
            view_only: self.view_only,
            ..ScreenOptions::default()
        };
        let spawned = std::thread::Builder::new()
            .name("screen-connect".into())
            .spawn(move || {
                let result = match client.open_screen() {
                    Ok(link) => Ok(Screen::start(
                        Transport { link, reason },
                        options,
                        move || {
                            let _ = wake_sender.try_send(());
                        },
                    )),
                    Err(error) => Err(connect_message(&error)),
                };
                // The view was closed meanwhile: nobody is left to end the session.
                if let Err(async_channel::SendError(Ok(screen))) = done_sender.send_blocking(result)
                {
                    screen.close();
                }
            });
        if let Err(error) = spawned {
            self.phase = Phase::Failed(error.to_string());
            cx.notify();
            return;
        }
        self.session = Some(cx.spawn(async move |this, cx| {
            let Ok(result) = done.recv().await else {
                return;
            };
            if !this
                .update(cx, |view, cx| view.attached(result, cx))
                .unwrap_or(false)
            {
                return;
            }
            while wake.recv().await.is_ok() {
                if this.update(cx, Self::woke).is_err() {
                    break;
                }
            }
        }));
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, Self::tick).is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Shows `message` instead of a picture, until connecting again.
    pub(crate) fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.reset(cx);
        self.phase = Phase::Failed(message);
        cx.notify();
    }

    /// Ends the session and its work. The view is about to go.
    pub(crate) fn shutdown(&mut self, cx: &mut Context<Self>) {
        self.reset(cx);
    }

    /// Back to nothing connected: the session closed, its work and its input gone.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.session = None;
        self.ticker = None;
        self.release(cx);
        self.phase = Phase::Connecting;
        self.clipboard = None;
        self.pan = (0.5, 0.5);
        self.stats = ScreenStats::default();
        self.buttons = PointerButtons::default();
        self.held = keyboard::Held::default();
        self.pressed.clear();
        self.wheel = (0.0, 0.0);
    }

    /// Closes the session and drops the picture's texture.
    fn release(&mut self, cx: &mut App) {
        if let Phase::Live(screen) = &self.phase {
            screen.close();
        }
        if let Some(image) = self.shown.take().and_then(|shown| shown.image) {
            cx.drop_image(image, None);
        }
    }

    fn attached(&mut self, result: Result<Screen, String>, cx: &mut Context<Self>) -> bool {
        match result {
            Ok(screen) => {
                screen.set_view_only(self.view_only);
                self.phase = Phase::Live(screen);
                self.sync_clipboard_out(cx);
                cx.notify();
                true
            }
            Err(message) => {
                self.phase = Phase::Failed(message);
                cx.notify();
                false
            }
        }
    }

    /// The session has something new: a picture, a state, text for the clipboard.
    fn woke(&mut self, cx: &mut Context<Self>) {
        let Phase::Live(screen) = &self.phase else {
            return;
        };
        let screen = screen.clone();
        if let Some(frame) = screen.frame()
            && self
                .shown
                .as_ref()
                .is_none_or(|shown| shown.frame.id != frame.id)
        {
            self.show(frame, cx);
        }
        if let Some(text) = screen.take_clipboard() {
            self.receive_clipboard(text, cx);
        }
        if self.toolbar {
            self.stats = screen.stats();
        }
        cx.notify();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.toolbar
            && let Phase::Live(screen) = &self.phase
        {
            self.stats = screen.stats();
            cx.notify();
        }
    }

    fn show(&mut self, frame: atur_screen::Frame, cx: &mut Context<Self>) {
        let image = texture(&frame);
        let shown = Shown { frame, image };
        if let Some(previous) = self.shown.replace(shown).and_then(|shown| shown.image) {
            cx.defer(move |cx| cx.drop_image(previous, None));
        }
    }

    fn display(&self) -> Display {
        match &self.phase {
            Phase::Connecting => Display::Connecting,
            Phase::Failed(message) => Display::Failed(message.clone()),
            Phase::Live(screen) => match screen.state() {
                ScreenState::Connecting => Display::Connecting,
                ScreenState::Ready(_) if self.shown.is_some() => Display::Picture,
                ScreenState::Ready(_) => Display::Waiting,
                ScreenState::Closed(reason) => Display::Failed(self.ended(&reason)),
            },
        }
    }

    /// Why the stream ended, in the server's words when it gave any.
    fn ended(&self, reason: &str) -> String {
        let remote = self
            .remote_reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        remote
            .filter(|remote| !remote.trim().is_empty())
            .or_else(|| (!reason.trim().is_empty()).then(|| reason.to_owned()))
            .unwrap_or_else(|| tr!("The screen connection closed.").to_string())
    }

    // Clipboard

    /// Sends the local clipboard's text to the remote one, if it is new.
    fn sync_clipboard_out(&mut self, cx: &mut Context<Self>) {
        let Phase::Live(screen) = &self.phase else {
            return;
        };
        if self.view_only {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if !policy::clipboard_allowed(&text) || self.clipboard.as_deref() == Some(text.as_str()) {
            return;
        }
        screen.send_clipboard(&text);
        self.clipboard = Some(text);
    }

    /// Puts text the remote clipboard gained on the local one.
    fn receive_clipboard(&mut self, text: String, cx: &mut Context<Self>) {
        if !policy::clipboard_allowed(&text) || self.clipboard.as_deref() == Some(text.as_str()) {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.clipboard = Some(text);
    }

    // Keyboard

    /// A key went down while the view has the focus. The reserved chord is the
    /// view's own; everything else is the remote keyboard's.
    fn key_down(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) {
        if keyboard::is_reserved(keystroke) {
            self.toolbar = !self.toolbar;
            cx.notify();
            return;
        }
        let Phase::Live(screen) = &self.phase else {
            if keystroke.key == "escape" {
                cx.emit(ScreenViewEvent::Close);
            }
            return;
        };
        if self.view_only {
            return;
        }
        let screen = screen.clone();
        self.sync_modifiers(&screen, keystroke.modifiers);
        let text = keyboard::typed(keystroke);
        if let Some(keysym) =
            atur_screen::keys::keysym(&keystroke.key, text, keystroke.modifiers.shift)
        {
            self.pressed.insert(keystroke.key.clone(), keysym);
            screen.key(keysym, true);
        }
    }

    fn key_up(&mut self, keystroke: &Keystroke) {
        let Phase::Live(screen) = &self.phase else {
            return;
        };
        if self.view_only {
            return;
        }
        let screen = screen.clone();
        if let Some(keysym) = self.pressed.remove(&keystroke.key) {
            screen.key(keysym, false);
        }
        self.sync_modifiers(&screen, keystroke.modifiers);
    }

    /// Makes the remote modifier keys match `modifiers`, as pressed here.
    fn sync_modifiers(&mut self, screen: &Screen, modifiers: Modifiers) {
        if self.view_only {
            return;
        }
        let after = keyboard::held(modifiers, cfg!(target_os = "macos"));
        for (keysym, down) in keyboard::changes(self.held, after) {
            screen.key(keysym, down);
        }
        self.held = after;
    }

    /// Lets go of every key and button on the remote computer.
    fn release_input(&mut self) {
        self.buttons = PointerButtons::default();
        self.held = keyboard::Held::default();
        self.pressed.clear();
        self.wheel = (0.0, 0.0);
        if let Phase::Live(screen) = &self.phase {
            screen.release_all();
        }
    }

    fn set_view_only(&mut self, view_only: bool, cx: &mut Context<Self>) {
        if view_only {
            self.release_input();
        }
        self.view_only = view_only;
        if let Phase::Live(screen) = &self.phase {
            screen.set_view_only(view_only);
        }
        if !view_only {
            self.sync_clipboard_out(cx);
        }
        cx.notify();
    }

    // Pointer

    /// The picture pixel under `position`, a point in the window. A picture
    /// shown at its own size slides to follow the pointer.
    fn picture_at(&mut self, position: Point<Pixels>, window: &Window) -> Option<(i32, i32)> {
        let frame = &self.shown.as_ref()?.frame;
        let picture = (frame.width, frame.height);
        let bounds = self.viewport.get();
        let local = (
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        );
        let view = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        if self.fit == Fit::Actual {
            self.pan = geometry::pan(view, local);
        }
        let rect = geometry::placement(self.fit, view, picture, window.scale_factor(), self.pan)?;
        Some(geometry::picture_point(rect, picture, local))
    }

    fn set_button(&mut self, button: MouseButton, down: bool) -> bool {
        match button {
            MouseButton::Left => self.buttons.left = down,
            MouseButton::Middle => self.buttons.middle = down,
            MouseButton::Right => self.buttons.right = down,
            MouseButton::Navigate(_) => return false,
        }
        true
    }

    fn send_pointer(
        &mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Phase::Live(screen) = &self.phase else {
            return;
        };
        let screen = screen.clone();
        self.sync_modifiers(&screen, modifiers);
        let Some((x, y)) = self.picture_at(position, window) else {
            return;
        };
        screen.pointer(x, y, self.buttons);
        if self.fit == Fit::Actual {
            cx.notify();
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        if self.set_button(event.button, true) {
            self.send_pointer(event.position, event.modifiers, window, cx);
        }
        cx.stop_propagation();
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.set_button(event.button, false) {
            self.send_pointer(event.position, event.modifiers, window, cx);
        }
        cx.stop_propagation();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.send_pointer(event.position, event.modifiers, window, cx);
    }

    fn scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Phase::Live(screen) = &self.phase else {
            return;
        };
        let screen = screen.clone();
        let (delta, precise) = match event.delta {
            ScrollDelta::Lines(lines) => ((lines.x, lines.y), false),
            ScrollDelta::Pixels(pixels) => ((f32::from(pixels.x), f32::from(pixels.y)), true),
        };
        let (across, along) = geometry::notches(delta, precise, &mut self.wheel);
        if across != 0 || along != 0 {
            self.sync_modifiers(&screen, event.modifiers);
            if let Some((x, y)) = self.picture_at(event.position, window) {
                // GPUI counts a leftward scroll as positive; the remote counts rightward so.
                screen.scroll(x, y, -across, along);
            }
        }
        cx.stop_propagation();
    }
}

/// What to tell the user when the server would not start a stream.
fn connect_message(error: &muxy_client::ClientError) -> String {
    match error {
        muxy_client::ClientError::Server(reply) if !reply.message.trim().is_empty() => {
            reply.message.clone()
        }
        other => other.to_string(),
    }
}

/// The picture as a texture, for pictures of plain pixels.
fn texture(frame: &atur_screen::Frame) -> Option<Arc<RenderImage>> {
    match &frame.pixels {
        atur_screen::Pixels::Bgra(bytes) => {
            let expected = usize::try_from(frame.width)
                .ok()?
                .checked_mul(usize::try_from(frame.height).ok()?)?
                .checked_mul(4)?;
            if bytes.len() != expected {
                return None;
            }
            let buffer = image::RgbaImage::from_raw(frame.width, frame.height, bytes.to_vec())?;
            Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
        }
        #[cfg(target_os = "macos")]
        atur_screen::Pixels::Surface(_) => None,
    }
}

/// Draws the picture in `rect`, a rectangle in the window.
fn paint_picture(shown: &Shown, rect: Bounds<Pixels>, window: &mut Window) {
    match &shown.frame.pixels {
        atur_screen::Pixels::Bgra(_) => {
            if let Some(image) = &shown.image {
                let _ =
                    window.paint_image(rect, gpui::Corners::all(px(0.0)), image.clone(), 0, false);
            }
        }
        #[cfg(target_os = "macos")]
        atur_screen::Pixels::Surface(surface) => {
            window.paint_surface(rect, surface.buffer().clone());
        }
    }
}

impl Render for ScreenView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let display = self.display();
        let picture = matches!(display, Display::Picture);
        let top = if cfg!(target_os = "macos") {
            self.metrics.title_bar_height()
        } else {
            px(0.0)
        };
        let content = match display {
            Display::Picture => self.picture_area(top, cx),
            Display::Connecting => self.status_area(top, tr!("Connecting…"), None, cx),
            Display::Waiting => self.status_area(top, tr!("Waiting for the screen…"), None, cx),
            Display::Failed(message) => {
                self.status_area(top, tr!("Can't show the screen"), Some(message), cx)
            }
        };
        div()
            .id("screen-view")
            .debug_selector(|| "screen-view".into())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(if picture {
                gpui::black()
            } else {
                self.theme.bg
            })
            .key_context("ScreenView")
            .track_focus(&self.focus)
            .on_key_up(cx.listener(|view, event: &gpui::KeyUpEvent, _, _| {
                view.key_up(&event.keystroke);
            }))
            .on_modifiers_changed(cx.listener(
                |view, event: &gpui::ModifiersChangedEvent, _, cx| {
                    if let Phase::Live(screen) = &view.phase {
                        let screen = screen.clone();
                        view.sync_modifiers(&screen, event.modifiers);
                    }
                    cx.stop_propagation();
                },
            ))
            .child(content)
            .when(self.toolbar, |view| view.child(self.toolbar_area(top, cx)))
    }
}

impl ScreenView {
    /// The remote desktop, with the mouse mapped onto it.
    fn picture_area(&self, top: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let shown = self.shown.clone();
        let viewport = Rc::clone(&self.viewport);
        let (fit, pan) = (self.fit, self.pan);
        let mut area = div()
            .id("screen-picture")
            .absolute()
            .top(top)
            .left_0()
            .right_0()
            .bottom_0()
            .overflow_hidden()
            .cursor(CursorStyle::None);
        for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
            area = area
                .on_mouse_down(button, cx.listener(Self::mouse_down))
                .on_mouse_up(button, cx.listener(Self::mouse_up))
                .on_mouse_up_out(button, cx.listener(Self::mouse_up));
        }
        area.on_mouse_move(cx.listener(Self::mouse_move))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .child(
                canvas(
                    move |bounds, _, _| viewport.set(bounds),
                    move |bounds, (), window, _| {
                        let Some(shown) = &shown else {
                            return;
                        };
                        let view = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                        let picture = (shown.frame.width, shown.frame.height);
                        if let Some(rect) =
                            geometry::placement(fit, view, picture, window.scale_factor(), pan)
                        {
                            let rect = Bounds::new(
                                point(bounds.origin.x + px(rect.x), bounds.origin.y + px(rect.y)),
                                size(px(rect.width), px(rect.height)),
                            );
                            paint_picture(shown, rect, window);
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }

    /// A message in the middle of the view, with what to do about it.
    fn status_area(
        &self,
        top: Pixels,
        title: SharedString,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (theme, m) = (&self.theme, self.metrics);
        let failed = detail.is_some();
        div()
            .absolute()
            .top(top)
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(m.spacing5())
            .text_color(theme.fg)
            .child(
                div()
                    .text_size(m.font_headline())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .children(detail.map(|detail| {
                div()
                    .max_w(m.scaled(420.0))
                    .text_center()
                    .text_size(m.font_body())
                    .text_color(theme.fg_muted)
                    .child(detail)
            }))
            .when(failed, |area| {
                area.child(
                    div()
                        .flex()
                        .gap(m.spacing3())
                        .child(
                            self.tool_button("screen-retry", tr!("Retry connection"), false)
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(ScreenViewEvent::Retry)),
                                ),
                        )
                        .child(
                            self.tool_button("screen-leave", tr!("Close"), false)
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(ScreenViewEvent::Close)),
                                ),
                        ),
                )
            })
            .into_any_element()
    }

    fn tool_button(
        &self,
        id: &'static str,
        label: SharedString,
        active: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let (theme, m) = (&self.theme, self.metrics);
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(m.control_medium())
            .px(m.spacing4())
            .rounded(m.radius_sm())
            .text_size(m.font_footnote())
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .when(active, |button| {
                button.bg(theme.accent).text_color(theme.accent_foreground)
            })
            .when(!active, |button| {
                button
                    .bg(theme.surface)
                    .text_color(theme.fg)
                    .hover(|hover| hover.bg(theme.hover))
            })
            .child(label)
    }

    /// The small bar over the picture: fit, view only, statistics, close.
    fn toolbar_area(&self, top: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let (theme, m) = (self.theme.clone(), self.metrics);
        let hint = tr!(
            "Press %@ to show or hide this toolbar.",
            keyboard::chord_label(cfg!(target_os = "macos"))
        );
        let fit_label = match self.fit {
            Fit::Window => tr!("Fit"),
            Fit::Actual => SharedString::from("1:1"),
        };
        let bar = div()
            .id("screen-toolbar")
            .debug_selector(|| "screen-toolbar".into())
            .occlude()
            .flex()
            .items_center()
            .gap(m.spacing3())
            .p(m.spacing2())
            .rounded(m.radius_lg())
            .bg(theme.bg)
            .border_1()
            .border_color(theme.border)
            .text_size(m.font_footnote())
            .text_color(theme.fg_muted)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .tooltip({
                let theme = theme.clone();
                move |_, cx| {
                    cx.new(|_| {
                        Tooltip::new(
                            hint.clone(),
                            theme.raised(),
                            theme.fg,
                            theme.border,
                            theme.bg,
                        )
                    })
                    .into()
                }
            })
            .child(
                self.tool_button("screen-fit", fit_label, false)
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.fit = view.fit.toggled();
                        cx.notify();
                    })),
            )
            .child(
                self.tool_button("screen-view-only", tr!("View only"), self.view_only)
                    .on_click(cx.listener(|view, _, _, cx| {
                        let view_only = !view.view_only;
                        view.set_view_only(view_only, cx);
                    })),
            )
            .child(div().px(m.spacing2()).child(stats_label(&self.stats)))
            .child(
                self.tool_button("screen-close", tr!("Close"), false)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ScreenViewEvent::Close))),
            );
        div()
            .absolute()
            .top(top + m.spacing4())
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(bar)
            .into_any_element()
    }
}

/// The codec and how it is doing, such as `h264 (videotoolbox) · 60 fps · 3200 kbps`.
fn stats_label(stats: &ScreenStats) -> String {
    if stats.codec.is_empty() {
        return String::new();
    }
    format!(
        "{} · {:.0} fps · {:.0} kbps",
        stats.codec, stats.fps, stats.kbps
    )
}

#[cfg(test)]
mod tests;
