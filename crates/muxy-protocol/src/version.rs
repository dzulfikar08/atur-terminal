use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize, Encode, Decode,
)]
#[serde(transparent)]
#[cbor(transparent)]
pub struct Version(#[n(0)] pub u16);

/// Builds before V2. Only recognised so they can be told to update.
pub const V1: Version = Version(1);
/// Fields and variants carry numbers, so additive changes keep the version.
pub const V2: Version = Version(2);
/// Bumped only by a breaking change.
pub const CURRENT: Version = V2;
/// During beta a build speaks only its current version.
pub const SUPPORTED: &[Version] = &[CURRENT];

/// Something a server can do that a client checks before offering it.
///
/// Add one only when a client must change its UI for older servers. Numbers
/// are permanent.
#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize, Encode, Decode,
)]
#[serde(transparent)]
#[cbor(transparent)]
pub struct Feature(#[n(0)] pub u16);

impl Feature {
    /// Server-side key encoding, paste framing, focus reports and ordered screen clearing.
    pub const TERMINAL_INPUT: Self = Self(1);
    /// `RequestBody::OfferUdp`: a paired connection can ask for a UDP port to move to.
    pub const UDP_OFFER: Self = Self(2);
    /// `RequestBody::ReadAddresses`: a paired connection can ask for the server's current addresses.
    pub const SERVER_ADDRESSES: Self = Self(3);
    /// `RequestBody::OpenScreen`: a local or ssh connection can mirror the server's desktop.
    /// Only servers that can capture a Wayland desktop list it, and they list it
    /// from the handshake, not in [`FEATURES`].
    pub const SCREEN: Self = Self(4);
    /// `FilesAction::ReadRange` and `FilesAction::WriteChunk`: files up to
    /// `MAX_CHUNKED_FILE_BYTES`, read and written in pieces.
    pub const FILE_CHUNKS: Self = Self(5);
}

/// Features this build's server supports.
pub const FEATURES: &[Feature] = &[
    Feature::TERMINAL_INPUT,
    Feature::UDP_OFFER,
    Feature::SERVER_ADDRESSES,
    Feature::FILE_CHUNKS,
];
