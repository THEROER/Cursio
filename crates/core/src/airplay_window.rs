//! The mirrored iPhone screen, offered to the mesh as a transferable window.
//!
//! Without this the iPhone reaches exactly one machine — whichever one it was
//! picked from in the Screen Mirroring list — and Cursio's AirPlay support
//! behaves like a separate program that happens to share a process. The screen
//! belongs on the mesh like anything else.
//!
//! ## Why this reuses window transfer rather than adding a protocol
//!
//! AirPlay delivers H.264, and [`TileCodec::LossyVideo`] already carries H.264
//! between Cursio machines — it is what a Windows host uses for a window in
//! motion. So the access units go out **exactly as they arrived**: nothing is
//! decoded, re-encoded, or converted on the way to a peer, and the viewer needs
//! no new code at all, because its surrogate window already decodes that codec.
//!
//! ## What an iPhone is not
//!
//! It is not a mesh peer: it has no Cursio identity, runs no modules, and never
//! appears in the peer registry. The machine it mirrors to is the host, and the
//! phone's screen is simply one of the things that host can show the others.
//! That also means [`apply_input`](CaptureSession::apply_input) has nowhere to
//! go — AirPlay mirroring is one-way, so a viewer's clicks are dropped rather
//! than pretended about.

use cursio_airplay::{MirrorReader, MirrorTap, MirrorUnit};
use cursio_protocol::{
    Rect, TileCodec, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput,
    WindowMeta,
};
use cursio_window::{CaptureSession, WindowCapture, WindowError, CAPTURE_FRAME_QUEUE_CAPACITY};
use tokio::sync::mpsc;

/// What the peer's surrogate window is titled.
const AIRPLAY_WINDOW_TITLE: &str = "Cursio (iPhone)";

/// Offers whatever an iPhone is currently mirroring to this machine.
pub struct AirPlayWindowCapture {
    tap: MirrorTap,
    width: u32,
    height: u32,
}

impl AirPlayWindowCapture {
    pub fn new(tap: MirrorTap, width: u32, height: u32) -> Self {
        Self { tap, width, height }
    }
}

impl WindowCapture for AirPlayWindowCapture {
    type Session = AirPlayCaptureSession;

    /// Always succeeds, even with no iPhone connected.
    ///
    /// Subscribing early is the point: the offer goes to the peer straight
    /// away, and frames start flowing the moment someone picks this machine in
    /// Screen Mirroring — rather than requiring the user to arrange the two
    /// events in the right order.
    fn begin(&mut self, id: WindowId) -> Result<Self::Session, WindowError> {
        let (frames_tx, frames_rx) = mpsc::channel(CAPTURE_FRAME_QUEUE_CAPACITY);
        let reader = self.tap.subscribe();
        let dirty = Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        };
        let forward = tokio::spawn(forward_stream(reader, frames_tx, id, dirty));
        Ok(AirPlayCaptureSession {
            meta: WindowMeta {
                id,
                title: AIRPLAY_WINDOW_TITLE.to_string(),
                width: self.width,
                height: self.height,
            },
            frames: frames_rx,
            forward,
            reported_input: false,
        })
    }
}

/// Turns the mirrored stream into window frames for one peer.
///
/// Sending is awaited rather than dropped on a full queue. A dropped access
/// unit corrupts every frame that references it, whereas waiting pushes the
/// backlog onto the tap — which is built to skip a slow reader ahead to the
/// newest picture and say so.
async fn forward_stream(
    mut reader: MirrorReader,
    frames: mpsc::Sender<WindowFrame>,
    id: WindowId,
    dirty: Rect,
) {
    let mut seq = 0_u32;
    while let Some(unit) = reader.next().await {
        let data = match unit {
            MirrorUnit::ParameterSets(bytes) | MirrorUnit::AccessUnit(bytes) => bytes,
        };
        let frame = WindowFrame {
            id,
            seq,
            dirty,
            codec: TileCodec::LossyVideo,
            data: data.to_vec(),
        };
        seq = seq.wrapping_add(1);
        if frames.send(frame).await.is_err() {
            return;
        }
    }
}

/// One peer's view of the mirrored screen.
pub struct AirPlayCaptureSession {
    meta: WindowMeta,
    frames: mpsc::Receiver<WindowFrame>,
    forward: tokio::task::JoinHandle<()>,
    reported_input: bool,
}

impl CaptureSession for AirPlayCaptureSession {
    fn meta(&self) -> &WindowMeta {
        &self.meta
    }

    fn frames(&mut self) -> &mut mpsc::Receiver<WindowFrame> {
        &mut self.frames
    }

    fn apply_input(&mut self, _input: WindowInput) -> Result<(), WindowError> {
        // Said once per session rather than per event: a viewer clicking on the
        // mirrored screen would otherwise fill the log at pointer rate.
        if !self.reported_input {
            self.reported_input = true;
            tracing::info!(
                id = self.meta.id,
                "input on a mirrored iPhone screen is ignored; AirPlay mirroring is one-way"
            );
        }
        Ok(())
    }

    fn on_ack(&mut self, _ack: WindowFrameAck) {
        // Nothing to adapt: the iPhone chooses its own bitrate and resolution,
        // and this host only forwards what it is given.
    }

    fn close(&mut self, reason: WindowCloseReason) {
        tracing::info!(
            id = self.meta.id,
            ?reason,
            "no longer sharing the iPhone screen"
        );
        self.forward.abort();
    }
}

impl Drop for AirPlayCaptureSession {
    fn drop(&mut self) {
        self.forward.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_offer_describes_the_iphone_screen() {
        let mut capture = AirPlayWindowCapture::new(MirrorTap::new(), 1920, 1080);
        let session = capture.begin(7).unwrap();
        assert_eq!(session.meta().id, 7);
        assert_eq!(session.meta().title, AIRPLAY_WINDOW_TITLE);
        assert_eq!(session.meta().width, 1920);
        assert_eq!(session.meta().height, 1080);
    }

    /// The parameter sets have to reach the peer as an ordinary frame, because
    /// its decoder is configured by the stream and nothing else.
    #[tokio::test]
    async fn mirrored_units_become_video_frames_in_order() {
        let tap = MirrorTap::new();
        let mut capture = AirPlayWindowCapture::new(tap.clone(), 1280, 720);
        let mut session = capture.begin(1).unwrap();
        // Let the forwarding task subscribe before anything is published.
        tokio::task::yield_now().await;

        tap.publish_parameter_sets(&[0, 0, 0, 1, 0x67]);
        tap.publish_access_unit(&[0, 0, 0, 1, 0x65, 0xaa]);

        let first = session.frames().recv().await.unwrap();
        assert_eq!(first.codec, TileCodec::LossyVideo);
        assert_eq!(first.data, [0, 0, 0, 1, 0x67]);
        assert_eq!(first.seq, 0);
        assert_eq!(first.dirty.width, 1280);

        let second = session.frames().recv().await.unwrap();
        assert_eq!(second.data, [0, 0, 0, 1, 0x65, 0xaa]);
        assert_eq!(second.seq, 1);
    }

    #[tokio::test]
    async fn closing_stops_the_forwarder() {
        let tap = MirrorTap::new();
        let mut capture = AirPlayWindowCapture::new(tap.clone(), 640, 480);
        let mut session = capture.begin(2).unwrap();
        tokio::task::yield_now().await;

        session.close(WindowCloseReason::HostClosed);
        tokio::task::yield_now().await;
        tap.publish_access_unit(&[1, 2, 3]);

        assert!(session.frames().try_recv().is_err());
    }
}
