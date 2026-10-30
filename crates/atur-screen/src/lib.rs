//! Screen mirroring client: an RFB 3.8 viewer over any byte transport.
//!
//! * [`ScreenTransport`] is the byte pipe to the server (wayvnc); the app implements it.
//! * [`Screen::start`] spawns a reader thread that only blocks in `recv` and hands chunks over
//!   a small bounded channel, a processor thread that owns the protocol state machine, and a
//!   timer thread for coalesced pointer moves and rate-limited wake-ups.
//! * `rfb` is a sans-IO state machine (security None, ServerInit, Raw, CopyRect, ZRLE,
//!   H.264, DesktopSize, ExtendedDesktopSize, ContinuousUpdates, Fence, extended clipboard):
//!   bytes in any split go in, framebuffer changes, events and reply bytes come out.
//! * `decode` holds the H.264 decoders (VideoToolbox, or an `ffmpeg` process); the session
//!   advertises H.264 only when a decoder exists, ZRLE works without one.
//! * The framebuffer is a persistent BGRA buffer; every finished update is copied into one
//!   [`Frame`] in a single-slot mailbox, so video is never queued and a slow UI only
//!   drops frames. Because the reader waits when the mailbox of chunks is full, the transport
//!   window closes and the server sends only the latest damage.
//! * Input goes straight to the transport from the caller's thread under one write lock, so
//!   keys and buttons keep their order; only pointer moves are coalesced (at most one per 8 ms).
//! * [`keys::keysym`] maps GPUI key names to X11 keysyms.
//! * Every wire length is bounded; protocol errors close the session with a readable reason.

pub mod decode;
mod frame;
pub mod keys;
mod rfb;
mod session;

#[cfg(target_os = "macos")]
pub use frame::Surface;
pub use frame::{Frame, Pixels};
pub use session::{
    PointerButtons, Screen, ScreenInfo, ScreenOptions, ScreenState, ScreenStats, ScreenTransport,
};
