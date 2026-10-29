//! An encrypted byte stream over UDP that survives loss, jitter, address
//! changes, and long silences, in the manner of mosh.
//!
//! ssh carries one random secret to the client. Each datagram is sealed with
//! keys derived from it, so no handshake packet exists to attack and a packet
//! that does not open is dropped without a reply. Above the datagrams runs a
//! reliable ordered stream with timers made for bad links.
//!
//! The server end learns where the client is from the newest authentic
//! packet, so a client that changes network keeps its session.

mod connection;
mod crypto;
mod endpoint;

pub use connection::Stats;
pub use crypto::Secret;
pub use endpoint::{Options, UdpServer, UdpStatus, UdpStream, connect, parse_ports};
