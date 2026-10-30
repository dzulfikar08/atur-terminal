//! The Screen view of another computer's server. It is an overlay, so it is
//! never saved with the layout; it ends with the server's connection.

use std::rc::Rc;

use gpui::{AppContext, Context, Entity, Focusable, Window};
use muxy_app_core::ServerId;
use muxy_ui::command_palette::{Command, Registry};
use muxy_ui::tr;

use super::AppModel;
use crate::views::command_palette::Handler;
use crate::views::overlays::Overlay;
use crate::views::screen::{self, ScreenView, ScreenViewEvent};

impl AppModel {
    /// The server's second connection, when the server can be viewed: it is
    /// another computer's, connected, and offers screen sharing.
    fn screen_client(&self, server: ServerId) -> Option<muxy_client::Client> {
        let client = self.extensions.client(server)?;
        screen::offered(
            !server.is_local(),
            self.ready(server),
            client.supports(muxy_protocol::Feature::SCREEN),
        )
        .then_some(client)
    }

    /// Whether `server`'s screen can be viewed now.
    pub(crate) fn screen_available(&self, server: ServerId) -> bool {
        self.screen_client(server).is_some()
    }

    /// The servers whose screen can be viewed now, with their names.
    pub(crate) fn screen_servers(&self) -> Vec<(ServerId, String)> {
        self.settings
            .servers
            .iter()
            .filter(|entry| self.screen_available(entry.id))
            .map(|entry| (entry.id, entry.name.clone()))
            .collect()
    }

    /// One palette command, "Screen: name", for each server that can be viewed.
    pub(crate) fn register_screen_commands(&self, registry: &mut Registry<Handler>) {
        for (server, name) in self.screen_servers() {
            let open: Handler = Rc::new(move |model, window, cx| {
                model.open_screen(server, window, cx);
            });
            registry.register(
                Command::new(format!("screen-{server}"), tr!("Screen: %@", &name), open)
                    .keywords("Screen Remote Desktop Mirror View"),
            );
        }
    }

    /// Why `server`'s screen can't be viewed, for the user.
    fn screen_unavailable(&mut self, server: ServerId, cx: &mut Context<Self>) -> String {
        let name = self.server_label(server);
        if !self.ready(server) {
            tr!("%@ isn't connected.", &name).to_string()
        } else if !server.is_local() && self.extensions.client(server).is_none() {
            self.extension_client(server, cx);
            tr!("Still connecting to %@. Try again in a moment.", &name).to_string()
        } else {
            tr!("%@ can't share its screen.", &name).to_string()
        }
    }

    /// Shows `server`'s screen over the window.
    pub(crate) fn open_screen(
        &mut self,
        server: ServerId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_prompt.is_some() {
            return;
        }
        let Some(client) = self.screen_client(server) else {
            let reason = self.screen_unavailable(server, cx);
            self.show_toast(tr!("Screen view unavailable").to_string(), Some(reason), cx);
            return;
        };
        self.dismiss_overlay(cx);
        let name = self.server_label(server);
        let (theme, metrics) = (self.theme.clone(), self.metrics);
        let view = cx.new(|cx| ScreenView::new(server, name, theme, metrics, window, cx));
        view.update(cx, |view, cx| view.connect(client, cx));
        self.overlay_subscription = Some(cx.subscribe_in(
            &view,
            window,
            |model, view, event: &ScreenViewEvent, _, cx| match event {
                ScreenViewEvent::Close => model.dismiss_overlay(cx),
                ScreenViewEvent::Retry => model.retry_screen(view, cx),
            },
        ));
        view.focus_handle(cx).focus(window);
        self.overlay = Some(Overlay::Screen(view));
        cx.notify();
    }

    /// Connects the view again with the server's connection of now.
    fn retry_screen(&mut self, view: &Entity<ScreenView>, cx: &mut Context<Self>) {
        let server = view.read(cx).server;
        if let Some(client) = self.screen_client(server) {
            view.update(cx, |view, cx| view.connect(client, cx));
        } else {
            let reason = self.screen_unavailable(server, cx);
            view.update(cx, |view, cx| view.fail(reason, cx));
        }
    }

    /// Closes the Screen view when its server is no longer connected.
    pub(crate) fn sync_screen(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::Screen(view)) = &self.overlay else {
            return;
        };
        let (server, name) = {
            let view = view.read(cx);
            (view.server, view.name.clone())
        };
        if self.settings.server(server).is_some() && self.ready(server) {
            return;
        }
        self.dismiss_overlay(cx);
        self.show_toast(
            tr!("Screen view closed").to_string(),
            Some(tr!("%@ isn't connected.", &name).to_string()),
            cx,
        );
    }
}
