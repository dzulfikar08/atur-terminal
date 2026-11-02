//! How a server starts, watches and stops the program that shares this Mac's
//! screen, and what that program reports back.
//!
//! The program is `atur-term-screen`, from the `atur-screen-host` crate. It
//! cannot run inside the server: macOS attributes Screen Recording and
//! Accessibility permission to the application a process belongs to, and a
//! server started over ssh belongs to `sshd`, which the user cannot sensibly
//! allow. The program is therefore a small application of its own, inside the
//! app bundle, and is started through Launch Services so that it is its own
//! responsible application. Everything crosses to it as arguments and a
//! status file; the pictures and input travel over a private Unix socket
//! speaking RFB, like `wayvnc` on Linux.

pub mod launch;
pub mod status;
