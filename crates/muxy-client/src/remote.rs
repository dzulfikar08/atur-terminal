//! Connections to a server over the network, as a paired device.

use std::io;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use muxy_protocol::transport::udp::Secret;
use muxy_protocol::transport::{ByteStream, tls};
use muxy_protocol::{
    DeviceCredential, DeviceId, ErrorCode, ErrorReply, Feature, PairRequest, Paired, PairingInvite,
    PairingOffer, RemoteAccessSettings, RemoteAccessState, ReplyBody, RequestBody, UdpOffer,
};

use crate::udp::{self, UdpSettings};
use crate::{Client, ClientError, Transport};

/// Budget for reaching and verifying one address of a server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Where a paired server can be reached, and the certificate it must present.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteEndpoint {
    pub hosts: Vec<String>,
    pub port: u16,
    pub fingerprint: [u8; 32],
}

impl From<&PairingInvite> for RemoteEndpoint {
    fn from(invite: &PairingInvite) -> Self {
        Self {
            hosts: invite.hosts.clone(),
            port: invite.port,
            fingerprint: invite.fingerprint,
        }
    }
}

impl Client {
    /// Connects as a paired device, trying each host in order.
    pub fn connect_remote(
        endpoint: &RemoteEndpoint,
        credential: DeviceCredential,
    ) -> Result<Self, ClientError> {
        Self::connect_remote_at(endpoint, credential).map(|(client, _)| client)
    }

    /// Like [`Client::connect_remote`], and also names the host that answered,
    /// so a second path to the same server can use the address that worked.
    pub fn connect_remote_at(
        endpoint: &RemoteEndpoint,
        credential: DeviceCredential,
    ) -> Result<(Self, String), ClientError> {
        let (stream, host) = connect(endpoint)?;
        Self::connect_remote_stream(stream, credential).map(|client| (client, host))
    }

    /// Says Hello and signs in as a paired device over a stream that already
    /// reaches the server: TLS, or UDP once an offer was claimed.
    pub fn connect_remote_stream(
        stream: Box<dyn ByteStream>,
        credential: DeviceCredential,
    ) -> Result<Self, ClientError> {
        let client = Self::from_stream_with_timeout(stream, CONNECT_TIMEOUT)?;
        match client.request(RequestBody::Authenticate(credential))? {
            ReplyBody::Authenticated => Ok(client),
            body => Err(ClientError::UnexpectedReply(Box::new(body))),
        }
    }

    /// Whether the connected server answers [`Client::request_udp`]. Older
    /// servers do not, and are never asked.
    pub fn supports_udp(&self) -> bool {
        self.supports(Feature::UDP_OFFER)
    }

    /// Asks the server for a UDP port and key. The server answers an error
    /// with code `Unsupported` when UDP is turned off there.
    pub fn request_udp(&self) -> Result<UdpOffer, ClientError> {
        match self.request(RequestBody::OfferUdp)? {
            ReplyBody::UdpOffer(offer) => Ok(offer),
            body => Err(ClientError::UnexpectedReply(Box::new(body))),
        }
    }

    /// Connects to a server's UDP `offer` at `host` and signs in as a paired
    /// device over it, with the settings' first-contact and idle times. Fails
    /// with an I/O error of kind `TimedOut` when nothing answers.
    pub fn connect_remote_udp(
        host: &str,
        offer: &UdpOffer,
        credential: DeviceCredential,
        settings: &UdpSettings,
    ) -> Result<Self, ClientError> {
        let addresses = resolve(host)?;
        let stream = udp::connect(
            &addresses,
            offer.port,
            &Secret::from_bytes(offer.key),
            settings,
        )?;
        Self::connect_remote_stream(stream, credential)
            .map(|client| client.with_transport(Transport::Udp))
    }

