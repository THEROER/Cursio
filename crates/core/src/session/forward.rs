//! Input on its way through a session: the bounded queues it waits in,
//! forwarding what this machine captured, and merging what arrived.

use crate::session::latency::LatencyWindow;
use crate::session::SessionError;
use cursio_protocol::Message;
use std::time::Duration;

/// Bounded input supplied by the desktop hub or the Android touchpad.
pub type InputSender = crate::queue::Sender<CapturedInput>;
pub type InputReceiver = crate::queue::Receiver<CapturedInput>;

pub fn input_channel() -> (InputSender, InputReceiver) {
    crate::queue::channel(
        "captured input",
        4096,
        512 * 1024,
        |_| std::mem::size_of::<CapturedInput>(),
        |previous, next| crate::queue::merge_motion(&mut previous.event, &next.event),
    )
}

pub(super) type Incoming = (Message, std::time::Instant);
pub(super) fn inbound_channel() -> (
    crate::queue::Sender<Incoming>,
    crate::queue::Receiver<Incoming>,
) {
    crate::queue::channel(
        "session inbound",
        4096,
        4 * 1024 * 1024,
        |(message, _)| crate::queue::message_bytes(message),
        |previous, next| crate::queue::merge_messages(&mut previous.0, &next.0),
    )
}

/// Most queued messages folded into one batch of received input.
///
/// A bound only so a peer streaming input faster than it can be read cannot
/// hold the loop inside one turn; draining costs nanoseconds a message, so in
/// practice a batch ends where the queue does.
pub(super) const RECEIVED_INPUT_BATCH: usize = 128;
pub(super) const CAPTURED_INPUT_BATCH: usize = 128;
pub(super) const INPUT_PUMP_BUDGET: Duration = Duration::from_millis(2);
/// One locally captured input event, stamped when it was handed to sessions.
///
/// The stamp is what turns "the mouse lags" into a number on the sending side:
/// how long an event waited between capture handing it over and this session
/// giving it to the transport. The transport measures the rest of the way to
/// the socket itself.
#[derive(Debug, Clone, Copy)]
pub struct CapturedInput {
    pub event: cursio_protocol::InputEvent,
    pub at: std::time::Instant,
}

impl From<cursio_protocol::InputEvent> for CapturedInput {
    /// Stamped now: converting is the moment it is handed over.
    fn from(event: cursio_protocol::InputEvent) -> Self {
        Self {
            event,
            at: std::time::Instant::now(),
        }
    }
}

