//! Fan-out of the mirrored iPhone screen to anything beyond the local window.
//!
//! The receiver's own job ends at showing the screen on the machine the iPhone
//! picked. That is what makes AirPlay feel like a separate program bolted onto
//! one PC: the other machines in the mesh have no idea it is happening.
//!
//! A tap fixes that without a second decode. What arrives here is already
//! Annex-B H.264, and Cursio's window transfer already carries H.264 as
//! [`TileCodec::LossyVideo`](cursio_protocol::TileCodec) — so a peer can be
//! sent the very same access units and decode them with the surrogate window it
//! already has. Nothing is re-encoded, and the receiver stays unaware of who is
//! listening.
//!
//! ## Why parameter sets are held rather than just broadcast
//!
//! SPS/PPS arrive once, when the stream starts. A machine that joins the mesh a
//! minute later would receive only mid-stream frames and have nothing to
//! configure its decoder with, so the tap remembers the last configuration and
//! replays it to each new subscriber before the first frame it sees.

use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

/// How many access units a slow subscriber may fall behind before it is dropped.
///
/// Falling behind is not recoverable by waiting: video is a live stream, and a
/// subscriber that cannot keep up wants the newest picture rather than a queue
/// of stale ones. `broadcast` signals the gap, and the reader resynchronises at
/// the next parameter set.
const TAP_CAPACITY: usize = 64;

/// One unit of the mirrored stream, ready for a decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorUnit {
    /// SPS/PPS in Annex-B framing. Always precedes the frames it describes.
    ParameterSets(Arc<[u8]>),
    /// One complete access unit in Annex-B framing.
    AccessUnit(Arc<[u8]>),
}

/// Publishes the mirrored screen to any number of readers.
#[derive(Clone)]
pub struct MirrorTap {
    sender: broadcast::Sender<MirrorUnit>,
    /// The most recent parameter sets, replayed to whoever subscribes next.
    parameter_sets: Arc<Mutex<Option<Arc<[u8]>>>>,
}

impl MirrorTap {
    pub fn new() -> Self {
        Self {
            sender: broadcast::channel(TAP_CAPACITY).0,
            parameter_sets: Arc::new(Mutex::new(None)),
        }
    }

    /// A reader of everything published from now on, preceded by the parameter
    /// sets already seen so its decoder can start on the next frame.
    pub fn subscribe(&self) -> MirrorReader {
        // Subscribing before reading the stored sets, so a configuration
        // published in between is seen twice rather than missed. A decoder
        // handed the same SPS/PPS twice simply reconfigures; one handed none
        // cannot decode at all.
        let stream = self.sender.subscribe();
        let replay = self
            .parameter_sets
            .lock()
            .ok()
            .and_then(|sets| sets.clone())
            .map(MirrorUnit::ParameterSets);
        MirrorReader { stream, replay }
    }

    pub(crate) fn publish_parameter_sets(&self, bytes: &[u8]) {
        let sets: Arc<[u8]> = Arc::from(bytes);
        if let Ok(mut stored) = self.parameter_sets.lock() {
            *stored = Some(sets.clone());
        }
        let _ = self.sender.send(MirrorUnit::ParameterSets(sets));
    }

    pub(crate) fn publish_access_unit(&self, bytes: &[u8]) {
        let _ = self.sender.send(MirrorUnit::AccessUnit(Arc::from(bytes)));
    }

    /// Whether anything is listening, so the receiver can skip the copy when
    /// nothing is.
    pub(crate) fn has_readers(&self) -> bool {
        self.sender.receiver_count() != 0
    }
}

impl Default for MirrorTap {
    fn default() -> Self {
        Self::new()
    }
}

/// One reader's view of the mirrored stream.
pub struct MirrorReader {
    stream: broadcast::Receiver<MirrorUnit>,
    replay: Option<MirrorUnit>,
}

impl MirrorReader {
    /// The next unit, or `None` once the receiver is gone.
    ///
    /// A reader that fell too far behind is told how much it missed and then
    /// continues from the newest units; the caller waits for the next parameter
    /// set or keyframe rather than treating the gap as fatal.
    pub async fn next(&mut self) -> Option<MirrorUnit> {
        if let Some(replay) = self.replay.take() {
            return Some(replay);
        }
        loop {
            match self.stream.recv().await {
                Ok(unit) => return Some(unit),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "mirror reader fell behind; skipping ahead");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_late_subscriber_is_given_the_parameter_sets_first() {
        let tap = MirrorTap::new();
        tap.publish_parameter_sets(&[0, 0, 0, 1, 0x67]);
        tap.publish_access_unit(&[0, 0, 0, 1, 0x65, 0xaa]);

        // Subscribed after both: the frame is gone, the configuration is not.
        let mut late = tap.subscribe();
        assert_eq!(
            late.next().await,
            Some(MirrorUnit::ParameterSets(Arc::from(
                [0, 0, 0, 1, 0x67].as_slice()
            )))
        );

        tap.publish_access_unit(&[0, 0, 0, 1, 0x65, 0xbb]);
        assert_eq!(
            late.next().await,
            Some(MirrorUnit::AccessUnit(Arc::from(
                [0, 0, 0, 1, 0x65, 0xbb].as_slice()
            )))
        );
    }

    #[tokio::test]
    async fn readers_see_the_stream_in_order() {
        let tap = MirrorTap::new();
        let mut reader = tap.subscribe();
        tap.publish_parameter_sets(&[1]);
        tap.publish_access_unit(&[2]);
        tap.publish_access_unit(&[3]);

        assert_eq!(
            reader.next().await,
            Some(MirrorUnit::ParameterSets(Arc::from([1].as_slice())))
        );
        assert_eq!(
            reader.next().await,
            Some(MirrorUnit::AccessUnit(Arc::from([2].as_slice())))
        );
        assert_eq!(
            reader.next().await,
            Some(MirrorUnit::AccessUnit(Arc::from([3].as_slice())))
        );
    }

    #[test]
    fn a_tap_with_no_readers_is_visible_as_such() {
        let tap = MirrorTap::new();
        assert!(!tap.has_readers());
        let _reader = tap.subscribe();
        assert!(tap.has_readers());
    }
}
