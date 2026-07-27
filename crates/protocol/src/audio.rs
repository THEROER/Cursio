//! Audio-sharing wire types.
//!
//! Cursio shares audio between machines as a **networked, per-application audio
//! bus** — the transport that replaces a zoo of AudioRelay + Bluetooth hops,
//! *not* a mixer (VoiceMeeter/Carla stay). Each machine presents the peer's
//! audio as extra nodes in its local graph: the peer's speaker output arrives
//! as a source you can pull from, and things you play into Cursio's virtual sink
//! stream out to the peer — the Mouse-Without-Borders model, but for sound.
//!
//! The defining requirement is **per-application** granularity: each app on a
//! peer (Spotify, Discord, a game…) appears on the other machine as its *own*
//! named stream, not merged into one blob. That is what lets VoiceMeeter/Carla
//! and a volume knob control each app independently across machines — something
//! the system mixer can't do once audio has left it as a single merged stream.
//!
//! These types are the neutral vocabulary for that exchange, mirroring
//! [`input`](crate::input) and [`window`](crate::window): OS-agnostic, with the
//! platform (PipeWire on Linux, WASAPI on Windows) mapping to/from them behind
//! traits in the `cursio-audio` crate. See `docs/audio-design.md`.

use serde::{Deserialize, Serialize};

/// Identifies one logical audio stream within a session.
///
/// One per stream — a whole-device stream or a single app's node. Allocated by
/// the side that *owns* the stream (the source) and echoed on every chunk/ack
/// for it, so a session can carry many concurrent streams (one per app, both
/// directions) without ambiguity. Session-scoped and opaque; not an OS handle.
pub type AudioStreamId = u32;

/// Sample layout of an audio stream.
///
/// Kept minimal and explicit so both sides agree on how to interpret
/// [`AudioChunk`] payloads without negotiation. 48 kHz stereo `S16` is the
/// sensible default; other rates/kinds are carried, not assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    /// Samples per second per channel (e.g. 48000).
    pub sample_rate: u32,
    /// Channel count (1 = mono, 2 = stereo).
    pub channels: u8,
    /// How each sample is encoded within a (decoded) frame.
    pub sample_kind: SampleKind,
}

/// Per-sample numeric format of decoded audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SampleKind {
    /// Signed 16-bit, little-endian, interleaved by channel.
    S16Le,
    /// 32-bit float, native endian, interleaved by channel.
    F32,
}

/// How an [`AudioChunk`]'s payload is encoded on the wire.
///
/// Raw PCM is fine on a LAN for bring-up but wasteful (48 kHz stereo s16 ≈
/// 1.5 Mbit/s *per stream*, multiplied per app); a steady multi-app stream uses
/// **Opus** in low-latency mode. Tagging the codec per stream means PCM↔Opus
/// needs no protocol change — the same discipline as
/// [`TileCodec`](crate::TileCodec) for windows. Only these two for now; more can
/// be added without touching callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    /// Uncompressed PCM in the stream's [`AudioFormat`]. LAN / bring-up only.
    RawPcm,
    /// Opus-encoded frames (low-latency). The steady-state codec.
    Opus,
}

/// The direction/role of a shared audio stream, from the **advertiser's** view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamRole {
    /// Output the advertiser is producing (something is *playing*), for the peer
    /// to render — e.g. an app's sound or a device's mixed output.
    Playback,
    /// Input the advertiser is offering as a capture source — e.g. a microphone
    /// the peer can pull from.
    Capture,
}

/// Announces a stream the peer can consume.
///
/// Sent (in [`Message::AudioStreamAdvertise`](crate::Message::AudioStreamAdvertise))
/// when an app starts playing or a device/mic becomes shareable, before any
/// [`AudioChunk`] for its `id`. `app_name` is the crux of per-app sharing:
/// - `Some(name)` ⇒ this stream is **one application's** audio; the peer should
///   create a distinct named node for it ("Cursio: Spotify (PC)") so it can be
///   routed and volume-controlled on its own.
/// - `None` ⇒ a whole-device stream (a device's mixed output or a raw mic).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStreamMeta {
    /// Owner-allocated id for this stream; echoed on every related message.
    pub id: AudioStreamId,
    /// Whether this is output to render or a capture source to pull from.
    pub role: StreamRole,
    /// The application this stream belongs to, if it is a per-app node; `None`
    /// for a whole-device stream.
    pub app_name: Option<String>,
    /// The **specific output device** this stream represents, when the owner is
    /// mirroring its devices one-by-one rather than offering a single global
    /// stream.
    ///
    /// `Some((id, description))` ⇒ the peer should create a distinct local
    /// device named after `description` ("Cursio: Realtek …"), and audio routed
    /// into it comes back tagged with this stream's `id` so the owner can render
    /// it into *that exact* device (`id` is the owner's platform handle for it:
    /// a PipeWire node name, or a Windows endpoint friendly name).
    /// `None` ⇒ the global "everything" stream.
    pub device: Option<(String, String)>,
    /// Sample layout of the (decoded) audio.
    pub format: AudioFormat,
    /// How [`AudioChunk`] payloads for this stream are encoded.
    pub codec: AudioCodec,
}