/// Forward one just-received captured event to the peer, **coalescing pointer
/// motion** to keep latency low under a flood of events.
///
/// A fast mouse move — or dragging a window — produces hundreds of tiny
/// `PointerMotion` events per second (and evdev even splits X and Y into
/// separate ones). Sending each as its own Noise frame lets an unbounded queue
/// build up faster than the link drains it, so latency grows without bound.
///
/// Instead, we drain everything already waiting in the capture channel
/// (`try_recv`, non-blocking) and merge consecutive motion into a single event
/// (summing `dx`/`dy`, which is exact for relative deltas). Any non-motion event
/// (button, key, scroll) first flushes the accumulated motion so ordering is
/// preserved, then is sent as-is. The net effect: one motion frame per drain
/// instead of hundreds, with identical resulting cursor movement.
pub(super) async fn forward_captured(
    tx: &mut dyn cursio_transport::TransportTx,
    first: CapturedInput,
    captured: Option<&mut InputReceiver>,
    latency: &mut LatencyWindow,
) -> Result<(), SessionError> {
    use cursio_protocol::InputEvent;
    use tokio::sync::mpsc::error::TryRecvError;

    /// Motion not yet sent, and when the oldest part of it was captured —
    /// what the latency figure charges a coalesced frame with, since that
    /// part waited longest.
    #[derive(Default)]
    struct Pending {
        dx: i32,
        dy: i32,
        oldest: Option<std::time::Instant>,
    }

    async fn flush(
        tx: &mut dyn cursio_transport::TransportTx,
        pending: &mut Pending,
        latency: &mut LatencyWindow,
    ) -> Result<(), SessionError> {
        if pending.dx != 0 || pending.dy != 0 {
            tx.send(&Message::Input(InputEvent::PointerMotion {
                dx: pending.dx,
                dy: pending.dy,
            }))
            .await?;
        }
        if let Some(oldest) = pending.oldest {
            latency.record(oldest.elapsed());
        }
        *pending = Pending::default();
        Ok(())
    }

    // Fold motion into the accumulator, or flush it and send the event.
    async fn handle(
        tx: &mut dyn cursio_transport::TransportTx,
        input: CapturedInput,
        pending: &mut Pending,
        latency: &mut LatencyWindow,
    ) -> Result<(), SessionError> {
        match input.event {
            InputEvent::PointerMotion { dx, dy } => {
                let (x, y) = (pending.dx.checked_add(dx), pending.dy.checked_add(dy));
                if x.is_none() || y.is_none() {
                    flush(tx, pending, latency).await?;
                }
                pending.dx = pending.dx.checked_add(dx).expect("accumulator was flushed");
                pending.dy = pending.dy.checked_add(dy).expect("accumulator was flushed");
                pending.oldest.get_or_insert(input.at);
                Ok(())
            }
            other => {
                // Preserve order: pending motion goes out before this event.
                //
                // Logged because motion is coalesced and non-motion is not: if a
                // button release were ever dropped or reordered here, the peer
                // would be left holding a button down — which looks exactly like
                // "the cursor stopped moving after I clicked".
                if matches!(other, InputEvent::PointerButton { .. }) {
                    tracing::debug!(event = ?other, pending_dx = pending.dx, pending_dy = pending.dy,
                        "forwarding pointer button to peer");
                }
                flush(tx, pending, latency).await?;
                tx.send(&Message::Input(other)).await?;
                latency.record(input.at.elapsed());
                Ok(())
            }
        }
    }

    let mut pending = Pending::default();
    handle(tx, first, &mut pending, latency).await?;

    // Drain everything already queued without blocking, coalescing as we go.
    if let Some(rx) = captured {
        let started = std::time::Instant::now();
        for _ in 1..CAPTURED_INPUT_BATCH {
            if started.elapsed() >= INPUT_PUMP_BUDGET {
                break;
            }
            match rx.try_recv() {
                Ok(input) => handle(tx, input, &mut pending, latency).await?,
                Err(TryRecvError::Empty) => break,
                // Sender gone: send what we have, then stop draining. The
                // caller's next recv sees the closed channel and clears it.
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    // Send any leftover coalesced motion.
    flush(tx, &mut pending, latency).await
}

/// The next message for the session: one held back from the last batch of
/// received input first, then whatever the reader forwards.
pub(super) async fn next_inbound(
    held_back: &mut Option<(Message, std::time::Instant)>,
    inbound: &mut crate::queue::Receiver<Incoming>,
) -> Option<(Message, std::time::Instant)> {
    match held_back.take() {
        Some(held) => Some(held),
        None => inbound.recv().await,
    }
}

/// Take `first` and every input message already queued behind it, merging
/// consecutive pointer motion.
///
/// The injector, not the link, bounds how fast received input can land.
/// `SendInput` measured 1–2 ms per real pointer move on the Windows laptop
/// (p50 1.7 ms back to back, 25 ms at worst), and a 1000 Hz mouse on the peer
/// sends a move every millisecond. Injected one message at a time, the queue
/// grew for as long as the hand moved: 24 ms median and 240 ms p99 from
/// reading an event to injecting it, the cursor still replaying a movement
/// after the hand had stopped. Summing what has already queued is exact for
/// relative deltas, and each call to the injector then carries whatever
/// arrived while the previous one ran, so the backlog cannot outgrow one call.
/// With nothing queued a move goes out alone, as before.
///
/// Stops at the first message that is not input and hands it back through
/// `held_back` instead of handling it here: a `Grab` or `CrossEdge` decides
/// whether the input after it is injected at all.
///
/// Each event comes with when the reader took it off the wire; merged motion
/// keeps its oldest part's, which waited longest.
pub(super) fn drain_received_input(
    first: cursio_protocol::InputEvent,
    read_at: std::time::Instant,
    inbound: &mut crate::queue::Receiver<Incoming>,
    held_back: &mut Option<(Message, std::time::Instant)>,
) -> Vec<(cursio_protocol::InputEvent, std::time::Instant)> {
    let mut batch = Vec::new();
    merge_received(&mut batch, first, read_at);
    for _ in 1..RECEIVED_INPUT_BATCH {
        match inbound.try_recv() {
            Ok((Message::Input(event), at)) => merge_received(&mut batch, event, at),
            Ok(other) => {
                *held_back = Some(other);
                break;
            }
            // Closed is left for the next `recv` to report, after this batch
            // has been injected.
            Err(_) => break,
        }
    }
    batch
}

/// Append `event` to `batch`, folding it into the last entry when both are
/// pointer motion.
pub(super) fn merge_received(
    batch: &mut Vec<(cursio_protocol::InputEvent, std::time::Instant)>,
    event: cursio_protocol::InputEvent,
    at: std::time::Instant,
) {
    use cursio_protocol::InputEvent::PointerMotion;
    if let (PointerMotion { dx, dy }, Some((PointerMotion { dx: sx, dy: sy }, _))) =
        (event, batch.last_mut())
    {
        if let (Some(x), Some(y)) = (sx.checked_add(dx), sy.checked_add(dy)) {
            *sx = x;
            *sy = y;
            return;
        }
    }
    batch.push((event, at));
}

/// Await the next item from an optional channel, or stay pending forever if
/// there's no channel — so the `select!` arm is inert rather than busy-looping
/// on `None`. Used for both the capture and the ownership-command channels.
pub(super) async fn recv_maybe<T>(
    channel: &mut Option<&mut tokio::sync::mpsc::UnboundedReceiver<T>>,
) -> Option<T> {
    match channel {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

pub(super) async fn recv_captured(
    channel: &mut Option<&mut InputReceiver>,
) -> Option<CapturedInput> {
    match channel {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::motion;

    /// Queued motion reaches the injector as one move with the same total, a
    /// click keeps its place between the moves either side of it, and the
    /// merged move is charged with the wait of its oldest part.
    #[test]
    fn queued_received_motion_is_merged_around_a_click() {
        let click = cursio_protocol::InputEvent::PointerButton {
            button: cursio_protocol::MouseButton::Left,
            pressed: true,
        };
        let (tx, mut rx) = inbound_channel();
        let first_at = std::time::Instant::now();
        for event in [motion(2, 1), click, motion(-1, 0), motion(-2, 5)] {
            tx.send((Message::Input(event), std::time::Instant::now()))
                .unwrap();
        }
        let mut held_back = None;
        let batch = drain_received_input(motion(1, 1), first_at, &mut rx, &mut held_back);
        let events: Vec<_> = batch.iter().map(|(event, _)| *event).collect();
        assert_eq!(events, vec![motion(3, 2), click, motion(-3, 5)]);
        assert_eq!(batch[0].1, first_at);
        assert!(held_back.is_none());
    }

    /// A handoff read in the middle of a burst ends the batch and is handed
    /// back rather than handled here, and nothing behind it is taken: whether
    /// the input after a `Grab` is injected at all depends on the `Grab`.
    #[test]
    fn a_handoff_ends_a_batch_of_received_input() {
        let (tx, mut rx) = inbound_channel();
        let now = std::time::Instant::now();
        tx.send((Message::Input(motion(1, 0)), now)).unwrap();
        tx.send((Message::Grab { active: false }, now)).unwrap();
        tx.send((Message::Input(motion(5, 0)), now)).unwrap();
        let mut held_back = None;
        let batch = drain_received_input(motion(1, 0), now, &mut rx, &mut held_back);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].0, motion(2, 0));
        assert!(matches!(
            held_back,
            Some((Message::Grab { active: false }, _))
        ));
        assert!(matches!(rx.try_recv(), Ok((Message::Input(_), _))));
    }
}
