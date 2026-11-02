//! The Mac as the RFB server's host: H.264 from `VideoToolbox`, input through
//! `CGEvent`, the clipboard through the pasteboard.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::capture::pasteboard;
use super::encoder::VideoToolboxEncoder;
use super::input::Injector;
use crate::encoder_config::EncoderConfig;
use crate::serve::{Broadcast, Encoder, Host};

pub struct MacHost {
    name: String,
    fps: u32,
    broadcast: Arc<Broadcast>,
    /// `None` for the test pattern, which has no screen to drive.
    injector: Option<Mutex<Injector>>,
    /// Whether Accessibility is allowed; refreshed while the program runs.
    trusted: AtomicBool,
}

impl fmt::Debug for MacHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MacHost")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl MacHost {
    #[must_use]
    pub fn new(
        name: String,
        fps: u32,
        injector: Option<Injector>,
        trusted: bool,
        broadcast: Arc<Broadcast>,
    ) -> Self {
        Self {
            name,
            fps,
            broadcast,
            injector: injector.map(Mutex::new),
            trusted: AtomicBool::new(trusted),
        }
    }

    /// Records whether Accessibility is allowed now.
    pub fn set_trusted(&self, trusted: bool) {
        self.trusted.store(trusted, Ordering::Release);
    }
}

impl Host for MacHost {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn encoder(&self, width: u32, height: u32) -> Option<Box<dyn Encoder>> {
        match VideoToolboxEncoder::new(EncoderConfig::for_size(width, height, self.fps)) {
            Ok(encoder) => Some(Box::new(encoder)),
            Err(reason) => {
                log::warn!("no H.264 encoder for {width}x{height}: {reason}");
                None
            }
        }
    }

    fn input_allowed(&self) -> bool {
        self.injector.is_some() && self.trusted.load(Ordering::Acquire)
    }

    fn pointer(&self, buttons: u8, x: u16, y: u16) {
        let Some(geometry) = self.broadcast.geometry() else {
            return;
        };
        if let Some(injector) = &self.injector {
            injector
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pointer(&geometry, buttons, x, y);
        }
    }

    fn key(&self, keysym: u32, down: bool) {
        if let Some(injector) = &self.injector {
            injector
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .key(keysym, down);
        }
    }

    fn release_all(&self) {
        if let Some(injector) = &self.injector {
            injector
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .release_all();
        }
    }

    fn clipboard_text(&self) -> Option<String> {
        pasteboard::text()
    }

    fn clipboard_changes(&self) -> u64 {
        pasteboard::change_count()
    }

    fn set_clipboard_text(&self, text: &str) -> u64 {
        pasteboard::set_text(text)
    }
}
