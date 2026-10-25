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
    ScreenSource, WindowCloseReason, WindowFrame, WindowFrameAck, WindowInput, WindowMeta,
};
use tokio::sync::mpsc;

use crate::surrogate_front::SurrogateFront;
use crate::{SourcePick, Surrogate, WindowError, WindowRenderer};

mod unseen;
mod win32;

pub use unseen::set_icons_shown;

/// What only a Win32 surrogate is told: a remote-screen view switching what it
/// shows.
pub(crate) enum Win32Command {
    /// The view's stream starts over with another source: drop the decoder and
    /// the picture, keep the window.
    Restart { title: String },
    /// The view's selector list changed.
    Sources(Vec<ScreenSource>),
}

/// What the session hands the window thread.
pub(crate) type SurrogateCommand = crate::surrogate_front::SurrogateCommand<Win32Command>;

/// A surrogate window on Windows, driven from the session.
pub struct Win32Surrogate {
    front: SurrogateFront<Win32Command>,
    input: mpsc::UnboundedReceiver<WindowInput>,
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
        self.front.apply_frame(frame)
    }

    fn input(&mut self) -> &mut mpsc::UnboundedReceiver<WindowInput> {
        &mut self.input
    }

    fn take_ack(&mut self) -> Option<WindowFrameAck> {
        self.front.take_ack()
    }

    fn take_keyframe_request(&mut self) -> bool {
        self.front.take_keyframe_request()
    }

    fn restart_stream(&mut self, title: &str) {
        self.front.restart();
        self.front.platform(Win32Command::Restart {
            title: title.to_string(),
        });
    }

    fn set_sources(&mut self, sources: &[ScreenSource]) {
        self.front.platform(Win32Command::Sources(sources.to_vec()));
    }

    fn take_source_pick(&mut self) -> Option<SourcePick> {
        self.picks.as_mut()?.try_recv().ok()
    }

    fn close(&mut self, reason: WindowCloseReason) {
        self.front.close(reason);
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
    let wants_keyframe = Arc::new(AtomicBool::new(false));
    let thread_wants_keyframe = wants_keyframe.clone();

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
                thread_wants_keyframe,
            );
        })
        .map_err(|e| WindowError::Backend(format!("spawn surrogate thread: {e}")))?;

    // Block until the window exists, so a failure surfaces from `open()`
    // rather than as a window that never appears.
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Win32Surrogate {
            front: SurrogateFront::new(
                meta.id,
                (meta.width, meta.height),
                cmd_tx,
                presented_rx,
                wants_keyframe,
            ),
            input: input_rx,
            closed,
            picks: view.then_some(picks_rx),
        }),
        Ok(Err(e)) => Err(WindowError::Backend(e)),
        Err(_) => Err(WindowError::Backend(
            "surrogate thread ended before signalling readiness".into(),
        )),
    }
}
