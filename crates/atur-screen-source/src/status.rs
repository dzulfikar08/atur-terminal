//! What `atur-term-screen` reports in its status file.
//!
//! The program rewrites the file (write to a sibling, then rename) at every
//! change, so a reader sees either the old or the new content, never a mix.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Where the program is in its life.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Started, not yet listening.
    Starting,
    /// Listening on the socket and able to capture.
    Ready,
    /// Screen Recording is not allowed for the program; nothing can be shared.
    NeedsPermission,
    /// Could not start for another reason, given in `message`.
    Failed,
    /// Stopped after having been ready; `message` says why, when it is known.
    Ended,
}

/// The content of the status file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Status {
    pub state: State,
    /// Process number of the program.
    pub pid: u32,
    /// Whether Screen Recording is allowed; `None` while not known.
    pub screen_recording: Option<bool>,
    /// Whether Accessibility (input) is allowed; `None` while not known.
    /// Without it the screen is shown but mouse and keyboard do nothing.
    pub accessibility: Option<bool>,
    /// A sentence for the user when something is wrong, otherwise empty.
    pub message: String,
    /// Size of the shared picture in pixels, once ready.
    pub width: u32,
    pub height: u32,
}

impl Status {
    #[must_use]
    pub fn new(state: State, pid: u32) -> Self {
        Self {
            state,
            pid,
            screen_recording: None,
            accessibility: None,
            message: String::new(),
            width: 0,
            height: 0,
        }
    }

    /// Writes the status to `path` atomically.
    ///
    /// # Errors
    /// When the file or its sibling cannot be written.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let mut sibling = path.as_os_str().to_owned();
        sibling.push(".tmp");
        let sibling = std::path::PathBuf::from(sibling);
        let text = serde_json::to_vec(self).map_err(io::Error::other)?;
        fs::write(&sibling, text)?;
        fs::rename(&sibling, path)
    }

    /// Reads the status at `path`; `None` when there is none yet or it is damaged.
    #[must_use]
    pub fn read(path: &Path) -> Option<Self> {
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_survives_the_file() {
        let folder = std::env::temp_dir().join(format!("atur-status-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("status");
        assert_eq!(Status::read(&path), None);
        let mut status = Status::new(State::NeedsPermission, 7);
        status.screen_recording = Some(false);
        status.message = "allow it".into();
        status.write(&path).unwrap();
        assert_eq!(Status::read(&path), Some(status));
        assert!(!folder.join("status.tmp").exists());
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(Status::read(&path), None);
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn states_are_spelled_in_kebab_case() {
        let text = serde_json::to_string(&Status::new(State::NeedsPermission, 1)).unwrap();
        assert!(text.contains(r#""state":"needs-permission""#), "{text}");
    }
}
