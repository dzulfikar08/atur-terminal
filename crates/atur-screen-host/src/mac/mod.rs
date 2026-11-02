//! Everything that touches macOS: capture, encoding, input, the pasteboard and
//! permissions, and the program that ties them to the RFB server.

pub mod capture;
pub mod display;
pub mod encoder;
pub mod host;
pub mod input;
pub mod pixel;
pub mod run;