/// One small frame of audio for a stream.
///
/// Chunks are short and fixed (e.g. ~10 ms) so the receiver can run a **jitter
/// buffer** — order by `seq`, detect loss, and trade a few ms of latency for
/// gap-free playback (an underrun is instantly audible, unlike a dropped video
/// frame). `timestamp` is the stream's own sample clock at the first sample, so
/// the receiver can resample to its device clock and absorb drift. `data` is the
/// samples encoded per the stream's [`AudioCodec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioChunk {
    /// Which stream this chunk belongs to.
    pub id: AudioStreamId,
    /// Monotonic per-stream frame counter, for ordering/loss-detection and acks.
    pub seq: u32,
    /// Sample-clock position of this chunk's first sample, for drift correction.
    pub timestamp: u64,
    /// The audio samples, encoded per the stream's codec.
    pub data: Vec<u8>,
}

/// Receiver -> sender feedback for adaptive rate control.
///
/// Like the window path, the sender must not out-run the link. The receiver
/// periodically reports the newest chunk it has played; the sender uses the
/// sent-vs-acked gap plus RTT to adjust Opus bitrate. The rule mirrors video —
/// **degrade rather than buffer unboundedly** — but audio deliberately keeps a
/// *small* jitter buffer, so this governs bitrate, not whether to buffer at all.
/// Sent sparsely (a few per second), not per chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFrameAck {
    /// Which stream this acks.
    pub id: AudioStreamId,
    /// The highest [`AudioChunk::seq`] the receiver has played out.
    pub acked_seq: u32,
}

/// Why an audio stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioStreamCloseReason {
    /// The source stopped (app quit / stopped playing, or device removed).
    SourceGone,
    /// The consumer no longer wants the stream.
    ConsumerClosed,
}

/// The replicated state of one shared audio **device** (see docs/audio-design.md,
/// "Device state is REPLICATED with a per-device owner").
///
/// In the "one system" model a device is a *distributed object*: its real face
/// lives on its **owner** (the machine the physical device is on), and every
/// other machine shows an Intent Sink that mirrors this exact state. There is
/// **exactly one volume** per device, replicated identically everywhere — any
/// "make the remote quieter" gain belongs to a Route/Stream, never here.
///
/// This is a **whole object**: it is replicated in full and applied
/// idempotently, so a node can resend the latest on reconnect without relying on
/// deltas (invariant #10). Extend it (channel volumes, balance, default) by
/// adding fields — receivers apply whatever they understand.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeviceState {
    /// Master volume, linear, `0.0`..=`1.0` (`1.0` = unity/full). The device's
    /// single replicated volume — not a gain.
    pub volume: f32,
    /// Whether the device is muted.
    pub mute: bool,
}

/// A single mutable field of a [`DeviceState`], for an owner-routed change request.
///
/// A non-owner never writes device state directly (invariant #3); it sends the
/// owner one of these as a *request*, and reflects the [`DeviceState`] the owner
/// broadcasts back. One field per message keeps "set volume" and "toggle mute"
/// independent so a race on one never clobbers the other.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum DeviceStateField {
    /// Request the master volume be set to this linear value (`0.0`..=`1.0`).
    Volume(f32),
    /// Request the mute flag be set to this value.
    Mute(bool),
}

/// Where one Remote Stream is targeted: the device its audio should render into
/// on the receiving machine (see docs/audio-design.md, "Route → Target device").
///
/// Part of the replicated **RoutingState**. `device` is `Some((handle,
/// description))` for a specific target device on the receiver, or `None` for the
/// receiver's default output. `app_name` is carried for logs/UI so a route is
/// legible ("Spotify → VAIO3") without cross-referencing the advertise.
///
/// Only the **stream's host** machine may set this (invariant #4): the receiver
/// reflects it, never rewrites it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteEntry {
    /// The Remote Stream this route is for.
    pub id: AudioStreamId,
    /// The application whose audio this stream carries, for legibility.
    pub app_name: Option<String>,
    /// Target device on the receiver; `None` = the receiver's default output.
    pub device: Option<(String, String)>,
}

/// A whole-object snapshot of every active Remote Stream's target device.
///
/// Replicated by the **host** of the streams (invariant #4) so routing survives a
/// reconnect **without relying on PipeWire re-emitting link events** — the design
/// requirement for this phase. Applied idempotently: the receiver overwrites its
/// per-stream targets with exactly this set, so a resent snapshot is always safe
/// (invariant #10). Sent on (re)connect and whenever membership changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RoutingState {
    /// The current routes, one per active Remote Stream.
    pub routes: Vec<RouteEntry>,
}

