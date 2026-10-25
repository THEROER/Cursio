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
    ScreenSource, SourceId, VideoCaps, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId,
    WindowInput, WindowMeta,
};

/// What this machine's H.264 decoder takes, for the peer's encoder.
///
/// FFmpeg decodes High 4:4:4 Predictive; Media Foundation silently does not
/// (see [`VideoCaps`]), and macOS has no viewer yet.
pub const LOCAL_VIDEO_CAPS: VideoCaps = VideoCaps {
    lossless: cfg!(target_os = "linux"),
};

pub mod codec;
/// Diffing and codec routing, shared by every capture backend.
pub mod frame_pipeline;
/// The viewer's assembled window image, shared by every surrogate backend.
pub mod framebuffer;
/// What the backends found that the user has to fix, for the core to list.
pub mod notice;
pub mod window_test_support;

#[cfg(any(target_os = "linux", target_os = "windows", test))]
mod presentation_ack;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod surrogate_front;

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
mod pipewire_pod;
#[cfg(target_os = "linux")]
pub use linux_capture::{PortalCaptureSession, PortalScreenSources, PortalWindowCapture};

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
pub use windows_capture::{WgcCaptureSession, WgcScreenSources, WgcWindowCapture};

#[cfg(target_os = "windows")]
mod windows_surrogate;
#[cfg(target_os = "windows")]
pub use windows_surrogate::{Win32Surrogate, Win32WindowRenderer};

/// Whether remote-screen views keep their own notification-area icon. Off
/// while the Cursio app is attached: its tray carries every view's selector.
/// Views exist as windows on Windows only; elsewhere there is no icon.
pub fn set_remote_screen_icons_shown(shown: bool) {
    #[cfg(target_os = "windows")]
    windows_surrogate::set_icons_shown(shown);
    #[cfg(not(target_os = "windows"))]
    let _ = shown;
}

#[cfg(target_os = "linux")]
mod linux_surrogate;
#[cfg(target_os = "linux")]
pub use linux_surrogate::{GtkSurrogate, GtkWindowRenderer, LinuxSurrogate};

/// A peer's screen source published as a PipeWire node instead of a window.
#[cfg(target_os = "linux")]
mod linux_screen_source;
#[cfg(target_os = "linux")]
pub use linux_screen_source::PipeWireScreenSource;

/// The dialog shown when a capture app asks the portal for a screen.
#[cfg(target_os = "linux")]
pub mod linux_chooser;
/// Cursio's own ScreenCast portal backend, which adds the peers' sources.
#[cfg(target_os = "linux")]
pub mod linux_portal;

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

    /// Aggregate network budget allocated to this capture, in bits/second.
    /// Backends with adaptive encoders apply it in addition to presentation ACKs.
    fn set_network_budget(&mut self, _bits_per_second: u32, _congested: bool) {}

    /// Resize the real window, because the viewer resized its surrogate.
    ///
    /// Advisory in both directions: the host may clamp the request to what the
    /// window will accept, and the viewer learns the outcome only from the
    /// frames that follow, never from a reply. Default is a no-op for a backend
    /// that cannot resize its source — the viewer then keeps scaling the
    /// picture, which is the old behaviour rather than a new failure.
    fn request_resize(&mut self, _width: u32, _height: u32) -> Result<(), WindowError> {
        Ok(())
    }

    /// What the viewer's decoder takes, so the encoder picks a stream that
    /// will show there. Until told, a backend must assume only the High
    /// profile. Default is a no-op for a backend with one kind of stream.
    fn set_peer_video(&mut self, _caps: VideoCaps) {}

    /// The viewer has no picture it can build on — its decoder is waiting for
    /// a keyframe, or it just started — so send the whole current picture now,
    /// even if the source is not changing. Default is a no-op for a backend
    /// whose every frame is already whole.
    fn request_keyframe(&mut self) {}

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

    /// Whether the user closed this surrogate's window, so the host should be
    /// told and stop streaming. The input channel closing says the same, but
    /// only once every sender of it is dropped — which a toolkit holding on to
    /// a closed window's handlers may never do. Default `false`, for a backend
    /// whose input channel is the whole answer.
    fn is_gone(&self) -> bool {
        false
    }

    /// The ack this surrogate wants to send back for rate control, if one is due
    /// — the highest `seq` it has presented since the last ack. Returns `None`
    /// when nothing new is worth reporting, so the caller sends acks sparingly
    /// (a few per second) rather than one per frame. See [`WindowFrameAck`].
    fn take_ack(&mut self) -> Option<WindowFrameAck>;

    /// A size the user resized this surrogate to, to ask the host for, if one
    /// is pending.
    ///
    /// Coalesced by the backend rather than reported per motion event: a drag
    /// across the screen produces a continuous stream of sizes, and asking the
    /// host for every one of them would put a resize storm on the same channel
    /// the cursor rides. Default is `None` for a backend whose window the user
    /// cannot resize.
    fn take_resize_request(&mut self) -> Option<(u32, u32)> {
        None
    }

    /// Whether this surrogate's decoder is stuck without a keyframe — waiting
    /// for its first, or restarted after a failure — and the host should be
    /// asked for the whole picture. Taking it clears it; the decoder sets it
    /// again for every packet it still has to drop, so the caller decides how
    /// often to ask. Default is `false` for a surrogate with no decoder.
    fn take_keyframe_request(&mut self) -> bool {
        false
    }

    /// The stream behind this surrogate is about to start over with a
    /// different source; `title` names it.
    ///
    /// Only a remote-screen view is ever told this. Its window must survive a
    /// source switch — a screen-sharing application capturing it would lose
    /// the stream if it were recreated — so the decoder and the picture are
    /// reset in place instead. Default is a no-op for a surrogate that is only
    /// ever one window's.
    fn restart_stream(&mut self, _title: &str) {}

    /// The host's current shareable sources, for the view's selector.
    fn set_sources(&mut self, _sources: &[ScreenSource]) {}

    /// A source the user picked in the view's selector, if one is pending.
    fn take_source_pick(&mut self) -> Option<SourcePick> {
        None
    }

    /// The PipeWire node this view publishes, for a view that is a node
    /// rather than a window (Linux); `None` for a window.
    fn node_id(&self) -> Option<u32> {
        None
    }

    /// Tear down the surrogate window (host closed, user dismissed, or returned).
    fn close(&mut self, reason: WindowCloseReason);
}

