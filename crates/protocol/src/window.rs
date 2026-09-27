//! Window-transfer wire types.
//!
//! Cursio can "drag a window" from one machine to another: you grab a window's
//! titlebar, pull it past the screen edge, and it appears as a window on the
//! peer. The application keeps **running on its original host** — nothing about
//! its process, memory, or state is migrated. What moves is a *live view*: the
//! host streams the window's pixels to the peer, and the peer streams input
//! back. This is the RemoteApp / Xpra model (a single seamless remote window),
//! but bidirectional and cross-OS, riding the transport and pairing Cursio
//! already has.
//!
//! These types are the neutral vocabulary for that exchange, mirroring how
//! [`input`](crate::input) stays OS-agnostic:
//! - the **host** (source) captures a real window and emits [`WindowMeta`] +
//!   [`WindowFrame`]s;
//! - the **peer** (viewer) shows a surrogate window from those frames and emits
//!   [`WindowInput`] back, addressed in that window's own coordinate space.
//!
//! Nothing here knows how pixels are captured or drawn — that lives in the
//! `cursio-window` crate behind traits, the same split as `cursio-input`.

use serde::{Deserialize, Serialize};

/// Identifies one transferred window within a session.
///
/// Allocated by the **host** when it offers a window and echoed by the viewer
/// on every frame/input for that window, so a session can carry several
/// transferred windows at once without ambiguity. Session-scoped and opaque —
/// it is *not* the OS window handle (that never leaves the host).
pub type WindowId = u32;

/// How a [`WindowFrame`]'s tile payload is encoded.
///
/// **Why per-tile and not per-session:** raw pixels can't ride a real link at
/// video rates — one full 1080p BGRA frame is ~8.3 MiB, so 60 fps of it is
/// ~4 Gbit/s, past even gigabit LAN. But a mostly-static UI wants *lossless*
/// on its few small dirty rects (crisp text), while a playing video or a scroll
/// wants *lossy* on its large ones (or it drowns the channel). Those pull in
/// opposite directions, so the host tags each tile with the codec it chose for
/// *that* region — text stays sharp, motion stays cheap — and the viewer
/// decodes accordingly. This is the knob the adaptive controller turns under
/// bandwidth pressure (see [`WindowFrameAck`]).
///
/// Ordered from most faithful / most expensive to least. Only the first two are
/// implemented in the initial cut; the lossy variants are placeholders so the
/// wire format is stable before an encoder is wired in — callers never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileCodec {
    /// Raw 32-bit BGRA, 4 bytes/pixel, no compression. What both Windows capture
    /// and Wayland `shm` hand back most readily. Only sane for tiny tiles or the
    /// very first frame during bring-up; never for a steady stream.
    RawBgra8888,
    /// Lossless byte compression of the BGRA tile (e.g. zstd/lz4). Cheap CPU,
    /// pixel-exact — the default for small text/UI updates.
    LosslessBgra8888,
    /// Lossy still-image encoding of the tile (e.g. JPEG-class). For large
    /// updates where exactness doesn't matter (photos, gradients) and lossless
    /// wouldn't shrink enough. Placeholder until an encoder is added.
    LossyImage,
    /// A slice of a lossy inter-frame video codec (e.g. H.264/VP9), referencing
    /// prior frames so unchanged pixels cost ~nothing. For video/scroll/anim.
    /// The heaviest lever the adaptive controller reaches for. Placeholder.
    LossyVideo,
}

impl TileCodec {
    /// Whether this codec reproduces the source pixels exactly. Lossless codecs
    /// are safe to use for text/UI; lossy ones trade fidelity for bytes.
    pub fn is_lossless(self) -> bool {
        matches!(self, TileCodec::RawBgra8888 | TileCodec::LosslessBgra8888)
    }
}

/// A rectangle in a window's own pixel coordinates (origin = top-left of the
/// window's content, +x right, +y down).
///
/// Used both to describe the changed ("dirty") area a [`WindowFrame`] carries
/// and, in principle, any sub-region reference. Kept as plain `u32`s so it is
/// cheap to serialize and has no platform baggage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Announces a window the host is starting to transfer.
///
/// Sent once (in [`Message::WindowOffer`](crate::Message::WindowOffer)) before
/// any frames, so the viewer can create a correctly sized surrogate window and
/// label it. The `title` is display text only; the host's real window handle is
/// never exposed. `width`/`height` are the window's content size in pixels at
/// offer time — later size changes ride along with frames (a frame whose dirty
/// rect covers a new full size), so this is the initial geometry, not a fixed
/// contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowMeta {
    /// Host-allocated id for this transfer; echoed on every related message.
    pub id: WindowId,
    /// Human-readable window title, for the surrogate's titlebar/UI.
    pub title: String,
    /// Initial content width in pixels.
    pub width: u32,
    /// Initial content height in pixels.
    pub height: u32,
}

