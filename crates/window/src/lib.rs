//! Cursio window transfer — capture one window's pixels, show it on a peer.
//!
//! The "drag a window to the other machine" feature. Unlike a full remote
//! desktop, exactly one window is streamed, and the application stays running
//! on its original host — only pixels go out and input comes back (the
//! RemoteApp / Xpra "seamless single window" model, but bidirectional and
//! cross-OS). The neutral wire vocabulary lives in
//! [`cursio_protocol::window`]; this crate is the platform layer that produces
//! and consumes it.
//!
//! Two roles, each behind a trait so platform backends slot in the same way
//! [`InputInjector`](cursio_input) does — Linux (portal/PipeWire capture,
//! Wayland surrogate) and Windows (Graphics Capture, Win32 surrogate) can be
//! added without touching the session code that drives them:
//!
//! - **Host** ([`WindowCapture`]): given a real local window, produce a
//!   [`WindowMeta`] and a stream of [`WindowFrame`]s (dirty regions), and apply
//!   inbound [`WindowInput`] to the real window.
//! - **Viewer** ([`WindowRenderer`]): open a surrogate window from a
//!   [`WindowMeta`], blit incoming [`WindowFrame`]s into it, and emit
//!   [`WindowInput`] from the user's interaction with the surrogate.
//!
//! No backend is wired yet: this milestone fixes the traits and the data flow
//! so the protocol and the session integration can be built against a stable
//! shape. See `docs/window-transfer-design.md` for the model, the traffic /
//! latency budget, and the milestones.

use cursio_protocol::{
    WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput, WindowMeta,
};

pub mod codec;
/// Diffing and codec routing, shared by every capture backend.
pub mod frame_pipeline;
/// The viewer's assembled window image, shared by every surrogate backend.
pub mod framebuffer;
pub mod window_test_support;

#[cfg(any(target_os = "linux", target_os = "windows", test))]
mod presentation_ack;

/// The platform-independent half of the H.264 path (what a decoder returns,
/// and where a stream may be entered), shared by the Linux and Windows
/// decoders.
pub mod video;

#[cfg(target_os = "linux")]
pub mod video_decoder;
#[cfg(target_os = "linux")]
pub mod video_encoder;

/// The neutral vocabulary for "the user is dragging a window", shared by both
/// platforms' detectors.
pub mod drag;
/// Deciding when a drag past the screen edge becomes a transfer.
pub mod drag_trigger;

/// Watching for window drags on GNOME, so dragging a window past the edge can
/// start a transfer. Optional: needs the Shell extension in
/// `native/cursio-shell-extension/`, and degrades to nothing without it.
#[cfg(target_os = "linux")]
pub mod linux_drag;

#[cfg(target_os = "linux")]
mod linux_capture;
#[cfg(target_os = "linux")]
pub use linux_capture::{PortalCaptureSession, PortalWindowCapture};

#[cfg(target_os = "macos")]
mod macos_capture;
#[cfg(target_os = "macos")]
pub mod macos_drag;
#[cfg(target_os = "macos")]
pub use macos_capture::{MacCaptureSession, MacWindowCapture, MacWindowRenderer};

#[cfg(target_os = "windows")]
pub mod video_decoder_win;
#[cfg(target_os = "windows")]
pub mod video_encoder_win;

/// Detecting a window drag on Windows, via `GetGUIThreadInfo`. No extension
/// needed here — the OS answers this directly.
#[cfg(target_os = "windows")]
pub mod windows_drag;

#[cfg(target_os = "windows")]
mod windows_capture;
#[cfg(target_os = "windows")]
mod windows_input;
#[cfg(target_os = "windows")]
pub use windows_capture::{WgcCaptureSession, WgcWindowCapture};

#[cfg(target_os = "windows")]
mod windows_surrogate;
#[cfg(target_os = "windows")]
pub use windows_surrogate::{Win32Surrogate, Win32WindowRenderer};

#[cfg(target_os = "linux")]
mod linux_surrogate;
#[cfg(target_os = "linux")]
pub use linux_surrogate::{GtkSurrogate, GtkWindowRenderer};

/// Errors from a window-transfer backend (capture or render).
#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    /// The requested window doesn't exist or is no longer capturable.
    #[error("window not found or gone: {0}")]
    WindowGone(String),
    /// The platform capture/render facility failed or is unavailable here.
    #[error("window backend failure: {0}")]
    Backend(String),
    /// A feature isn't implemented for this platform/backend yet.
    #[error("window transfer not supported on this platform yet")]
    Unsupported,
}