/// A group operation on a whole **Route** — all the streams currently funnelled
/// into one target device (see docs/audio-design.md, "Route / Routing Group").
///
/// A Route ties many per-app Remote Streams to one destination; an op applies to
/// the group, and the host fans it out to each member. Group **gain** and
/// **mute** are properties of the Route, kept cleanly separate from the device's
/// single replicated volume — "make only the remote audio quieter" lives here,
/// never on the device (invariant #2). Latency/codec policy will extend this.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum RouteOp {
    /// Set the Route's group gain (linear, `1.0` = unity). Fanned out to each
    /// member as its effective per-stream gain, above the OS mixer.
    SetGain(f32),
    /// Mute/unmute the whole Route (independent of per-member gain).
    SetMute(bool),
}

/// The replicated state of one **Route** (a target device and its members).
///
/// Held and replicated by the **host** of the member streams (invariant #4), for
/// group operations and a future UI. `target_device` is the destination the
/// members render into on the receiver; `members` are the Remote Stream ids in
/// the group. `gain`/`mute` are the group's, fanned out to members as their
/// effective per-stream gain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteState {
    /// The destination device this Route funnels into `(handle, description)`.
    pub target_device: (String, String),
    /// The Remote Stream ids currently in this Route.
    pub members: Vec<AudioStreamId>,
    /// Group gain (linear, `1.0` = unity).
    pub gain: f32,
    /// Whether the whole Route is muted.
    pub mute: bool,
}

/// The audio module's **control** vocabulary, carried inside
/// [`Message::Module`](crate::Message::Module).
///
/// Everything except the samples themselves. [`AudioChunk`] stays a first-class
/// [`Message::AudioChunk`](crate::Message::AudioChunk) variant: chunks arrive
/// every ~10 ms per stream, multiplied by every app on every machine, so paying
/// an extra serialize and `Vec<u8>` copy per chunk is the one place where the
/// envelope's cost is not acceptable.
///
/// The control traffic here is low-rate by comparison (advertisements, gain
/// changes, device/route state) and is exactly the part that has grown most —
/// phases 4 through 7 all added to it. Enveloping it means the next such change
/// versions the audio module alone rather than locking out every peer. See
/// `docs/platform-design.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AudioMsg {
    /// Owner -> peer: announce a shareable stream (a device or one app's node).
    /// Sent before any chunk for its id.
    StreamAdvertise(AudioStreamMeta),
    /// Consumer -> owner: newest chunk played out, for adaptive bitrate control.
    FrameAck(AudioFrameAck),
    /// Either side: logical volume for a stream's node, applied above the OS
    /// mixer. `gain` is linear (1.0 = unity, 0.0 = mute).
    Gain { id: AudioStreamId, gain: f32 },
    /// Either side: end a stream (app stopped, device gone, consumer dropped it).
    StreamClose { id: AudioStreamId, reason: AudioStreamCloseReason },
    /// Non-owner -> owner: *request* a change to a device's replicated state.
    /// The owner decides, applies, and broadcasts — invariant #3.
    DeviceStateSet { device: String, field: DeviceStateField },
    /// Owner -> everyone: a device's current state, whole object and idempotent.
    DeviceState { device: String, state: DeviceState },
    /// Stream host -> receiver: (re)target one Remote Stream to a device.
    StreamRoute(RouteEntry),
    /// Stream host -> receiver: the whole routing map, idempotent, replicated on
    /// reconnect so routing survives without waiting for link events.
    RoutingState(RoutingState),
    /// Receiver -> stream host: a group operation on a whole Route.
    RouteOp { target_device: (String, String), op: RouteOp },
    /// Stream host -> receiver: the current state of one Route, idempotent.
    RouteState(RouteState),
    /// Consumer -> owner: whether anything here is actually listening to a
    /// stream. Idempotent — the current value, not a delta.
    ///
    /// A device is advertised so it appears in the consumer's device list, which
    /// is not the same as anyone having opened it. Without this the owner
    /// streamed every advertised endpoint from the moment it connected: a peer
    /// mirroring 13 microphones sent 13 live streams into nodes with no
    /// consumer, which cost bandwidth, forced the sender's backlog trim to
    /// discard chunks (heard as audio dropping in and out for the first seconds
    /// of a session), and piled up in the receiver's undrained queues.
    ///
    /// The stream stays open and the node stays published either way; this only
    /// says whether samples are worth sending right now.
    StreamDemand { id: AudioStreamId, listening: bool },
}

impl AudioMsg {
    /// Scheduling class for audio control traffic.
    ///
    /// `Audio`, matching the chunks it governs: an advertisement must not arrive
    /// after the chunks it introduces, and a close must not overtake the samples
    /// it ends. Same reasoning as the window module's control tier.
    pub const PRIORITY: crate::Priority = crate::Priority::Audio;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_app_stream_is_distinguishable_from_device() {
        let app = AudioStreamMeta {
            id: 1,
            role: StreamRole::Playback,
            app_name: Some("Spotify".into()),
            device: None,
            format: AudioFormat {
                sample_rate: 48000,
                channels: 2,
                sample_kind: SampleKind::S16Le,
            },
            codec: AudioCodec::Opus,
        };
        assert!(app.app_name.is_some(), "per-app node carries app_name");
    }
}