/// One update of a transferred window's pixels.
///
/// Carries only the changed region (`dirty`) rather than the whole window, so a
/// mostly-static window costs little to keep live — the host diffs successive
/// captures and sends just what moved. `data` is the `dirty` sub-rect encoded
/// with `codec`: for [`TileCodec::RawBgra8888`] that's `dirty.width *
/// dirty.height * 4` bytes row-major with no padding, and the viewer blits it at
/// `(dirty.x, dirty.y)`; for a compressed/lossy codec it's the encoded payload
/// the viewer must decode first. Splitting a big change into several tiles (each
/// its own `WindowFrame` with a distinct `dirty` and possibly a distinct
/// `codec`) is how the host keeps any single frame under the transport's
/// [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN) and mixes lossless text with lossy
/// motion in the same visual update.
///
/// A frame that resizes the window sends a `dirty` covering the full new size.
/// The very first frame after a [`WindowMeta`] is expected to be a full-window
/// update so the viewer has complete initial contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowFrame {
    /// Which transferred window this frame updates.
    pub id: WindowId,
    /// Monotonic per-window frame counter, so the viewer can ack the newest it
    /// has (see [`WindowFrameAck`]) and detect drops/reordering. Tiles that make
    /// up one visual update may share a `seq`.
    pub seq: u32,
    /// The changed region, in the window's own pixel coordinates.
    pub dirty: Rect,
    /// How `data` is encoded.
    pub codec: TileCodec,
    /// The `dirty` region encoded per `codec`.
    pub data: Vec<u8>,
}

/// Viewer -> host feedback for adaptive rate control.
///
/// The host can't just fire frames blindly: if it out-runs the link, an
/// unbounded queue builds and latency grows without bound (the same failure the
/// input path's motion-coalescing avoids). So the viewer periodically reports
/// the newest frame it has actually presented, and the host uses the gap
/// between what it *sent* and what's been *acked* — plus the round-trip time —
/// to estimate available bandwidth and dial `fps` / tile quality / codec
/// choice up or down. This is the closed loop that lets us aim for low latency
/// **without** drowning the channel: under pressure the host degrades quality,
/// never buffers stale frames.
///
/// Sent occasionally (not per frame — that would itself cost bandwidth), e.g.
/// a few times a second or whenever a `seq` jump is worth reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowFrameAck {
    /// Which transferred window this acks.
    pub id: WindowId,
    /// The highest [`WindowFrame::seq`] the viewer has decoded and presented.
    pub acked_seq: u32,
}

/// Input the viewer sends back to the host for a transferred window.
///
/// The peer owns the surrogate window, so it knows where inside that window the
/// pointer is; it reports **absolute** positions in the window's own coordinate
/// space (unlike shared-cursor [`InputEvent`](crate::InputEvent), which is
/// relative). The host maps `(x, y)` onto its real window and replays the
/// action there. Keeping this separate from `InputEvent` keeps the two flows —
/// "shared cursor moving between desktops" vs. "driving one remote window" —
/// from being confused for each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowInput {
    /// Pointer moved to `(x, y)` within the window's content.
    PointerMove { id: WindowId, x: u32, y: u32 },
    /// A mouse button changed state at `(x, y)` within the window.
    PointerButton {
        id: WindowId,
        x: u32,
        y: u32,
        button: crate::MouseButton,
        pressed: bool,
    },
    /// Scroll wheel motion while over the window. `dx`/`dy` in notches,
    /// positive y = up, matching [`InputEvent::PointerScroll`](crate::InputEvent).
    PointerScroll { id: WindowId, dx: i32, dy: i32 },
    /// A key changed state while the window is focused. `key` is the same
    /// platform-neutral evdev keycode namespace as
    /// [`InputEvent::Key`](crate::InputEvent).
    Key {
        id: WindowId,
        key: u16,
        pressed: bool,
    },
}

impl WindowInput {
    /// Which transferred window this event belongs to.
    ///
    /// Every variant carries the id, but they carry it separately — a host
    /// showing more than one window needs it to know which one the user is
    /// actually driving.
    pub fn id(&self) -> WindowId {
        match self {
            Self::PointerMove { id, .. }
            | Self::PointerButton { id, .. }
            | Self::PointerScroll { id, .. }
            | Self::Key { id, .. } => *id,
        }
    }
}