/// A running capture of one local window (the **host** side).
///
/// A backend implements this to turn a real OS window into the neutral frame
/// stream. Frames are delivered on the [`frames`](CaptureSession::frames)
/// channel (dirty regions, streamed as the window changes); inbound viewer
/// input is applied via [`apply_input`](CaptureSession::apply_input); the
/// [`meta`](CaptureSession::meta) is the offer to send before any frame.
///
/// Delivery uses channels rather than a callback so a slow link can't stall the
/// capture thread — same pattern as [`CaptureHandle`](cursio_input) for input.
pub trait CaptureSession {
    /// The offer describing this window, to send as
    /// [`Message::WindowOffer`](cursio_protocol::Message::WindowOffer) before
    /// any frames.
    fn meta(&self) -> &WindowMeta;

    /// The channel of pixel updates for this window. The first frame is a full
    /// window; subsequent frames carry only changed regions.
    fn frames(&mut self) -> &mut tokio::sync::mpsc::Receiver<WindowFrame>;

    /// Apply one viewer input event (in the window's own coordinates) to the
    /// real window on this host.
    fn apply_input(&mut self, input: WindowInput) -> Result<(), WindowError>;

    /// Feed back the viewer's latest [`WindowFrameAck`] so the backend's encoder
    /// can adapt: knowing the sent-vs-acked gap and RTT, it dials `fps`, tile
    /// quality, and [`TileCodec`](cursio_protocol::TileCodec) up or down to fit
    /// the link instead of queuing stale frames. A backend may ignore this
    /// while bringing capture up (it just won't adapt yet), but the plumbing is
    /// here from the start so the adaptive loop needs no protocol change later.
    fn on_ack(&mut self, ack: WindowFrameAck);

    /// Stop capturing and release the window. After this the session is done;
    /// callers should send a [`Message::WindowClose`](cursio_protocol::Message::WindowClose).
    fn close(&mut self, reason: WindowCloseReason);
}

/// Maximum encoded packets waiting between a capture backend and the session.
///
/// The capture side never waits on this queue. When it fills, the backend drops
/// the update and resets its differ/encoder so the next accepted update is
/// independently decodable instead of extending latency with stale frames.
pub const CAPTURE_FRAME_QUEUE_CAPACITY: usize = 8;

/// Selects and starts capturing a local window (the **host** side).
///
/// The entry point a backend provides. `begin` picks the window to transfer —
/// for the first cut, the window currently under the cursor / being dragged —
/// and returns a live [`CaptureSession`]. `id` is the session-scoped
/// [`WindowId`] the host assigns; the backend stamps it into the meta and every
/// frame so the viewer can address the right surrogate.
pub trait WindowCapture {
    type Session: CaptureSession;

    /// Begin transferring the window the user is dragging out. Returns a running
    /// capture, or an error if no suitable window is available.
    fn begin(&mut self, id: WindowId) -> Result<Self::Session, WindowError>;

    /// Aim the next [`begin`](Self::begin) at the window whose title contains
    /// this.
    ///
    /// The drag detector knows which window the user grabbed and the capture
    /// does not, so it has to be told. Leaving it out worked on Windows by
    /// accident — the window being dragged is the foreground one, which is what
    /// an empty hint selects there — and could never work on macOS, where an
    /// empty hint means the whole screen. Default is a no-op, for a backend
    /// that picks its window some other way (the Wayland portal opens its own
    /// chooser, and no title of ours would be honoured).
    fn aim_at(&mut self, _title: &str) {}
}

/// A surrogate window shown on the **viewer** side.
///
/// A backend implements this to display a transferred window: it opens a local
/// window sized/titled from the [`WindowMeta`], blits inbound frames, and
/// surfaces the user's interaction as [`WindowInput`] to send back to the host.
pub trait Surrogate {
    /// Blit one incoming pixel update into the surrogate window.
    fn apply_frame(&mut self, frame: &WindowFrame) -> Result<(), WindowError>;

    /// The channel of input the user generates against this surrogate, to
    /// forward to the host as
    /// [`Message::WindowInput`](cursio_protocol::Message::WindowInput).
    fn input(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<WindowInput>;

    /// The ack this surrogate wants to send back for rate control, if one is due
    /// — the highest `seq` it has presented since the last ack. Returns `None`
    /// when nothing new is worth reporting, so the caller sends acks sparingly
    /// (a few per second) rather than one per frame. See [`WindowFrameAck`].
    fn take_ack(&mut self) -> Option<WindowFrameAck>;

    /// Tear down the surrogate window (host closed, user dismissed, or returned).
    fn close(&mut self, reason: WindowCloseReason);
}

/// Opens surrogate windows from host offers (the **viewer** side).
///
/// The entry point a viewer backend provides: given a [`WindowMeta`] received
/// as a [`Message::WindowOffer`](cursio_protocol::Message::WindowOffer), create
/// the local surrogate window to mirror it.
pub trait WindowRenderer {
    type Surrogate: Surrogate;

    /// Open a surrogate window for a newly offered transfer.
    fn open(&mut self, meta: &WindowMeta) -> Result<Self::Surrogate, WindowError>;
}