/// What the user asked for in a remote-screen view's selector.
///
/// `Next`/`Previous` stay relative here rather than being resolved by the
/// window, because the window thread holds a copy of the list that may be a
/// refresh behind; the session resolves them against the list it last
/// received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePick {
    Source(SourceId),
    Next,
    Previous,
    Stop,
}

/// The sources a host can stream into a peer's remote-screen view.
///
/// Separate from [`WindowCapture`] because it answers a different question.
/// A drag names its window, so `begin` can be told which one; a remote-screen
/// request names a source out of a list this host published, and the capture
/// it starts may block for as long as a portal picker stays open on the host.
/// `open` therefore runs on a thread of its own, which is why this trait is
/// `Send + Sync` and hands back a boxed session rather than borrowing `&mut`
/// state the session loop owns.
pub trait ScreenSources: Send + Sync {
    /// What can be shown right now. Called every couple of seconds while a
    /// viewer is subscribed, on a thread of its own: enumerating may wait on
    /// other applications' message loops — Windows answers a window's title
    /// by asking the thread that owns it — so it is never on the session loop.
    fn list(&self) -> Result<Vec<ScreenSource>, WindowError>;

    /// Start capturing `source`, stamping `id` into its meta and frames.
    ///
    /// May block (a picker on the host); never called on the session loop.
    fn open(
        &self,
        id: WindowId,
        source: &ScreenSource,
    ) -> Result<Box<dyn CaptureSession + Send>, WindowError>;
}

impl CaptureSession for Box<dyn CaptureSession + Send> {
    fn meta(&self) -> &WindowMeta {
        (**self).meta()
    }

    fn frames(&mut self) -> &mut tokio::sync::mpsc::Receiver<WindowFrame> {
        (**self).frames()
    }

    fn apply_input(&mut self, input: WindowInput) -> Result<(), WindowError> {
        (**self).apply_input(input)
    }

    fn on_ack(&mut self, ack: WindowFrameAck) {
        (**self).on_ack(ack)
    }

    fn request_resize(&mut self, width: u32, height: u32) -> Result<(), WindowError> {
        (**self).request_resize(width, height)
    }

    fn set_peer_video(&mut self, caps: VideoCaps) {
        (**self).set_peer_video(caps)
    }

    fn request_keyframe(&mut self) {
        (**self).request_keyframe()
    }

    fn close(&mut self, reason: WindowCloseReason) {
        (**self).close(reason)
    }
}

/// Opens surrogate windows from host offers (the **viewer** side).
///
/// The entry point a viewer backend provides: given a [`WindowMeta`] received
/// as a [`Message::WindowOffer`](cursio_protocol::Message::WindowOffer), create
/// the local surrogate window to mirror it.
/// A viewer for a machine that only hosts: a phone streams its own screen and
/// has nowhere to show a computer's window, so an offer is refused with that
/// reason rather than left unanswered.
pub struct NoViewRenderer;

impl WindowRenderer for NoViewRenderer {
    type Surrogate = NoSurrogate;

    fn open(&mut self, _meta: &WindowMeta) -> Result<Self::Surrogate, WindowError> {
        Err(WindowError::Backend(
            "this machine shows its own screen to others but cannot show theirs".into(),
        ))
    }
}

/// Never constructed; it exists to satisfy [`NoViewRenderer`]'s associated type.
pub enum NoSurrogate {}

impl Surrogate for NoSurrogate {
    fn apply_frame(&mut self, _frame: &WindowFrame) -> Result<(), WindowError> {
        match *self {}
    }

    fn input(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<WindowInput> {
        match *self {}
    }

    fn take_ack(&mut self) -> Option<WindowFrameAck> {
        match *self {}
    }

    fn close(&mut self, _reason: WindowCloseReason) {
        match *self {}
    }
}

pub trait WindowRenderer {
    type Surrogate: Surrogate;

    /// Open a surrogate window for a newly offered transfer.
    fn open(&mut self, meta: &WindowMeta) -> Result<Self::Surrogate, WindowError>;

    /// Open a remote-screen view: a window that shows whichever of the peer's
    /// sources the user picks, and keeps existing across every switch.
    ///
    /// It differs from a transfer's surrogate in three ways. It is view-only —
    /// the shared cursor already drives the peer directly, and a monitor has
    /// no window to replay a click into. It letterboxes the picture instead of
    /// resizing itself to each source, since a screen-sharing application
    /// capturing it must see one stable window. And its selector (a context
    /// menu and keys) lives outside the picture, so choosing a source never
    /// shows up in the captured pixels. Default refuses, for a platform with
    /// no viewer at all.
    fn open_screen_view(&mut self, _meta: &WindowMeta) -> Result<Self::Surrogate, WindowError> {
        Err(WindowError::Unsupported)
    }
}