/// Why a window transfer ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowCloseReason {
    /// The host's real window closed, so the transfer can't continue.
    HostClosed,
    /// A user dismissed the surrogate on the viewer side.
    ViewerClosed,
    /// The user "dragged it back": the window returns to living only on the host.
    Returned,
}

/// The [`WindowId`] every remote-screen stream travels under.
///
/// Reserved rather than allocated because a remote-screen stream is not a
/// transfer: there is at most one per direction per link, it outlives every
/// source it shows, and both machines may run one towards the other at once.
/// Frames only ever flow host → viewer, so a frame bearing this id always
/// belongs to *our* view of the peer's screen, and an ack bearing it always to
/// the stream *we* host — the direction disambiguates what a shared id cannot.
/// Drag transfers allocate upwards from 1 and never reach it.
pub const SCREEN_STREAM_ID: WindowId = u32::MAX;

/// Identifies one shareable source in a host's [`ScreenSource`] list.
///
/// Chosen by the host and stable only for as long as the source exists; the
/// viewer echoes it back verbatim and never interprets it.
pub type SourceId = u32;

/// Whether a [`ScreenSource`] is one application window or a whole monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    Window,
    Monitor,
}

/// One thing a host can stream into a peer's remote-screen view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSource {
    pub id: SourceId,
    pub kind: SourceKind,
    /// Display text for the viewer's selector.
    pub title: String,
    /// Size in pixels as the host last saw it; `0` when unknown until capture
    /// starts (a portal-chosen source).
    pub width: u32,
    pub height: u32,
    /// Choosing this opens a picker **on the host**, rather than naming a
    /// concrete source.
    ///
    /// GNOME refuses to tell a client which windows exist
    /// (`Shell.Introspect.GetWindows` is `AccessDenied`), so a Linux host can
    /// only offer "let the portal ask". Flagged so the viewer can say where the
    /// dialog will appear instead of the stream seeming to hang.
    pub picked_on_host: bool,
}

/// The window module's **control** vocabulary, carried inside
/// [`Message::Module`](crate::Message::Module).
///
/// Deliberately *not* everything the module sends: per-frame pixels
/// ([`Message::WindowFrame`](crate::Message::WindowFrame)) and surrogate input
/// ([`Message::WindowInput`](crate::Message::WindowInput)) stay first-class
/// variants, because enveloping them would add a serialize and a `Vec<u8>` copy
/// to every frame and every pointer move — the two hot paths this module has.
///
/// What lives here is the low-rate control traffic: the offer that starts a
/// transfer, the ack that drives rate control, and the close that ends it. Those
/// are the parts most likely to grow (a resize, a title change, a codec
/// renegotiation), and growing them here versions this module alone. See
/// `docs/platform-design.md` for why the tiering is deliberate rather than a
/// half-finished migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowMsg {
    /// Host -> viewer: begin transferring a window. Sent once, before any frame.
    Offer(WindowMeta),
    /// Viewer -> host: newest frame presented, for adaptive rate control.
    FrameAck(WindowFrameAck),
    /// Viewer -> host: the user resized the surrogate; resize the real window.
    ///
    /// A request, not a statement of fact. The viewer does not change its own
    /// geometry on the strength of it: the host may clamp it to the window's
    /// minimum size, or refuse outright (a fixed-size dialog), and the truth
    /// arrives the ordinary way, as a full frame at the new size. Without this
    /// the surrogate could only stretch its texture over the new allocation —
    /// the picture grew, the pixels behind it did not, and the result was a
    /// blurry scale-up of a window that never actually resized.
    ResizeRequest {
        id: WindowId,
        width: u32,
        height: u32,
    },
    /// Either side: end a transfer. No frames or input for `id` are valid after.
    Close {
        id: WindowId,
        reason: WindowCloseReason,
    },
    /// Viewer -> host: send the list of shareable sources now, and again
    /// whenever it changes, for as long as this link lives.
    ///
    /// A subscription rather than a one-shot query because the list is what a
    /// right-click menu shows, and windows open and close while the viewer is
    /// up; polling from the viewer would cost a round trip per menu.
    ListSources,
    /// Host -> viewer: the current shareable sources.
    Sources(Vec<ScreenSource>),
    /// Host -> viewer: this host does not serve remote-screen views.
    ///
    /// Carries a reason because silence would be indistinguishable from a
    /// request that never arrived — the same rule as a service refusal.
    SourcesRefused { reason: String },
    /// Viewer -> host: stream this source into the view, replacing whatever it
    /// showed. Starting and switching are the same request.
    ShowSource { source: SourceId },
    /// Viewer -> host: stop streaming into the view; the view stays open.
    StopSource,
    /// Host -> viewer: the frames that follow under [`SCREEN_STREAM_ID`] are a
    /// new source. The viewer restarts its decoder and picture here and keeps
    /// its window.
    SourceShown {
        source: SourceId,
        title: String,
        width: u32,
        height: u32,
    },
    /// Host -> viewer: the view's stream ended (source closed, capture failed,
    /// picker cancelled). The window stays; it simply shows nothing.
    SourceEnded { reason: String },
    /// Either direction, once per link: what this machine's video decoder can
    /// take, so the peer encodes a stream that will actually show here.
    VideoCaps(VideoCaps),
}

