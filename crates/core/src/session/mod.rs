//! Session handshake and liveness.
//!
//! Once a [`Transport`] is connected (and, for a first meeting, pairing has
//! run), a session begins with a [`Hello`](cursio_protocol::Hello) exchange — each side advertises its
//! protocol version and identity, and a version mismatch is rejected up front.
//! The greeting is preceded by the update lane's frozen preamble, so a
//! mismatch is learned from a message every build can read, and ends in
//! [`SessionError::UpdateOnly`] — a link that can still carry a new build —
//! rather than in a decode error.
//! After that the session runs a heartbeat loop so both ends detect a dead link
//! promptly, until either side sends [`Message::Bye`](cursio_protocol::Message::Bye) or the link drops.

use std::time::Duration;

use cursio_protocol::PROTOCOL_VERSION;

use cursio_transport::{Transport, TransportError};

mod forward;
mod handshake;
mod latency;
mod role;
mod run;
#[cfg(test)]
mod test_support;

pub use forward::{input_channel, CapturedInput, InputReceiver, InputSender};
pub use handshake::{handshake, handshake_as, local_device_kind, PeerHello};
pub use role::{CursorRole, CursorSetup, Entry, Ownership, OwnershipCmd};
pub use run::run_session_live;

/// Errors during the session handshake or run loop.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// A peer from before the update lane: it greeted with its old `Hello`
    /// and cannot read anything newer, so it has to be updated by hand once.
    #[error(
        "protocol version mismatch: local {local}, peer {peer}; the peer is too old to be \
         updated over the mesh — install this version on it by hand once"
    )]
    VersionMismatch { local: u16, peer: u16 },
    /// The peer speaks a different session protocol but the same update lane:
    /// the link can carry builds and nothing else.
    #[error("peer {} speaks protocol {} (this build {}); only updates can cross", .0.peer_id, .0.protocol_version, PROTOCOL_VERSION)]
    UpdateOnly(cursio_protocol::update::Preamble),
    #[error("expected Hello, got a different message")]
    UnexpectedMessage,
    #[error("peer ended the session")]
    PeerLeft,
    #[error("peer stopped responding (no traffic for {0:?})")]
    PeerTimeout(Duration),
}

/// How often to send a heartbeat while a session is idle.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Warn when one turn of the session loop works for at least this long.
///
/// The loop now holds only input, ownership, the heartbeat and node status, so
/// a slow turn here is exactly a stall the cursor felt — no module can cause
/// one any more, which is what makes the warning worth reading when it fires.
/// Timed from the moment a turn wakes, not from the previous turn: with no
/// module ticking it, an idle loop waits in `select!` for up to a heartbeat,
/// and measuring the gap between turns reported that idleness as a
/// five-second stall every five seconds.
const SLOW_TURN_WARN: Duration = Duration::from_millis(250);

const SESSION_DRAIN_DEADLINE: Duration = Duration::from_secs(2);

/// How long to wait for *any* inbound traffic before declaring the peer gone.
///
/// A machine that loses power (or has its cable pulled) never sends a FIN, so
/// the link looks perfectly healthy until the OS TCP timeout fires — minutes
/// later. Until then every send queues into the void and the session appears
/// frozen. Noticing ourselves turns that into a few seconds.
///
/// Three heartbeats: one missed beat is ordinary jitter, three is gone.
const PEER_TIMEOUT: Duration = Duration::from_secs(15);

/// Run the session loop until the peer leaves or the link drops.
///
/// Ownership model — **one global active machine** (MWB shared cursor):
/// - **Active** (`am_active`): this machine holds the shared cursor. Local input
///   acts normally (not grabbed); inbound peer input is injected here; our own
///   captured input is *not* forwarded (it already acts locally).
/// - **Dormant**: local input is grabbed and streamed to the active peer;
///   inbound input is *not* injected (we're not the one holding the cursor).
///   Reached only by *handing the cursor over*, never at startup — see
///   [`Ownership::initial`].
/// - **Idle**: what the machine without the cursor does before anyone has
///   handed anything anywhere. Its own keyboard and mouse act on it, normally.
///
/// Exactly one side is active at any moment. The two sides start in opposite
/// states (`start_active`) and every handoff flips *both* atomically: the
/// hotkey sends [`Message::Grab`](cursio_protocol::Message::Grab) to make the peer active, and going active
/// ourselves happens only when the peer hands it to us. This makes the
/// both-own deadlock (which caused an input feedback loop) impossible.
///
/// The parameters wire this crate to platform code without depending on it:
/// - `cursor` — how the shared cursor is arranged on this link: who starts with
///   it, and whether the peer is the sort of machine that can hold it at all.
///   Build it with [`CursorSetup::negotiate`], which both sides run to reach
///   complementary answers.
/// - `captured` — local input events; forwarded only while dormant.
/// - `on_input` — inject an inbound event; called only while active.
/// - `commands` — hotkey toggle requests; `None` for a session that never
///   initiates a handoff.
/// - `on_ownership` — called on every change so the caller can grab/ungrab and
///   hide/show the cursor to match.
/// - `modules` — subsystems multiplexed onto the same channel. Each runs on a
///   thread of its own ([`crate::module_host`]) with its own handle onto the
///   link, and the reader routes its messages straight to it, so nothing a
///   module does — however long it waits — reaches this loop. Input
///   deliberately stays hard-wired here rather than becoming a module: its
///   ownership/grab/edge logic is ordering-sensitive and `!Send`, and it is
///   what this loop now exists for.
///
/// Takes the transport **by value** (`Box<dyn Transport>`) so it can
/// [`split`](Transport::split) it: the receive half runs in its own task and is
/// therefore never cancelled. That matters because `recv` is not cancel-safe
/// for the encrypted transport — cancelling it mid-await (which a plain
/// `select!` over `recv` does whenever a send wins) can desynchronise the Noise
/// cipher and silently drop all later inbound messages.
#[allow(clippy::too_many_arguments)]
pub async fn run_session<F, G, H>(
    transport: Box<dyn Transport>,
    cursor: CursorSetup,
    captured: Option<&mut InputReceiver>,
    on_input: F,
    commands: Option<&mut tokio::sync::mpsc::UnboundedReceiver<OwnershipCmd>>,
    on_ownership: G,
    modules: Vec<Box<dyn crate::module::SessionModule>>,
    poll_status: H,
) -> Result<(), SessionError>
where
    F: FnMut(cursio_protocol::InputEvent),
    G: FnMut(Ownership),
    H: FnMut() -> Option<cursio_protocol::NodeStatus>,
{
    run_session_live(
        transport,
        cursor,
        captured,
        on_input,
        commands,
        on_ownership,
        modules,
        poll_status,
        None,
        None,
    )
    .await
}