    /// Pairs with the server that issued the invite; the connection stays open.
    pub fn pair(invite: &PairingInvite, name: &str) -> Result<(Self, Paired), ClientError> {
        let (stream, _) = connect(&RemoteEndpoint::from(invite))?;
        let client = Self::from_stream_with_timeout(stream, CONNECT_TIMEOUT)?;
        match client.request(RequestBody::Pair(PairRequest {
            secret: invite.secret,
            name: name.into(),
        }))? {
            ReplyBody::Paired(paired) => Ok((client, paired)),
            body => Err(ClientError::UnexpectedReply(Box::new(body))),
        }
    }

    pub fn read_remote_access(&self) -> Result<RemoteAccessState, ClientError> {
        remote_access(self.request(RequestBody::ReadRemoteAccess)?)
    }

    pub fn write_remote_access(
        &self,
        settings: RemoteAccessSettings,
    ) -> Result<RemoteAccessState, ClientError> {
        remote_access(self.request(RequestBody::WriteRemoteAccess(settings))?)
    }

    pub fn start_pairing(&self) -> Result<PairingOffer, ClientError> {
        match self.request(RequestBody::StartPairing)? {
            ReplyBody::Pairing(offer) => Ok(offer),
            body => Err(ClientError::UnexpectedReply(Box::new(body))),
        }
    }

    /// Like [`Client::start_pairing`], but the link lists `hosts` before the
    /// addresses the server finds itself, for a server that phones reach by
    /// another name, such as behind NAT. Without hosts it sends the plain
    /// request, which older servers also know.
    pub fn start_pairing_with(&self, hosts: Vec<String>) -> Result<PairingOffer, ClientError> {
        if hosts.is_empty() {
            return self.start_pairing();
        }
        muxy_protocol::validate_pairing_hosts(&hosts).map_err(ClientError::Invalid)?;
        match self.request(RequestBody::StartPairingWithHosts(hosts)) {
            Ok(ReplyBody::Pairing(offer)) => Ok(offer),
            Ok(body) => Err(ClientError::UnexpectedReply(Box::new(body))),
            Err(ClientError::Server(error)) if error.code == ErrorCode::Unsupported => {
                Err(ClientError::Server(ErrorReply {
                    code: ErrorCode::Unsupported,
                    message:
                        "This server can't add addresses to a pairing code. Update Atur Terminal on it."
                            .into(),
                }))
            }
            Err(error) => Err(error),
        }
    }

    pub fn cancel_pairing(&self) -> Result<RemoteAccessState, ClientError> {
        remote_access(self.request(RequestBody::CancelPairing)?)
    }

    pub fn revoke_device(&self, device: DeviceId) -> Result<RemoteAccessState, ClientError> {
        remote_access(self.request(RequestBody::RevokeDevice(device))?)
    }
}

fn remote_access(body: ReplyBody) -> Result<RemoteAccessState, ClientError> {
    match body {
        ReplyBody::RemoteAccess(state) => Ok(state),
        body => Err(ClientError::UnexpectedReply(Box::new(body))),
    }
}

/// A host presenting another certificate is skipped, never trusted; the
/// mismatch is reported only if no host answers with the pinned one.
fn connect(endpoint: &RemoteEndpoint) -> Result<(Box<dyn ByteStream>, String), ClientError> {
    let mut mismatch = false;
    let mut failure = None;
    for host in &endpoint.hosts {
        match tls::connect(host, endpoint.port, endpoint.fingerprint, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok((stream, host.clone())),
            Err(error) => {
                mismatch |= tls::is_identity_mismatch(&error);
                failure = Some(error);
            }
        }
    }
    if mismatch {
        return Err(ClientError::IdentityMismatch);
    }
    Err(ClientError::Io(failure.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "the server has no known address")
    })))
}

/// The addresses a host name stands for, without duplicates.
fn resolve(host: &str) -> io::Result<Vec<IpAddr>> {
    let mut addresses = Vec::new();
    for address in (host, 0).to_socket_addrs()? {
        if !addresses.contains(&address.ip()) {
            addresses.push(address.ip());
        }
    }
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the server's name has no address",
        ));
    }
    Ok(addresses)
}
