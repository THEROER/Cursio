//! The session's side of a surrogate window, the same on every platform.
//!
//! A surrogate's window lives on a thread of its own — GTK's and Win32's
//! windows both belong to the thread that made them — and only channels cross
//! to it: frames in, presentations and input out. Everything on the session's
//! side of those channels is platform-free: noticing from a frame that the
//! host resized the window, decoding a tile here so the window thread stays
//! free to paint, passing a video packet through untouched, counting what was
//! presented into acknowledgements, and asking the window to close. The GTK
//! and Win32 surrogates hold one of these and add only what their windows
//! differ in.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cursio_protocol::{Rect, TileCodec, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId};
use tokio::sync::mpsc;

use crate::codec::decode_tile;
use crate::framebuffer::{geometry_of, Geometry};
use crate::presentation_ack::PresentationAcks;
use crate::WindowError;

/// What the session hands a window thread. `X` is what only one platform's
/// window understands.
pub(crate) enum SurrogateCommand<X> {
    /// A decoded tile to blit and present.
    Frame { seq: u32, dirty: Rect, raw: Vec<u8> },
    /// A raw H.264 packet, decoded on the window's own side.
    ///
    /// Every other codec is decoded on the session's thread. Video cannot be:
    /// a decoder is stateful and its pieces are not `Send` (`ffmpeg`'s scaling
    /// context deliberately does not claim to be), while a session module
    /// must be. Nor on the GTK thread itself, where it used to be: a phone's
    /// 848×1920 stream took ticks past 100 ms there, and the window stopped
    /// answering its own close button. Sending the packet rather than the
    /// pixels also saves a copy, since the decoded frame is wanted where the
    /// framebuffer lives.
    VideoPacket { seq: u32, packet: Vec<u8> },
    /// Window geometry changed; reallocate before the next blit.
    Resize { width: u32, height: u32 },
    /// The window grew, learnt from a tile rather than a full frame. Keeps the
    /// pixels already drawn — see [`Framebuffer::grow_to`].
    ///
    /// [`Framebuffer::grow_to`]: crate::framebuffer::Framebuffer::grow_to
    Grow { width: u32, height: u32 },
    /// Tear the window down.
    Close(WindowCloseReason),
    /// Something only this platform's window does. A GTK window has nothing
    /// (its `X` is uninhabited); a Win32 remote-screen view switches source.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Platform(X),
}

/// One surrogate's session side; see the module docs.
pub(crate) struct SurrogateFront<X> {
    id: WindowId,
    /// Geometry as the viewer currently believes it — used to spot a resize and
    /// to size-check tiles before they reach the window thread.
    width: u32,
    height: u32,
    commands: mpsc::UnboundedSender<SurrogateCommand<X>>,
    presented: mpsc::UnboundedReceiver<u32>,
    acks: PresentationAcks,
    /// Whether a `LossyVideo` packet has been seen, so the log says once that
    /// the stream switched to video.
    video_started: bool,
    /// Raised by the window thread while its decoder waits for a keyframe,
    /// and here when frames went missing on the way.
    wants_keyframe: Arc<AtomicBool>,
    /// The newest `seq` seen, to notice frames that never arrived.
    last_seq: Option<u32>,
}

impl<X> SurrogateFront<X> {
    pub(crate) fn new(
        id: WindowId,
        (width, height): (u32, u32),
        commands: mpsc::UnboundedSender<SurrogateCommand<X>>,
        presented: mpsc::UnboundedReceiver<u32>,
        wants_keyframe: Arc<AtomicBool>,
    ) -> Self {
        Self {
            id,
            width,
            height,
            commands,
            presented,
            acks: PresentationAcks::new(id),
            video_started: false,
            wants_keyframe,
            last_seq: None,
        }
    }

    /// The size the window is being shown at. Only GTK asks: it lets the
    /// user drag the window to a size and tells the host.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Route one frame to the window thread.
    pub(crate) fn apply_frame(&mut self, frame: &WindowFrame) -> Result<(), WindowError> {
        self.note_seq(frame.seq);
        // A frame the picture cannot hold means the host resized the window;
        // adopt the new geometry before blitting into it.
        match geometry_of(frame.dirty, frame.codec, self.width, self.height) {
            Geometry::Unchanged => {}
            Geometry::Resize { width, height } => {
                (self.width, self.height) = (width, height);
                self.send(SurrogateCommand::Resize { width, height });
            }
            Geometry::Grow { width, height } => {
                (self.width, self.height) = (width, height);
                self.send(SurrogateCommand::Grow { width, height });
            }
        }

        // `LossyVideo` is the one codec whose packets are *not* self-contained:
        // each references earlier ones, so it needs a decoder that persists
        // across frames, and one packet may yield zero pictures (the decoder is
        // still accumulating) or several. Every other codec is a pure
        // bytes-in/pixels-out transform.
        if frame.codec == TileCodec::LossyVideo {
            if !self.video_started {
                self.video_started = true;
                tracing::info!(id = self.id, "host switched this window to a video stream");
            }
            return self.deliver(SurrogateCommand::VideoPacket {
                seq: frame.seq,
                packet: frame.data.clone(),
            });
        }

        let expected = (frame.dirty.width as usize) * (frame.dirty.height as usize) * 4;
        // Decoding happens here, off the window thread: zstd on a large tile is
        // real work and the window thread must stay free to actually paint.
        let raw = decode_tile(frame.codec, &frame.data, expected)?;
        self.deliver(SurrogateCommand::Frame {
            seq: frame.seq,
            dirty: frame.dirty,
            raw,
        })
    }

