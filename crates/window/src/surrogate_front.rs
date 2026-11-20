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
use crate::frame_gap::FrameGap;
use crate::framebuffer::{geometry_of, Geometry, VideoSize};
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
    ///
    /// `restart` says the stream starts over at another size and needs a
    /// fresh decoder; see [`VideoSize`].
    VideoPacket {
        seq: u32,
        packet: Vec<u8>,
        restart: bool,
    },
    /// The frame numbered `seq` changed the window's size; the window takes it
    /// on with [`Framebuffer::follow`] before the frame arrives.
    ///
    /// [`Framebuffer::follow`]: crate::framebuffer::Framebuffer::follow
    Geometry { seq: u32, change: Geometry },
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
    video_size: VideoSize,
    /// Raised by the window thread while its decoder waits for a keyframe,
    /// and here when frames went missing on the way.
    wants_keyframe: Arc<AtomicBool>,
    gap: FrameGap,
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
            video_size: VideoSize::default(),
            wants_keyframe,
            gap: FrameGap::default(),
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
        let missing = self.gap.note(frame.seq);
        if missing > 0 {
            tracing::debug!(
                id = self.id,
                missing,
                "frames went missing; asking for the whole picture"
            );
            self.wants_keyframe.store(true, Ordering::Relaxed);
        }
        // A frame the picture cannot hold means the host resized the window;
        // adopt the new geometry before blitting into it.
        let change = geometry_of(frame.dirty, frame.codec, self.width, self.height);
        if let Geometry::Resize { width, height } | Geometry::Grow { width, height } = change {
            (self.width, self.height) = (width, height);
            self.send(SurrogateCommand::Geometry {
                seq: frame.seq,
                change,
            });
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
            let restart = self.video_size.starts_over(frame.dirty);
            return self.deliver(SurrogateCommand::VideoPacket {
                seq: frame.seq,
                packet: frame.data.clone(),
                restart,
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
        self.video_size.forget();
        self.acks = PresentationAcks::new(self.id);
        self.gap.reset();
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
    use crate::framebuffer::{Admitted, Framebuffer};

    type Command = SurrogateCommand<std::convert::Infallible>;

    fn front(
        width: u32,
        height: u32,
    ) -> (
        SurrogateFront<std::convert::Infallible>,
        mpsc::UnboundedReceiver<Command>,
    ) {
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (_presented_tx, presented) = mpsc::unbounded_channel();
        let front = SurrogateFront::new(
            1,
            (width, height),
            commands,
            presented,
            Arc::new(AtomicBool::new(false)),
        );
        (front, commands_rx)
    }

    fn tile(seq: u32, x: u32, y: u32, width: u32, height: u32) -> WindowFrame {
        WindowFrame {
            id: 1,
            seq,
            dirty: Rect {
                x,
                y,
                width,
                height,
            },
            codec: TileCodec::RawBgra8888,
            data: vec![0x55; (width * height * 4) as usize],
        }
    }

    fn video(seq: u32, width: u32, height: u32) -> WindowFrame {
        WindowFrame {
            id: 1,
            seq,
            dirty: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            codec: TileCodec::LossyVideo,
            data: vec![0, 0, 0, 1, 0x65],
        }
    }

    /// The window thread's part, as both surrogates do it: geometry and tiles
    /// in order, video packets left for a decoder that answers later.
    fn window_applies(fb: &mut Framebuffer, commands: &mut mpsc::UnboundedReceiver<Command>) {
        while let Ok(command) = commands.try_recv() {
            match command {
                SurrogateCommand::Geometry { seq, change } => fb.follow(seq, change),
                SurrogateCommand::Frame { dirty, raw, .. } => {
                    fb.blit(dirty, &raw).expect("no tile is refused");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn a_skipped_frame_asks_for_the_whole_picture() {
        let (mut front, _window) = front(4, 4);
        front.apply_frame(&video(1, 4, 4)).unwrap();
        front.apply_frame(&video(2, 4, 4)).unwrap();
        assert!(!front.take_keyframe_request());
        front.apply_frame(&video(4, 4, 4)).unwrap();
        assert!(front.take_keyframe_request());
    }

    #[test]
    fn a_window_maximised_on_the_host_keeps_its_new_size_against_a_late_picture() {
        let (mut front, mut commands) = front(960, 1032);
        let mut fb = Framebuffer::new(960, 1032);
        front.apply_frame(&video(1, 960, 1032)).unwrap();
        window_applies(&mut fb, &mut commands);

        front.apply_frame(&video(2, 1920, 1032)).unwrap();
        window_applies(&mut fb, &mut commands);
        // The decoder, behind the frames, now hands over the first stream's
        // picture.
        assert_eq!(fb.admit(1, 960, 1032), Admitted::Stale);

        // The host settles the picture with bands across the new width.
        for y in [0, 259, 518] {
            front.apply_frame(&tile(3, 0, y, 1920, 259)).unwrap();
        }
        front.apply_frame(&tile(3, 0, 777, 1920, 256)).unwrap();
        window_applies(&mut fb, &mut commands);
        assert_eq!((fb.width, fb.height), (1920, 1033));
    }

    #[test]
    fn a_tile_that_grows_the_window_announces_it_with_its_own_number() {
        let (mut front, mut commands) = front(4, 2);
        front.apply_frame(&tile(7, 2, 1, 4, 3)).unwrap();
        assert!(matches!(
            commands.try_recv(),
            Ok(SurrogateCommand::Geometry {
                seq: 7,
                change: Geometry::Grow {
                    width: 6,
                    height: 4
                }
            })
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(SurrogateCommand::Frame { seq: 7, .. })
        ));
    }

    fn restart_of(commands: &mut mpsc::UnboundedReceiver<Command>) -> Option<bool> {
        while let Ok(command) = commands.try_recv() {
            if let SurrogateCommand::VideoPacket { restart, .. } = command {
                return Some(restart);
            }
        }
        None
    }

    #[test]
    fn a_video_stream_at_a_new_size_gets_a_fresh_decoder() {
        let (mut front, mut commands) = front(960, 1032);
        front.apply_frame(&video(1, 960, 1032)).unwrap();
        assert_eq!(restart_of(&mut commands), Some(false));
        front.apply_frame(&video(2, 960, 1032)).unwrap();
        assert_eq!(restart_of(&mut commands), Some(false));
        front.apply_frame(&video(3, 1920, 1032)).unwrap();
        assert_eq!(restart_of(&mut commands), Some(true));
    }

    /// An odd-sized window: video crops it to even, the settled picture comes
    /// as tiles at full size. The stream itself never changed, so its decoder
    /// must not be thrown away and wait for a keyframe.
    #[test]
    fn tiles_of_another_size_between_video_frames_do_not_restart_the_decoder() {
        let (mut front, mut commands) = front(1824, 1004);
        front.apply_frame(&video(1, 1824, 1004)).unwrap();
        assert_eq!(restart_of(&mut commands), Some(false));
        front.apply_frame(&tile(2, 0, 753, 1825, 252)).unwrap();
        front.apply_frame(&video(3, 1824, 1004)).unwrap();
        assert_eq!(restart_of(&mut commands), Some(false));
    }
}
