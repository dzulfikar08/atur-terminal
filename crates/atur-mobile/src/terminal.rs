use std::sync::{Arc, Mutex, PoisonError};

use muxy_client::Client;
use muxy_protocol::Size;

use crate::MobileError;
use crate::keys::{self, Key, Modifiers};
use crate::mouse::{self, MouseButton, ScrollDirection};
use crate::records::Screen;
use crate::scrollback::Scrollback;
use crate::terminals::{Terminals, View};

/// One attached terminal session.
#[derive(uniffi::Object)]
pub struct Terminal {
    pub(crate) client: Client,
    pub(crate) view: Arc<View>,
    pub(crate) terminals: Arc<Terminals>,
    /// The grid last told to the server, which starts as the one attached with.
    pub(crate) told: Mutex<Size>,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Terminal")
            .field("session", &self.view.session)
            .finish_non_exhaustive()
    }
}

#[uniffi::export]
impl Terminal {
    pub fn session_id(&self) -> u64 {
        self.view.session.get()
    }

    /// The visible screen; call again after a `ScreenChanged` event.
    pub fn screen(&self) -> Screen {
        let content = self.view.lock();
        Screen::new(
            &content.grid,
            &content.title,
            &content.directory.0,
            content.input,
        )
    }

    /// Copies the newest history rows and the screen for scrolling back; new
    /// output keeps updating `screen` but not the copy.
    pub fn scrollback(&self, max_rows: u16) -> Result<Arc<Scrollback>, MobileError> {
        Scrollback::take(&self.client, self.view.channel, max_rows).map(Arc::new)
    }

    pub fn send_input(&self, bytes: Vec<u8>) -> Result<(), MobileError> {
        Ok(self.client.send_input(self.view.channel, &bytes)?)
    }

    pub fn send_key(&self, key: Key, modifiers: Modifiers) -> Result<(), MobileError> {
        let modes = self.view.lock().grid.modes;
        match keys::encode(&key, modifiers, modes) {
            Some(bytes) => self.send_input(bytes),
            None => Ok(()),
        }
    }

    /// Sends text as one paste, marked as such when the program asks for it.
    pub fn paste(&self, text: String) -> Result<(), MobileError> {
        let bracketed = self.view.lock().grid.modes.bracketed_paste;
        self.send_input(keys::paste(&text, bracketed))
    }

    /// Clicks at a cell: a press, then a release. The program sees it only
    /// while `Screen::mouse_tracking` is on.
    pub fn click(
        &self,
        button: MouseButton,
        row: u16,
        column: u16,
        modifiers: Modifiers,
    ) -> Result<(), MobileError> {
        for event in mouse::click(button, row, column, modifiers) {
            self.client.send_mouse(self.view.channel, event)?;
        }
        Ok(())
    }

    /// Turns the mouse wheel one step at a cell. The program sees it only
    /// while `Screen::mouse_tracking` or `Screen::alternate_scroll` is on.
    pub fn scroll(
        &self,
        direction: ScrollDirection,
        row: u16,
        column: u16,
    ) -> Result<(), MobileError> {
        let event = mouse::scroll(direction, row, column);
        Ok(self.client.send_mouse(self.view.channel, event)?)
    }

    /// Tells the server the grid this device's view has, when it is not the one the server
    /// was told last. The session takes that size only while this device is the one
    /// typing: a phone that has not typed yet only watches, and follows the size the
    /// session has. The screen changes size when the server's answer arrives.
    pub fn resize(&self, columns: u16, rows: u16) -> Result<(), MobileError> {
        let size = Size {
            cols: columns,
            rows,
        };
        if *self.told.lock().unwrap_or_else(PoisonError::into_inner) == size {
            return Ok(());
        }
        self.client.resize(self.view.channel, size)?;
        *self.told.lock().unwrap_or_else(PoisonError::into_inner) = size;
        Ok(())
    }

    /// Stops receiving this terminal; the session keeps running.
    pub fn detach(&self) -> Result<(), MobileError> {
        self.terminals.remove(self.view.channel);
        let detached = self.client.detach(self.view.channel);
        self.terminals.track(&self.client);
        Ok(detached?)
    }
}
