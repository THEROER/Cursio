//! The traffic of a turn: frames out of each hosted capture, bounded so pixels
//! never crowd the link, and a surrogate's input, acks and keyframe requests
//! back to the host.

use crate::window_session::send_control;
use crate::window_session::WindowEngine;
use cursio_protocol::{Message, Priority, WindowCloseReason, WindowId, WindowMsg};
use cursio_transport::{TransportError, TransportTx};
use cursio_window::{CaptureSession, Surrogate, WindowCapture, WindowRenderer};

/// Most frames to forward in one [`SessionModule::pump`] call.
///
/// The session cannot reach its inbound branch while a module pumps, so a module
/// that drains a busy channel dry starves the peer's messages — the discipline
/// the trait documents. A capture at 60 fps produces roughly one frame per
/// 16 ms while `pump` runs far more often than that, so a small bound keeps
/// latency low and still never monopolises the loop.
pub(super) const MAX_FRAMES_PER_PUMP: usize = 4;

/// Stop pulling capture packets once this much video is waiting for the socket.
///
/// `PriorityTransport::send` only enqueues, so without this gate the session
/// drains one queue into another and can still accumulate seconds of stale
/// video. Backpressure now reaches the bounded capture channel instead.
pub(super) const VIDEO_QUEUE_HIGH_WATER_BYTES: usize = 2 * 1024 * 1024;

/// Most input events to forward per pump, viewer→host.
///
/// Pointer motion is chatty; the same starvation rule applies, and stale motion
/// is worthless anyway.
pub(super) const MAX_INPUT_PER_PUMP: usize = 32;

/// How often a viewer may ask the host for a whole picture.
///
/// A decoder waiting for a keyframe drops every packet and raises its request
/// again for each; the answer is an IDR or a full picture, the most expensive
/// thing on the link, so one second between asks bounds that cost while still
/// recovering a stuck view within a second of the host hearing it.
pub(super) const KEYFRAME_REQUEST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Rate-limits one stream's keyframe requests to [`KEYFRAME_REQUEST_INTERVAL`].
#[derive(Debug, Default)]
pub(crate) struct KeyframeRequests {
    last: Option<std::time::Instant>,
}

impl KeyframeRequests {
    /// Whether to send a request now, taking the surrogate's want only when
    /// one may go — so a want raised during the quiet interval is kept, not
    /// lost.
    pub(crate) fn poll(&mut self, now: std::time::Instant, wants: impl FnOnce() -> bool) -> bool {
        let ready = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= KEYFRAME_REQUEST_INTERVAL);
        if !ready || !wants() {
            return false;
        }
        self.last = Some(now);
        true
    }

    /// A request went out for another reason (a new stream); the next one
    /// waits its turn from here.
    pub(crate) fn note_sent(&mut self, now: std::time::Instant) {
        self.last = Some(now);
    }
}

impl<C: WindowCapture, R: WindowRenderer> WindowEngine<C, R> {
    /// Host → peer: forward captured frames, bounded per call.
    pub(super) async fn pump_frames(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        if self.hosted.is_empty() {
            return Ok(());
        }
        // Round-robin, one bounded pass per window rather than draining each in
        // turn: a busy window (a video playing) would otherwise spend the whole
        // budget every pump and a second window would never be sent at all.
        let ids: Vec<WindowId> = self.hosted.keys().copied().collect();
        let per_window = (MAX_FRAMES_PER_PUMP / ids.len()).max(1);
        let mut ended: Vec<WindowId> = Vec::new();

        for id in ids {
            let Some(hosted) = self.hosted.get_mut(&id) else {
                continue;
            };
            for _ in 0..per_window {
                if tx.queue_depth(Priority::Video).queued_bytes >= VIDEO_QUEUE_HIGH_WATER_BYTES {
                    break;
                }
                // `try_recv`, never `recv`: the capture channel going quiet is
                // the normal case (a static window emits nothing), and awaiting
                // it here would block the whole session loop on an idle window.
                match hosted.session.frames().try_recv() {
                    Ok(frame) => {
                        hosted.last_frame = Some(std::time::Instant::now());
                        tx.send(&Message::WindowFrame(frame)).await?;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        // The capture backend ended on its own (window closed,
                        // portal revoked). Tell the peer so its surrogate goes.
                        ended.push(id);
                        break;
                    }
                }
            }
        }

        for id in ended {
            tracing::info!(id, "window capture ended; telling the peer");
            self.hosted.remove(&id);
            self.note_transfer_ended();
            send_control(
                tx,
                WindowMsg::Close {
                    id,
                    reason: WindowCloseReason::HostClosed,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Viewer → host: forward the surrogate's input, then an ack if one is due.
    pub(super) async fn pump_viewer(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        if self.viewed.is_empty() {
            return Ok(());
        }
        let ids: Vec<WindowId> = self.viewed.keys().copied().collect();
        // Input is what the user's hand is doing right now, so each window gets
        // the full budget rather than a share of it: only the focused surrogate
        // produces any, and starving it to reserve room for idle ones would be
        // felt directly as lag.
        let mut closed: Vec<WindowId> = Vec::new();
        // Collected while iterating and sent after, so the borrow of `viewed`
        // ends before anything is awaited.
        let mut outbound: Vec<Message> = Vec::new();
        let mut control: Vec<WindowMsg> = Vec::new();
        let now = std::time::Instant::now();

        for id in ids {
            let Some(surrogate) = self.viewed.get_mut(&id) else {
                continue;
            };
            let mut gone = surrogate.is_gone();
            for _ in 0..MAX_INPUT_PER_PUMP {
                match surrogate.input().try_recv() {
                    Ok(input) => outbound.push(Message::WindowInput(input)),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        gone = true;
                        break;
                    }
                }
            }
            if gone {
                closed.push(id);
                continue;
            }
            // The user resized this surrogate: ask the host to resize the real
            // window, so the picture is re-rendered at the new size rather than
            // scaled up to it.
            if let Some((width, height)) = surrogate.take_resize_request() {
                control.push(WindowMsg::ResizeRequest { id, width, height });
            }
            // Rate-control feedback, self-limited by the surrogate to a few per
            // second — see `Surrogate::take_ack`.
            if let Some(ack) = surrogate.take_ack() {
                control.push(WindowMsg::FrameAck(ack));
            }
            let asks = self.keyframe_asks.entry(id).or_default();
            if asks.poll(now, || surrogate.take_keyframe_request()) {
                control.push(WindowMsg::KeyframeRequest { id });
            }
        }

        for msg in outbound {
            tx.send(&msg).await?;
        }
        for msg in control {
            send_control(tx, msg).await?;
        }
        for id in closed {
            tracing::info!(id, "surrogate window closed by the user");
            self.viewed.remove(&id);
            self.keyframe_asks.remove(&id);
            send_control(
                tx,
                WindowMsg::Close {
                    id,
                    reason: WindowCloseReason::ViewerClosed,
                },
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_requests_go_at_most_once_a_second_and_a_held_want_is_kept() {
        let mut asks = KeyframeRequests::default();
        let start = std::time::Instant::now();
        assert!(asks.poll(start, || true));
        let mut taken = false;
        assert!(
            !asks.poll(start + std::time::Duration::from_millis(500), || {
                taken = true;
                true
            })
        );
        assert!(
            !taken,
            "a want must not be consumed while no request may go"
        );
        assert!(asks.poll(start + KEYFRAME_REQUEST_INTERVAL, || true));
        assert!(!asks.poll(start + 3 * KEYFRAME_REQUEST_INTERVAL, || false));
    }
}
