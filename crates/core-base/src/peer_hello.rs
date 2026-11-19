//! What a session learned about its peer from the greeting.
//!
//! Defined here, below the session that reads it, because every module is
//! built from it (`SessionCtx::peer`) and modules live in crates of their own.

use cursio_protocol::{DeviceKind, ModuleAdvert, Negotiated, PeerId};

/// Identity and capabilities learned from the peer's [`Hello`](cursio_protocol::Hello).
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
    /// The peer's screens on the shared desk; see `cursio_core::desk`.
    pub layout: cursio_protocol::Layout,
    /// What the peer can carry for the mesh, and what quality it wants from a
    /// duty the mesh places. See [`cursio_protocol::ServiceOffer`].
    pub service: cursio_protocol::ServiceOffer,
    /// The peer's power and network state as of the greeting. Superseded by
    /// every [`Message::NodeStatus`](cursio_protocol::Message::NodeStatus) the session then reports.
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