/// Run the session loop, ignoring any inbound input events.
///
/// Thin wrapper over [`run_session`] for callers that only need liveness (no
/// input capture or injection), e.g. tests or a liveness-only role.
pub async fn run_heartbeat(transport: Box<dyn Transport>) -> Result<(), SessionError> {
    // Ownership is irrelevant with no capture or injection: stay dormant and
    // let the peer keep the cursor it already has.
    let cursor = CursorSetup {
        start_active: false,
        peer_takes_cursor: true,
    };
    // Liveness only: nothing here watches the machine, so it never has a status
    // change to report.
    run_session(
        transport,
        cursor,
        None,
        |_| {},
        None,
        |_| {},
        Vec::new(),
        || None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::forward::forward_captured;
    use crate::session::forward::merge_received;
    use crate::session::forward::CAPTURED_INPUT_BATCH;
    use crate::session::latency::LatencyWindow;
    use crate::session::test_support::motion;
    use crate::test_support::RecordingTx;
    use cursio_protocol::Message;
    use cursio_transport::TransportTx;
    use std::sync::Arc;
    #[tokio::test]
    async fn capture_forwarding_yields_before_exhausting_a_large_nonmergeable_backlog() {
        let (sender, mut receiver) = input_channel();
        for _ in 0..1000 {
            sender
                .send(CapturedInput::from(cursio_protocol::InputEvent::Key {
                    key: 1,
                    pressed: true,
                }))
                .unwrap();
        }
        let first = receiver.try_recv().unwrap();
        let mut output = RecordingTx::default();
        forward_captured(
            &mut output,
            first,
            Some(&mut receiver),
            &mut LatencyWindow::new("test"),
        )
        .await
        .unwrap();
        assert!(!receiver.is_empty());
        assert!(output.sent.len() <= CAPTURED_INPUT_BATCH);
    }

    #[test]
    fn relative_motion_overflow_splits_instead_of_losing_displacement() {
        let now = std::time::Instant::now();
        let mut batch = vec![(motion(i32::MAX, 0), now)];
        merge_received(&mut batch, motion(1, 0), now);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].0, motion(i32::MAX, 0));
        assert_eq!(batch[1].0, motion(1, 0));
    }

    struct BlockedWire {
        incoming: tokio::sync::mpsc::Receiver<Message>,
        entered: Arc<tokio::sync::Notify>,
    }
    struct BlockedSend(Arc<tokio::sync::Notify>);
    struct ReceiveWire(tokio::sync::mpsc::Receiver<Message>);
    #[async_trait::async_trait]
    impl TransportTx for BlockedSend {
        async fn send(&mut self, _: &Message) -> Result<(), TransportError> {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    #[async_trait::async_trait]
    impl cursio_transport::TransportRx for ReceiveWire {
        async fn recv(&mut self) -> Result<Message, TransportError> {
            self.0.recv().await.ok_or(TransportError::Closed)
        }
    }
    #[async_trait::async_trait]
    impl Transport for BlockedWire {
        async fn send(&mut self, _: &Message) -> Result<(), TransportError> {
            std::future::pending().await
        }
        async fn recv(&mut self) -> Result<Message, TransportError> {
            self.incoming.recv().await.ok_or(TransportError::Closed)
        }
        fn kind(&self) -> &'static str {
            "blocked-test"
        }
        fn split(
            self: Box<Self>,
        ) -> (Box<dyn TransportTx>, Box<dyn cursio_transport::TransportRx>) {
            (
                Box::new(BlockedSend(self.entered)),
                Box::new(ReceiveWire(self.incoming)),
            )
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_sender_does_not_prevent_receiving_goodbye_or_boundless_flush() {
        let (send, incoming) = tokio::sync::mpsc::channel(1);
        let entered = Arc::new(tokio::sync::Notify::new());
        let wire = BlockedWire {
            incoming,
            entered: entered.clone(),
        };
        let start = tokio::time::Instant::now();
        let (result, ()) = tokio::join!(run_heartbeat(Box::new(wire)), async {
            entered.notified().await;
            send.send(Message::Bye).await.unwrap();
        });
        assert!(matches!(result, Err(SessionError::PeerLeft)));
        assert!(start.elapsed() <= SESSION_DRAIN_DEADLINE + Duration::from_millis(1));
    }
}