    /// Ask for the whole picture when numbers were skipped.
    ///
    /// A frame can go missing on purpose: a sender drops video too old to be
    /// worth showing, and a session moving to another path does not resend
    /// it. Every later H.264 packet then references a picture this decoder
    /// never had, and a lost tile leaves its region stale, both until the
    /// host sends the whole picture — which, with an unbounded GOP, it does
    /// only when asked. The decoder cannot notice by itself: it decodes the
    /// references it lacks into smears rather than failing. Tiles of one
    /// update share a `seq`, so only a step of two or more is a loss; a
    /// number going backwards is a stream that started over.
    fn note_seq(&mut self, seq: u32) {
        if let Some(last) = self.last_seq {
            let step = seq.wrapping_sub(last);
            if step == 0 || step > u32::MAX / 2 {
                if step != 0 {
                    self.last_seq = Some(seq);
                }
                return;
            }
            if step > 1 {
                tracing::debug!(
                    id = self.id,
                    missing = step - 1,
                    "frames went missing; asking for the whole picture"
                );
                self.wants_keyframe.store(true, Ordering::Relaxed);
            }
        }
        self.last_seq = Some(seq);
    }

    pub(crate) fn take_ack(&mut self) -> Option<WindowFrameAck> {
        while let Ok(seq) = self.presented.try_recv() {
            self.acks.note_presented(seq);
        }
        self.acks.take()
    }

    pub(crate) fn take_keyframe_request(&mut self) -> bool {
        self.wants_keyframe.swap(false, Ordering::Relaxed)
    }

    /// The stream starts over with another source: what was learnt about the
    /// old one (its video, its presentations) no longer applies.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn restart(&mut self) {
        self.video_started = false;
        self.acks = PresentationAcks::new(self.id);
        self.last_seq = None;
    }

    /// The window thread ended, and with it the command receiver. Win32 learns
    /// that sooner, from its own closed flag.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn is_gone(&self) -> bool {
        self.commands.is_closed()
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn platform(&self, command: X) {
        self.send(SurrogateCommand::Platform(command));
    }

    pub(crate) fn close(&mut self, reason: WindowCloseReason) {
        tracing::info!(?reason, id = self.id, "closing surrogate window");
        self.send(SurrogateCommand::Close(reason));
    }

    /// A command the window may already be past wanting: a closed window has
    /// nothing to resize.
    fn send(&self, command: SurrogateCommand<X>) {
        let _ = self.commands.send(command);
    }

    /// A command whose loss is the window having gone.
    fn deliver(&self, command: SurrogateCommand<X>) -> Result<(), WindowError> {
        self.commands
            .send(command)
            .map_err(|_| WindowError::Backend("surrogate window is gone".into()))
    }
}

impl<X> Drop for SurrogateFront<X> {
    fn drop(&mut self) {
        self.send(SurrogateCommand::Close(WindowCloseReason::ViewerClosed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn front() -> (
        SurrogateFront<std::convert::Infallible>,
        mpsc::UnboundedReceiver<SurrogateCommand<std::convert::Infallible>>,
    ) {
        let (commands, window) = mpsc::unbounded_channel();
        let (_presented_tx, presented) = mpsc::unbounded_channel();
        let front = SurrogateFront::new(
            7,
            (4, 4),
            commands,
            presented,
            Arc::new(AtomicBool::new(false)),
        );
        (front, window)
    }

    fn video(seq: u32) -> WindowFrame {
        WindowFrame {
            id: 7,
            seq,
            dirty: Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            codec: TileCodec::LossyVideo,
            data: vec![0, 0, 0, 1],
        }
    }

    #[test]
    fn a_skipped_frame_asks_for_the_whole_picture() {
        let (mut front, _window) = front();
        front.apply_frame(&video(1)).unwrap();
        front.apply_frame(&video(2)).unwrap();
        assert!(!front.take_keyframe_request());
        front.apply_frame(&video(4)).unwrap();
        assert!(front.take_keyframe_request());
    }

    #[test]
    fn tiles_sharing_a_number_and_a_restarted_stream_are_not_losses() {
        let (mut front, _window) = front();
        front.apply_frame(&video(10)).unwrap();
        front.apply_frame(&video(10)).unwrap();
        front.apply_frame(&video(11)).unwrap();
        front.apply_frame(&video(0)).unwrap();
        front.apply_frame(&video(1)).unwrap();
        assert!(!front.take_keyframe_request());
    }

    #[test]
    fn numbering_wraps_without_a_loss() {
        let (mut front, _window) = front();
        front.apply_frame(&video(u32::MAX)).unwrap();
        front.apply_frame(&video(0)).unwrap();
        assert!(!front.take_keyframe_request());
    }
}
