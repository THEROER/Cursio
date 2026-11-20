//! A session that outlives its connection: the channel's session
//! (`cursio_channel::session`) over Cursio's classes and lanes, as a
//! [`Transport`].
//!
//! Connections are only *paths* under it — one per lane through the router,
//! one per lane over a direct Wi-Fi link when the two machines share one, and
//! a Bluetooth standby that can carry every lane. See "A session outlives its
//! connection" and "Two paths, one mesh" in `docs/mesh-design.md`.

use async_trait::async_trait;
use cursio_channel::LaneId;
use cursio_protocol::{Message, SessionTag};

use crate::lane::Lane;
use crate::{Transport, TransportError, TransportRx, TransportTx};

pub use cursio_channel::session::{direct_lost, network_lost, HOLD};
pub use cursio_channel::{Path, Paths, ResumeHandle};

/// What a path runs over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// TCP over Wi-Fi or Ethernet, carrying the interactive lane.
    Network,
    /// RFCOMM to a paired device: the standby that carries every lane.
    Bluetooth,
    NetworkAudio,
    NetworkVideo,
    NetworkBulk,
}

impl PathKind {
    pub fn for_lane(lane: Lane) -> Self {
        match lane {
            Lane::Interactive => Self::Network,
            Lane::Audio => Self::NetworkAudio,
            Lane::Video => Self::NetworkVideo,
            Lane::Bulk => Self::NetworkBulk,
        }
    }
}

impl From<PathKind> for cursio_channel::PathKind {
    fn from(kind: PathKind) -> Self {
        let lane = |lane: Lane| cursio_channel::PathKind::Lane(LaneId::from(lane));
        match kind {
            PathKind::Network => lane(Lane::Interactive),
            PathKind::NetworkAudio => lane(Lane::Audio),
            PathKind::NetworkVideo => lane(Lane::Video),
            PathKind::NetworkBulk => lane(Lane::Bulk),
            PathKind::Bluetooth => cursio_channel::PathKind::Standby,
        }
    }
}

/// A [`Transport`] for a session that can change its connection.
pub struct ResumableLink(cursio_channel::ResumableLink<Message>);

impl ResumableLink {
    /// Start a session over `first`, known to both ends as `tag`. Spawns its
    /// tasks, so it must run inside the runtime the session will.
    pub fn new(first: Path, tag: SessionTag) -> (Self, ResumeHandle) {
        let (link, handle) = cursio_channel::ResumableLink::new(first, tag, crate::lane::classes());
        (Self(link), handle)
    }
}

#[async_trait]
impl Transport for ResumableLink {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        Ok(self.0.send(msg).await?)
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        Ok(self.0.recv().await?)
    }

    fn kind(&self) -> &'static str {
        "resumable"
    }

    fn split(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        self.split_prioritized()
    }

    fn split_prioritized(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        let (tx, rx) = self.0.split();
        (Box::new(tx), Box::new(ResumableRx(rx)))
    }
}

/// The session's receiving end: frames from every path, in order, once each.
pub struct ResumableRx(cursio_channel::ResumableRx<Message>);

#[async_trait]
impl TransportRx for ResumableRx {
    async fn recv(&mut self) -> Result<Message, TransportError> {
        Ok(self.0.recv().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::crypto::EncryptedTransport;
    use crate::pairing::Role;
    use crate::test_util::MemTransport;
    use cursio_protocol::InputEvent;
    use std::time::Duration;

    async fn path_pair(kind: PathKind) -> (Path, Path, SessionTag) {
        let (a, b) = MemTransport::pair();
        let (a, b) = tokio::join!(
            EncryptedTransport::establish(a, b"a shared secret", Role::Initiator),
            EncryptedTransport::establish(b, b"a shared secret", Role::Responder),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        let tag = a.session_tag();
        (a.into_path(kind), b.into_path(kind), tag)
    }

    struct End {
        tx: Box<dyn TransportTx>,
        rx: Box<dyn TransportRx>,
        handle: ResumeHandle,
    }

    async fn session() -> (End, End) {
        let (a, b, tag) = path_pair(PathKind::Network).await;
        let (link_a, handle_a) = ResumableLink::new(a, tag);
        let (link_b, handle_b) = ResumableLink::new(b, tag);
        for _ in 1..Lane::COUNT {
            let (pa, pb, _) = path_pair(PathKind::Network).await;
            let (ra, rb) = tokio::join!(handle_a.attach(pa), handle_b.attach(pb));
            ra.unwrap();
            rb.unwrap();
        }
        let (tx_a, rx_a) = Box::new(link_a).split_prioritized();
        let (tx_b, rx_b) = Box::new(link_b).split_prioritized();
        (
            End {
                tx: tx_a,
                rx: rx_a,
                handle: handle_a,
            },
            End {
                tx: tx_b,
                rx: rx_b,
                handle: handle_b,
            },
        )
    }

    fn key(n: u16) -> Message {
        Message::Input(InputEvent::Key {
            key: n,
            pressed: true,
        })
    }

    async fn next(end: &mut End) -> Message {
        tokio::time::timeout(Duration::from_secs(5), end.rx.recv())
            .await
            .expect("a message within five seconds")
            .unwrap()
    }

    async fn rejoin(a: &End, b: &End) {
        while !a.handle.network_complete() {
            let (pa, pb, _) = path_pair(PathKind::Network).await;
            let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
            ra.unwrap();
            rb.unwrap();
        }
    }

    /// Cursio's messages over the channel's session, every path a Noise
    /// connection carried as opaque messages: presses once and in order
    /// across a lost network, pointer motion from the gap not replayed.
    #[tokio::test]
    async fn presses_cross_a_lost_network_once_and_motion_from_the_gap_is_not_replayed() {
        let (mut a, mut b) = session().await;
        for n in 0..10 {
            a.tx.send(&key(n)).await.unwrap();
        }
        a.handle.network_lost();
        b.handle.network_lost();
        tokio::task::yield_now().await;
        assert!(a.tx.holding(), "a link with no path holds the session");
        for _ in 0..10 {
            a.tx.send(&Message::Input(InputEvent::PointerMotion {
                dx: 100,
                dy: 0,
            }))
            .await
            .unwrap();
        }
        a.tx.send(&key(10)).await.unwrap();
        rejoin(&a, &b).await;
        for n in 0..=10 {
            assert_eq!(next(&mut b).await, key(n));
        }
        assert!(!a.tx.holding());
        assert_eq!(a.tx.carrying_path(), Some("network"));
    }

    #[tokio::test]
    async fn a_bluetooth_standby_carries_input_and_names_itself_bluetooth() {
        let (mut a, mut b) = session().await;
        let (pa, pb, _) = path_pair(PathKind::Bluetooth).await;
        let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
        ra.unwrap();
        rb.unwrap();
        a.handle.release(PathKind::Network, "test");
        b.handle.release(PathKind::Network, "test");
        a.tx.send(&key(3)).await.unwrap();
        assert_eq!(next(&mut b).await, key(3));
        assert_eq!(a.tx.carrying_path(), Some(crate::stream::BLUETOOTH));
        assert_eq!(a.handle.report().carrying.as_deref(), Some("bluetooth"));
    }

    #[tokio::test]
    async fn the_path_kinds_name_cursios_lanes() {
        for lane in Lane::ALL {
            assert_eq!(
                cursio_channel::PathKind::from(PathKind::for_lane(lane)).lane(),
                Some(LaneId::from(lane))
            );
        }
        assert_eq!(
            cursio_channel::PathKind::from(PathKind::Bluetooth),
            cursio_channel::PathKind::Standby
        );
    }
}
