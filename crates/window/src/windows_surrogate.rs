//! Windows surrogate window: show a window transferred *from* the peer.
//!
//! The viewer side on Windows, mirroring [`linux_surrogate`](crate::linux_surrogate).
//! A plain Win32 window is created from the [`WindowMeta`], inbound frames are
//! blitted into a shared [`Framebuffer`], and `StretchDIBits` paints it. Input
//! from the user comes back out as [`WindowInput`] in the **host's** pixel
//! space.
//!
//! The same window serves as a **remote-screen view**: view-only, and
//! **unseen** — layered at alpha 0, never activated, parked past the edge of
//! every monitor — yet still listed and recorded by Discord and OBS, which is
//! the only reason it exists. Its selector is a notification-area icon's menu.
//! Why exactly that combination of states is in [`unseen`].
//!
//! ## Why raw Win32 rather than a toolkit
//!
//! The Linux side uses GTK because it was already in the tree and gave cooked
//! input for free. Windows has no such incumbent here, and the surrogate needs
//! very little: one window, one blit, and raw input events — which is close to
//! the minimum a Win32 window already does. Pulling in a toolkit would add a
//! dependency for the parts we would then have to bypass anyway (we want the
//! *raw* mouse coordinates, not a widget's interpretation of them).
//!
//! Pixels are cheap here for the same reason as on Linux: the wire format is
//! BGRA and `StretchDIBits` consumes BGRA directly, so a frame reaches the
//! screen without a colour conversion.
//!
//! ## Threading
//!
//! A Win32 window belongs to the thread that created it — its message loop must
//! run there and nowhere else. So the window lives on a **dedicated thread**
//! and only channels cross the boundary, exactly like the GTK surrogate:
//! frames in, input and acks out.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cursio_protocol::{
    Rect, ScreenSource, TileCodec, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId,
    WindowInput, WindowMeta,
};
use tokio::sync::mpsc;

use crate::codec::decode_tile;
use crate::framebuffer::{geometry_of, Geometry};
use crate::{
    presentation_ack::PresentationAcks, SourcePick, Surrogate, WindowError, WindowRenderer,
};

mod unseen;
mod win32;

/// What the session hands the window thread.
pub(crate) enum SurrogateCommand {
    /// A decoded tile to blit and present.
    Frame { seq: u32, dirty: Rect, raw: Vec<u8> },
    /// A raw H.264 packet, decoded **on the window thread**.
    ///
    /// Same reasoning as the GTK side: a video decoder is stateful and its
    /// pieces are not `Send`, while [`SessionModule`] requires `Send`. Sending
    /// the packet rather than the pixels also saves a copy, since the decoded
    /// frame is wanted exactly where the framebuffer lives.
    VideoPacket { seq: u32, packet: Vec<u8> },
    /// Window geometry changed; reallocate before the next blit.
    Resize { width: u32, height: u32 },
    /// The window grew, learnt from a tile rather than a full frame. Keeps the
    /// pixels already drawn — see [`Framebuffer::grow_to`].
    Grow { width: u32, height: u32 },
    /// A remote-screen view's stream starts over with another source: drop the
    /// decoder and the picture, keep the window.
    Restart { title: String },
    /// A remote-screen view's selector list changed.
    Sources(Vec<ScreenSource>),
    /// Tear the window down.
    Close(WindowCloseReason),
}

/// A surrogate window on Windows, driven from the session.
pub struct Win32Surrogate {
    id: WindowId,
    width: u32,
    height: u32,
    commands: mpsc::UnboundedSender<SurrogateCommand>,
    input: mpsc::UnboundedReceiver<WindowInput>,
    presented: mpsc::UnboundedReceiver<u32>,
    acks: PresentationAcks,
    video_started: bool,
    /// Set by the window thread when the user closes the window, so the session
    /// learns about it even before the channels drop.
    closed: Arc<AtomicBool>,
    /// Set for a remote-screen view: what the user picked in its selector.
    picks: Option<mpsc::UnboundedReceiver<SourcePick>>,
}

