//! The greeting both sides exchange before a session: protocol version,
//! identity, modules, position and state, and what kind of device this is.

use crate::session::SessionError;
use cursio_protocol::{
    DeviceKind, Hello, Message, ModuleAdvert, Negotiated, PeerId, PROTOCOL_VERSION,
};
use cursio_transport::Transport;

/// Identity and capabilities learned from the peer's [`Hello`].
#[derive(Debug, Clone)]
pub struct PeerHello {
    pub peer_id: PeerId,
    pub display_name: String,
    /// What sort of machine the peer is. Consulted before handing it the shared
    /// cursor — a phone has no screen edge to enter.
    pub device_kind: DeviceKind,
    /// Modules the peer advertised. Raw; use [`PeerHello::negotiate`] to decide
    /// what actually runs.
    pub modules: Vec<ModuleAdvert>,
    /// Where the peer stands in the layout, which is what decides the edge the
    /// cursor crosses to reach it.
    pub position: cursio_protocol::Position,
    /// The peer's screens on the shared desk; see [`crate::desk`].
    pub layout: cursio_protocol::Layout,
    /// What the peer can carry for the mesh, and what quality it wants from a
    /// duty the mesh places. See [`cursio_protocol::ServiceOffer`].
    pub service: cursio_protocol::ServiceOffer,
    /// The peer's power and network state as of the greeting. Superseded by
    /// every [`Message::NodeStatus`] the session then reports.
    pub status: cursio_protocol::NodeStatus,
    /// The sessions the peer is holding open, with anyone; see
    /// [`cursio_protocol::Hello::resumable`].
    pub resumable: Vec<cursio_protocol::SessionTag>,
}

impl PeerHello {
    /// Which of *our* modules run on this link, and why the rest don't.
    pub fn negotiate(&self, local: &[ModuleAdvert]) -> Negotiated {
        cursio_protocol::negotiate(local, &self.modules)
    }
}

/// Perform the opening [`Hello`] exchange and return the peer's identity.
///
/// Both sides send their own `Hello` and read the other's; the order is
/// symmetric because each `send` is independent of the peer's. A protocol
/// version mismatch is a hard error so incompatible builds fail fast.
#[allow(clippy::too_many_arguments)]
pub async fn handshake<T: Transport + ?Sized>(
    transport: &mut T,
    local: &crate::NodeIdentity,
    device_kind: DeviceKind,
    modules: &[ModuleAdvert],
    position: cursio_protocol::Position,
    layout: &cursio_protocol::Layout,
    service: cursio_protocol::ServiceOffer,
    status: cursio_protocol::NodeStatus,
    resumable: &[cursio_protocol::SessionTag],
) -> Result<PeerHello, SessionError> {
    handshake_as(
        PROTOCOL_VERSION,
        transport,
        local,
        device_kind,
        modules,
        position,
        layout,
        service,
        status,
        resumable,
    )
    .await
}

/// [`handshake`] claiming `protocol_version`, so a test can stand in for a
/// build that speaks another one.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn handshake_as<T: Transport + ?Sized>(
    protocol_version: u16,
    transport: &mut T,
    local: &crate::NodeIdentity,
    device_kind: DeviceKind,
    modules: &[ModuleAdvert],
    position: cursio_protocol::Position,
    layout: &cursio_protocol::Layout,
    service: cursio_protocol::ServiceOffer,
    status: cursio_protocol::NodeStatus,
    resumable: &[cursio_protocol::SessionTag],
) -> Result<PeerHello, SessionError> {
    // The preamble goes first and alone, in the frozen lane encoding, so the
    // peer learns our version from a message no build can fail to read — and
    // the greeting, whose shape does change, is only ever sent to a peer known
    // to understand it.
    let preamble = cursio_protocol::update::Preamble {
        protocol_version,
        peer_id: local.peer_id,
        build: crate::update::running_build(),
        platform: crate::update::platform(),
    };
    transport
        .send(&Message::Update(
            cursio_protocol::update::UpdateMsg::Preamble(preamble).encode(),
        ))
        .await?;
    match transport.recv().await? {
        Message::Update(bytes) => match cursio_protocol::update::UpdateMsg::decode(&bytes) {
            Ok(Some(cursio_protocol::update::UpdateMsg::Preamble(peer))) => {
                if peer.protocol_version != protocol_version {
                    return Err(SessionError::UpdateOnly(peer));
                }
            }
            _ => return Err(SessionError::UnexpectedMessage),
        },
        // A build from before the preamble opens with its greeting, whose
        // first field has always been the version.
        Message::Hello(old) => {
            return Err(SessionError::VersionMismatch {
                local: protocol_version,
                peer: old.protocol_version,
            })
        }
        _ => return Err(SessionError::UnexpectedMessage),
    }

    transport
        .send(&Message::Hello(Hello {
            protocol_version,
            peer_id: local.peer_id,
            display_name: local.display_name.clone(),
            device_kind,
            modules: modules.to_vec(),
            position,
            layout: layout.clone(),
            service,
            status,
            resumable: resumable.to_vec(),
        }))
        .await?;

    let hello = match transport.recv().await? {
        Message::Hello(h) => h,
        _ => return Err(SessionError::UnexpectedMessage),
    };

    if hello.protocol_version != protocol_version {
        return Err(SessionError::VersionMismatch {
            local: protocol_version,
            peer: hello.protocol_version,
        });
    }

    Ok(PeerHello {
        peer_id: hello.peer_id,
        display_name: hello.display_name,
        device_kind: hello.device_kind,
        modules: hello.modules,
        position: hello.position,
        layout: hello.layout,
        service: hello.service,
        status: hello.status,
        resumable: hello.resumable,
    })
}

/// What sort of machine *this* build runs on, as far as the cursor is concerned.
///
/// Desktop/laptop is not distinguishable without poking at hardware, and nothing
/// depends on the difference today — [`DeviceKind::takes_cursor`] treats them
/// alike.
///
/// A mobile build says so, and that one word decides how the whole session is
/// arranged: the peer keeps the cursor permanently, this side stays dormant,
/// and staying dormant is exactly what makes its input stream to the peer
/// rather than act locally. A phone that claimed to be a desktop would take the
/// cursor half the time and freeze the machine it was supposed to be driving.
#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn local_device_kind() -> DeviceKind {
    DeviceKind::Phone
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn local_device_kind() -> DeviceKind {
    DeviceKind::Desktop
}