/// What a viewer's H.264 decoder accepts, beyond the High profile every
/// decoder here takes.
///
/// Announced rather than assumed from the platform because the stream a host
/// would pick on its own is the best one *it* can make, and the best one is not
/// universally decodable: pixel-exact H.264 requires the High 4:4:4 Predictive
/// profile, which Media Foundation does not implement. It accepts the packets,
/// never emits a picture, and reports no error — so a Windows view of a Linux
/// host showed only the lossless tiles landing between video frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCaps {
    /// Decodes High 4:4:4 Predictive, the profile lossless H.264 needs.
    pub lossless: bool,
}

impl WindowMsg {
    /// Scheduling class for transfer control traffic.
    ///
    /// `Video`, matching the pixels it governs: an offer that outran its own
    /// frames, or a close that arrived before the last frame, would have the
    /// viewer acting on a window whose stream is in the wrong state. Keeping
    /// control in the same class as the frames keeps them ordered relative to
    /// each other.
    pub const PRIORITY: crate::Priority = crate::Priority::Video;

    /// Scheduling class for this particular message.
    ///
    /// The remote-screen requests and the source list are `Control`: none of
    /// them is ordered against pixels, and a source switch asked for while a
    /// monitor streams at full rate must not wait behind that monitor's
    /// frames. `SourceShown` and `SourceEnded` stay with the frames because
    /// they *are* ordered against them — the viewer must restart its decoder
    /// exactly between the old source's last packet and the new one's first.
    pub fn priority(&self) -> crate::Priority {
        match self {
            Self::ListSources
            | Self::Sources(_)
            | Self::SourcesRefused { .. }
            | Self::ShowSource { .. }
            | Self::StopSource
            | Self::VideoCaps(_) => crate::Priority::Control,
            _ => Self::PRIORITY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_and_lossless_are_lossless_others_not() {
        assert!(TileCodec::RawBgra8888.is_lossless());
        assert!(TileCodec::LosslessBgra8888.is_lossless());
        assert!(!TileCodec::LossyImage.is_lossless());
        assert!(!TileCodec::LossyVideo.is_lossless());
    }

    #[test]
    fn source_requests_jump_the_video_queue_but_a_switch_stays_with_its_frames() {
        use crate::Priority;
        assert_eq!(
            WindowMsg::ShowSource { source: 1 }.priority(),
            Priority::Control
        );
        assert_eq!(WindowMsg::ListSources.priority(), Priority::Control);
        assert_eq!(WindowMsg::Sources(Vec::new()).priority(), Priority::Control);
        let shown = WindowMsg::SourceShown {
            source: 1,
            title: String::new(),
            width: 0,
            height: 0,
        };
        assert_eq!(shown.priority(), Priority::Video);
        assert_eq!(
            WindowMsg::SourceEnded {
                reason: String::new()
            }
            .priority(),
            Priority::Video
        );
    }

    #[test]
    fn a_source_list_survives_the_wire() {
        let msg = WindowMsg::Sources(vec![ScreenSource {
            id: 0x8000_0000,
            kind: SourceKind::Monitor,
            title: "Screen 1".into(),
            width: 1920,
            height: 1080,
            picked_on_host: false,
        }]);
        let framed = crate::Message::module(crate::ModuleId::WINDOW, msg.priority(), &msg).unwrap();
        assert_eq!(
            framed
                .decode_module::<WindowMsg>(crate::ModuleId::WINDOW)
                .unwrap(),
            Some(msg)
        );
    }
}