impl Surrogate for Win32Surrogate {
    fn apply_frame(&mut self, frame: &WindowFrame) -> Result<(), WindowError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(WindowError::Backend("surrogate window was closed".into()));
        }

        match geometry_of(frame.dirty, frame.codec, self.width, self.height) {
            Geometry::Unchanged => {}
            Geometry::Resize { width, height } => {
                (self.width, self.height) = (width, height);
                let _ = self.commands.send(SurrogateCommand::Resize { width, height });
            }
            Geometry::Grow { width, height } => {
                (self.width, self.height) = (width, height);
                let _ = self.commands.send(SurrogateCommand::Grow { width, height });
            }
        }

        if frame.codec == TileCodec::LossyVideo {
            if !self.video_started {
                self.video_started = true;
                tracing::info!(id = self.id, "host switched this window to a video stream");
            }
            self.commands
                .send(SurrogateCommand::VideoPacket {
                    seq: frame.seq,
                    packet: frame.data.clone(),
                })
                .map_err(|_| WindowError::Backend("surrogate window is gone".into()))?;
            return Ok(());
        }

        let expected = (frame.dirty.width as usize) * (frame.dirty.height as usize) * 4;
        // Decoded here, off the window thread, so the message loop stays
        // responsive — the same discipline as the GTK surrogate.
        let raw = decode_tile(frame.codec, &frame.data, expected)?;
        self.commands
            .send(SurrogateCommand::Frame {
                seq: frame.seq,
                dirty: frame.dirty,
                raw,
            })
            .map_err(|_| WindowError::Backend("surrogate window is gone".into()))?;

        Ok(())
    }

    fn input(&mut self) -> &mut mpsc::UnboundedReceiver<WindowInput> {
        &mut self.input
    }

    fn take_ack(&mut self) -> Option<WindowFrameAck> {
        while let Ok(seq) = self.presented.try_recv() {
            self.acks.note_presented(seq);
        }
        self.acks.take()
    }

    fn restart_stream(&mut self, title: &str) {
        self.video_started = false;
        self.acks = PresentationAcks::new(self.id);
        let _ = self.commands.send(SurrogateCommand::Restart {
            title: title.to_string(),
        });
    }

    fn set_sources(&mut self, sources: &[ScreenSource]) {
        let _ = self
            .commands
            .send(SurrogateCommand::Sources(sources.to_vec()));
    }

    fn take_source_pick(&mut self) -> Option<SourcePick> {
        self.picks.as_mut()?.try_recv().ok()
    }

    fn close(&mut self, reason: WindowCloseReason) {
        tracing::info!(?reason, id = self.id, "closing surrogate window");
        let _ = self.commands.send(SurrogateCommand::Close(reason));
    }
}

impl Drop for Win32Surrogate {
    fn drop(&mut self) {
        let _ = self
            .commands
            .send(SurrogateCommand::Close(WindowCloseReason::ViewerClosed));
    }
}

/// The Windows [`WindowRenderer`] backend: one Win32 window per transfer.
pub struct Win32WindowRenderer;

impl WindowRenderer for Win32WindowRenderer {
    type Surrogate = Win32Surrogate;

    fn open(&mut self, meta: &WindowMeta) -> Result<Self::Surrogate, WindowError> {
        open_surrogate(meta, false)
    }

    /// An unseen window that capture apps still list and record; see
    /// [`unseen`] for the measurements behind "still".
    fn open_screen_view(&mut self, meta: &WindowMeta) -> Result<Self::Surrogate, WindowError> {
        open_surrogate(meta, true)
    }
}

fn open_surrogate(meta: &WindowMeta, view: bool) -> Result<Win32Surrogate, WindowError> {
    let (picks_tx, picks_rx) = mpsc::unbounded_channel();
    let selector = view.then_some(picks_tx);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (presented_tx, presented_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let closed = Arc::new(AtomicBool::new(false));

    let meta = meta.clone();
    let thread_meta = meta.clone();
    let thread_closed = closed.clone();
    std::thread::Builder::new()
        .name(format!("cursio-surrogate-{}", meta.id))
        .spawn(move || {
            win32::run(
                thread_meta,
                selector,
                cmd_rx,
                input_tx,
                presented_tx,
                ready_tx,
                thread_closed,
            );
        })
        .map_err(|e| WindowError::Backend(format!("spawn surrogate thread: {e}")))?;

    // Block until the window exists, so a failure surfaces from `open()`
    // rather than as a window that never appears.
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Win32Surrogate {
            id: meta.id,
            width: meta.width,
            height: meta.height,
            commands: cmd_tx,
            input: input_rx,
            presented: presented_rx,
            acks: PresentationAcks::new(meta.id),
            video_started: false,
            closed,
            picks: view.then_some(picks_rx),
        }),
        Ok(Err(e)) => Err(WindowError::Backend(e)),
        Err(_) => Err(WindowError::Backend(
            "surrogate thread ended before signalling readiness".into(),
        )),
    }
}
