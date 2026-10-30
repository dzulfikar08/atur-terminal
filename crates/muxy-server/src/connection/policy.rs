use muxy_protocol::{ClientKind, DeviceId, ErrorCode, RequestBody};

use crate::ServerError;

/// What a connection may ask for. A paired device can already reach a shell, so
/// these limits guard access management and the server's lifecycle, not data.
/// `device` is the paired device on the other end of a network connection, and
/// `screen_allowed` says whether such a device may view the screen.
pub(super) fn permit(
    device: Option<DeviceId>,
    screen_allowed: impl FnOnce(DeviceId) -> bool,
    body: &RequestBody,
) -> Result<(), ServerError> {
    let device_connection = device.is_some();
    match body {
        RequestBody::Authenticate(_) | RequestBody::Pair(_) => Err(ServerError::new(
            ErrorCode::BadRequest,
            "authentication is only the first request of a network connection",
        )),
        RequestBody::IdentifyClient(ClientKind::Unrecognized(_)) => Err(ServerError::new(
            ErrorCode::Unsupported,
            "this server doesn't know the client kind",
        )),
        RequestBody::ReadRemoteAccess
        | RequestBody::WriteRemoteAccess(_)
        | RequestBody::StartPairing
        | RequestBody::StartPairingWithHosts(_)
        | RequestBody::CancelPairing
        | RequestBody::RevokeDevice(_)
        | RequestBody::SetDeviceScreen(..)
        | RequestBody::StopServer
        | RequestBody::StopServerIfIdle
        | RequestBody::WriteServerSettings(_)
        | RequestBody::Exec(_)
        | RequestBody::CancelExec(_)
        | RequestBody::IdentifyClient(ClientKind::Desktop | ClientKind::Tui | ClientKind::Cli) => {
            local_only(device_connection)
        }
        RequestBody::OpenScreen => match device {
            Some(device) if !screen_allowed(device) => {
                Err(ServerError::new(ErrorCode::Unauthorized, SCREEN_REFUSAL))
            }
            _ => Ok(()),
        },
        RequestBody::OfferUdp => {
            network_only(device_connection, "UDP is only offered to paired devices")
        }
        RequestBody::ReadAddresses => network_only(
            device_connection,
            "addresses are only told to paired devices",
        ),
        RequestBody::IdentifyClient(ClientKind::Mobile)
        | RequestBody::CancelCreation(_)
        | RequestBody::ListSessions
        | RequestBody::ReadCatalog { .. }
        | RequestBody::MutateProject(_)
        | RequestBody::ListProjectSessions { .. }
        | RequestBody::CreateSession { .. }
        | RequestBody::EndSession(_)
        | RequestBody::Attach { .. }
        | RequestBody::AttachWithoutResize(_)
        | RequestBody::Detach(_)
        | RequestBody::Resize { .. }
        | RequestBody::ClearScreen(_)
        | RequestBody::Ping
        | RequestBody::ReadSavedScreen(_)
        | RequestBody::DiscardSession(_)
        | RequestBody::HistoryPage { .. }
        | RequestBody::SavedHistoryPage { .. }
        | RequestBody::Search { .. }
        | RequestBody::SetTerminalColors(_)
        | RequestBody::ReadServerSettings
        | RequestBody::SyncSessionReferences { .. }
        | RequestBody::CloseSession { .. }
        | RequestBody::Git(_)
        | RequestBody::ReadActivity
        | RequestBody::AcknowledgeActivity(_)
        | RequestBody::ClaimActivity(_)
        | RequestBody::Files(_)
        | RequestBody::ListFolders(_)
        | RequestBody::Upload(_)
        | RequestBody::WriteInput { .. } => Ok(()),
    }
}

/// What a paired device is told when it may not view the screen; phones show it as is.
const SCREEN_REFUSAL: &str = "Screen sharing is not allowed for this phone. Allow it under Settings, Phones on the computer, or run `atur-term mobile screen <device> on`.";

fn local_only(device: bool) -> Result<(), ServerError> {
    if device {
        Err(ServerError::new(
            ErrorCode::Unauthorized,
            "not available to paired devices",
        ))
    } else {
        Ok(())
    }
}

/// A local connection has a socket of its own; what a phone needs from the
/// network is for paired devices.
fn network_only(device: bool, refusal: &str) -> Result<(), ServerError> {
    if device {
        Ok(())
    } else {
        Err(ServerError::new(ErrorCode::BadRequest, refusal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permitted(device: Option<DeviceId>, allowed: bool, body: &RequestBody) -> Option<ErrorCode> {
        permit(device, |_| allowed, body)
            .err()
            .map(|error| error.code())
    }

    #[test]
    fn a_paired_device_opens_the_screen_only_when_allowed() {
        let phone = Some(DeviceId::from_u128(1));
        assert_eq!(permitted(None, false, &RequestBody::OpenScreen), None);
        assert_eq!(permitted(phone, true, &RequestBody::OpenScreen), None);
        assert_eq!(
            permitted(phone, false, &RequestBody::OpenScreen),
            Some(ErrorCode::Unauthorized)
        );
        let refusal = permit(phone, |_| false, &RequestBody::OpenScreen)
            .expect_err("a phone without the permission is refused");
        assert_eq!(refusal.message(), SCREEN_REFUSAL);
    }

    #[test]
    fn only_local_connections_change_the_screen_permission() {
        let phone = Some(DeviceId::from_u128(1));
        let body = RequestBody::SetDeviceScreen(DeviceId::from_u128(1), true);
        assert_eq!(permitted(None, false, &body), None);
        assert_eq!(permitted(phone, true, &body), Some(ErrorCode::Unauthorized));
    }
}
