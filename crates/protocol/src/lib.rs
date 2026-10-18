//! Cursio wire protocol.
//!
//! Defines the versioned message types exchanged between Cursio peers and the
//! length-prefixed framing used to put them on (and take them off) a byte
//! stream. This crate is transport-agnostic: it knows how to turn a
//! [`Message`] into bytes and back, but not how those bytes travel.

use serde::{Deserialize, Serialize};

pub mod audio;
pub mod calls;
pub mod capability;
pub mod clipboard;
pub mod files;
pub mod frame;
pub mod input;
pub mod messages;
pub mod node;
pub mod notify;
pub mod paths;
pub mod service;
pub mod share;
pub mod update;
pub mod usb;
pub mod window;

pub use capability::{
    negotiate, DeviceKind, ModuleAdvert, ModuleId, ModuleRole, Negotiated, SkipReason,
};

pub use audio::{
    AudioChunk, AudioCodec, AudioFormat, AudioFrameAck, AudioMsg, AudioStreamCloseReason,
    AudioStreamId, AudioStreamMeta, DeviceState, DeviceStateField, RouteEntry, RouteOp, RouteState,
    RoutingState, SampleKind, StreamRole,
};
pub use clipboard::{
    ClipboardData, ClipboardFailReason, ClipboardFailed, ClipboardFormatOffer, ClipboardGeneration,
    ClipboardMsg, ClipboardOffer, ClipboardRequest,
};
// Re-exported through the protocol crate so session code can name the format
// vocabulary without also depending on `clipboard-core` directly.
pub use clipboard_core::ClipboardFormat;
pub use files::{
    Chunk, FileError, FileHandle, FileKind, FileMeta, FileReqId, FilesMsg, OpenMode, Volume,
};
pub use frame::{decode_frame, encode_frame, FrameError, MAX_FRAME_LEN};
pub use input::{
    neighbour_across, Edge, EdgeSet, GesturePhase, InputEvent, MouseButton, Position,
    GESTURE_SCALE_ONE, SCROLL_NOTCH,
};
pub use node::{NetworkKind, NodeStatus, PowerSource};
pub use service::{Fitness, Quality, ServiceId, ServiceMsg, ServiceOffer};
pub use window::{
    Rect, ScreenSource, SourceId, SourceKind, TileCodec, VideoCaps, WindowCloseReason, WindowFrame,
    WindowFrameAck, WindowId, WindowInput, WindowMeta, WindowMsg, SCREEN_STREAM_ID,
};

/// Protocol version of the **session layer**: the greeting, pairing, liveness
/// and framing. Bumped when those change shape.
///
/// Peers exchange this in [`Hello`] and refuse to proceed on mismatch, so an
/// old build never silently misinterprets a newer peer's frames.
///
/// Deliberately *not* bumped for module changes any more. A module's own
/// vocabulary is versioned by [`ModuleAdvert::version`] and negotiated
/// per-module, so an audio change no longer locks out a peer that only wanted
/// clipboard. v12 introduced that split; v13 moved module traffic onto
/// [`Message::Module`], which is what makes it real — a new module now adds no
/// variant here at all. v14 added [`Hello::position`], which every node must
/// read to work out which of its edges faces which peer. v15 added
/// [`Hello::service`] so placement and quality are agreed across the mesh. v17
/// added [`Hello::status`] and [`Message::NodeStatus`], which every node both
/// reports and reads — it is not a module precisely so that it cannot be
/// negotiated away. v18 added [`Message::Reconfigure`], which lets a module
/// toggle or a new position reach a running link instead of restarting the
/// node. v19 opens every connection with an [`update::Preamble`] and added
/// [`Message::Update`]: two builds that disagree here still hand each other
/// new builds over the frozen update lane instead of hanging up. v20 carries
/// [`InputEvent::PointerScroll`] in [`input::SCROLL_NOTCH`]ths of a notch, so a
/// touchpad's fractional scrolling survives the trip. v21 added
/// [`InputEvent::GestureSwipe`] and [`InputEvent::GesturePinch`].
pub const PROTOCOL_VERSION: u16 = 21;

/// A stable identifier for a Cursio node, persisted across restarts.
pub type PeerId = uuid::Uuid;

/// Top-level message enum. Every frame on the wire is one of these.
///
/// The variants are grouped by phase:
/// - discovery/greeting: [`Message::Hello`]
/// - pairing: [`Message::PairRequest`], [`Message::PairResponse`],
///   [`Message::PairConfirm`], [`Message::PairFailed`]
/// - liveness: [`Message::Heartbeat`], [`Message::Bye`]
///
/// `PartialEq` (not `Eq`): [`Message::AudioGain`] carries a floating-point gain,
/// for which exact equality is not meaningful. Nothing keys a `Message` by value
/// (no `HashSet`/`HashMap` use); `PartialEq` is enough for the round-trip tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// First frame each side sends after connecting. Advertises who we are and
    /// which protocol version we speak so the peer can reject a mismatch early.
    Hello(Hello),

    /// Initiator -> responder: begin a password-authenticated pairing.
    /// `spake_msg` is this side's SPAKE2 message (see the transport crate).
    PairRequest { spake_msg: Vec<u8> },

    /// Responder -> initiator: the peer's SPAKE2 message, completing the
    /// key-agreement exchange. Both sides can now derive the shared key.
    PairResponse { spake_msg: Vec<u8> },

    /// Sent after each side has derived the shared key. Carries a MAC/key
    /// confirmation value so both sides prove they derived the *same* key
    /// (i.e. the password matched) before trusting each other.
    PairConfirm { confirmation: Vec<u8> },

    /// Pairing was rejected or failed (wrong password, version mismatch, etc.).
    PairFailed { reason: String },

    /// Keep-alive. Sent periodically once a session is established; the
    /// `nonce` lets the sender match a peer's echo if it wants RTT.
    Heartbeat { nonce: u64 },

    /// Graceful shutdown notice. The sender is closing the session.
    Bye,

    /// The sender's [`NodeStatus`] changed since the greeting.
    ///
    /// Sent on change rather than polled: battery level drifts slowly, but the
    /// switch to discharging — or onto a metered link — is what a peer acts on,
    /// and it happens between any two polls. [`NodeStatus`] itself decides what
    /// counts as a change worth sending, so a level ticking down one point a
    /// minute does not become a frame a minute.
    ///
    /// Not a [`Module`](Message::Module): every node has an answer here, and a
    /// peer sharing only files is exactly the one whose battery is worth
    /// knowing. Making it a module would mean it could be negotiated away.
    NodeStatus(node::NodeStatus),

    /// An opaque byte payload the protocol layer doesn't interpret.
    ///
    /// Used by the encrypted transport to carry Noise handshake frames and
    /// ciphertext over the inner (plaintext-framed) transport: the bytes are
    /// meaningful only to the crypto layer, so the protocol just ferries them.
    Opaque(Vec<u8>),

    /// One module's message, addressed by [`ModuleId`] and carried as bytes the
    /// session never interprets.
    ///
    /// This is what stops `Message` growing a variant per module. Each module
    /// owns its own enum, serializes it into `payload`, and the session routes
    /// the frame by `module` — so adding notifications, file transfer or a phone
    /// touchpad touches no shared type and needs no
    /// [`PROTOCOL_VERSION`] bump. A module's own vocabulary is versioned by
    /// [`ModuleAdvert::version`] and negotiated per-link.
    ///
    /// `priority` is carried explicitly because the scheduler must rank a frame
    /// without decoding it — the payload is opaque here by design, so
    /// [`Message::priority`] has nothing else to go on. A module that declares
    /// the wrong class breaks the one guarantee the shared channel rests on
    /// (see `docs/platform-design.md`, rule 2), so it is stated at the send site
    /// rather than inferred.
    ///
    /// **Not** used by the audio and window hot paths: their per-chunk traffic
    /// keeps first-class variants below, to avoid an extra serialize and
    /// `Vec<u8>` copy per frame. That tiering is deliberate — see
    /// `docs/platform-design.md`.
    Module {
        module: ModuleId,
        priority: Priority,
        payload: Vec<u8>,
    },

    /// A captured input event to replay on the peer (mouse/keyboard).
    ///
    /// Streamed once a session is up. Carries platform-neutral relative motion
    /// and key/button state (see [`input::InputEvent`]); the receiver injects
    /// it via a virtual device.
    Input(InputEvent),

    /// Ownership handoff for the shared cursor — sets the **receiver's** active
    /// state.
    ///
    /// `active = true` means "you are now the active machine, hold the cursor":
    /// the receiver should start injecting inbound [`Input`](Message::Input) and
    /// stop forwarding its own. `active = false` means "you are dormant now":
    /// grab and stream local input, stop injecting. The sender flips itself to
    /// the opposite state at the same instant, so exactly one machine is active
    /// at all times. See the transition design in `docs/input-design.md`.
    Grab { active: bool },

    /// Edge-crossing handoff: the sender's shared cursor left the sender across
    /// `edge` (one of *its* edges), so the receiver should become **active** and
    /// the cursor conceptually enters on the receiver's [`opposite`] edge at
    /// `pos_norm` along it.
    ///
    /// Like [`Grab { active: true }`](Message::Grab) — the sender flips itself to
    /// dormant at the same instant, preserving the "exactly one active" invariant
    /// — but carries entry geometry the plain `Grab` doesn't: which edge and
    /// where along it (`0.0`..=`1.0`; y-fraction for a vertical edge, x-fraction
    /// for a horizontal one). The receiver uses it to hide/show its cursor at the
    /// right spot, and later for an absolute warp-on-enter if we add one. Motion
    /// itself stays relative (see [`InputEvent`]).
    ///
    /// [`opposite`]: input::Edge::opposite
    CrossEdge { edge: input::Edge, pos_norm: f32 },

    /// Host -> viewer: one pixel update (dirty region) for a transferred window.
    /// Streamed continuously while the window is live on the viewer.
    ///
    /// A **hot path**: kept first-class rather than enveloped, so a frame costs
    /// no extra serialize or buffer copy. The module's low-rate control traffic
    /// (offer/ack/close) lives in [`window::WindowMsg`] instead.
    WindowFrame(window::WindowFrame),

    /// Viewer -> host: input from the surrogate window, in that window's own
    /// coordinate space. The host maps it onto its real window and replays it.
    /// Distinct from [`Input`](Message::Input) (shared-cursor relative motion).
    ///
    /// A hot path, for the same reason as [`WindowFrame`](Message::WindowFrame):
    /// this carries every pointer move over a transferred window.
    WindowInput(window::WindowInput),

    /// Owner -> peer: one small frame of samples for an advertised stream.
    /// Streamed continuously while the stream is live.
    ///
    /// The module's **hot path**, and the reason it keeps a first-class variant:
    /// chunks arrive every ~10 ms per stream, per app, per machine. Everything
    /// else the audio module says — advertisements, gain, device and route state
    /// — is enveloped as [`audio::AudioMsg`].
    AudioChunk(audio::AudioChunk),
    // Clipboard traffic is no longer here: it moved to `Message::Module` with
    // `ModuleId::CLIPBOARD`, carrying `clipboard::ClipboardMsg`. It was migrated
    // first because it is pure bulk — no per-frame hot path to pay an extra
    // serialize on. See `docs/platform-design.md`.
    /// The sender's configuration changed and the link stays up: it now
    /// offers `modules` and stands at `position`.
    ///
    /// Everything [`Hello`] settled that a user can change from the app, sent
    /// again instead of tearing the link down to greet anew. Both sides
    /// renegotiate from the latest adverts each has of the other, start what
    /// became agreed, stop what stopped being agreed, and leave the shared
    /// cursor and every other module exactly as they were. See
    /// `docs/mesh-design.md`, "Changing settings on a running node".
    ///
    /// Appended rather than placed beside `Hello`: variants are encoded by
    /// index, and every one after an insertion would change its number.
    Reconfigure(Reconfigure),

    /// One [`update::UpdateMsg`], in the lane's own frozen encoding.
    ///
    /// **Index 17, forever.** A build that can no longer hold a session with
    /// its peer still has to read this, so its place in the enum — and the
    /// plain byte vector it carries — must mean the same thing to every
    /// version. Variants are only ever appended after it. See [`update`].
    Update(Vec<u8>),
}

/// Payload of [`Message::Hello`].
///
/// Carries not just *who* the sender is but *what it can do*: the module set is
/// negotiated here, before any module starts, because a module must never run
/// one-sidedly. See [`capability`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Protocol version the sender speaks. Compared against [`PROTOCOL_VERSION`].
    ///
    /// This covers the **session layer only** — the greeting, pairing, liveness
    /// and framing. Module vocabularies version themselves via
    /// [`ModuleAdvert::version`], so a module changing shape no longer forces
    /// every peer to upgrade in lockstep.
    pub protocol_version: u16,
    /// Stable identity of the sending node.
    pub peer_id: PeerId,
    /// Human-friendly name (usually the hostname) for UI/logs.
    pub display_name: String,
    /// What sort of machine this is. Decides, among other things, whether the
    /// shared cursor can travel here at all — see [`DeviceKind::takes_cursor`].
    pub device_kind: DeviceKind,
    /// Subsystems this node has available on this link. A module runs only if
    /// both sides advertise it compatibly; see [`capability::negotiate`].
    pub modules: Vec<ModuleAdvert>,
    /// Where this machine stands on the layout grid, so each side can work out
    /// which of its own edges faces the other. See [`Position`].
    pub position: Position,
    /// What this node can carry for the mesh, and what it wants from it.
    ///
    /// In the greeting rather than asked for, because placing a duty has to be
    /// settled the moment two machines meet: a round trip later is a window in
    /// which both of them, or neither, are running it. See [`ServiceOffer`].
    pub service: ServiceOffer,
    /// What this node is *doing* — battery, power source, network — as opposed
    /// to what it can do.
    ///
    /// In the greeting so a peer knows the moment they meet, and re-sent as
    /// [`Message::NodeStatus`] whenever it changes. Every field inside is
    /// optional; an older peer's greeting decodes to
    /// [`NodeStatus::unknown`], which reads as "no information" rather than as
    /// a flat battery. See [`node`].
    pub status: NodeStatus,
}

/// Payload of [`Message::Reconfigure`]: the parts of [`Hello`] a running node
/// can change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconfigure {
    /// Everything the sender now offers, in full — not a delta, so a message
    /// that crossed another in flight still leaves both sides agreeing.
    pub modules: Vec<ModuleAdvert>,
    pub position: Position,
    /// Modules to stop and start again on both sides even though they stay
    /// agreed, because a setting they read when built changed. Restarting only
    /// one side would leave the other mid-conversation with a module that
    /// forgot it.
    pub restart: Vec<ModuleId>,
}

impl Hello {
    /// A greeting advertising no modules — the identity half only.
    ///
    /// Real nodes fill `modules` from their factory registry; this is for the
    /// cases that only need a well-formed `Hello`: transport tests using it as a
    /// convenient payload, and a peer whose module set is genuinely empty.
    pub fn bare(peer_id: PeerId, display_name: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            peer_id,
            display_name: display_name.into(),
            device_kind: DeviceKind::Desktop,
            modules: Vec::new(),
            position: Position::default(),
            service: ServiceOffer::none(),
            status: NodeStatus::unknown(),
        }
    }
}

impl Message {
    /// Serialize this message into raw bytes (no length prefix).
    ///
    /// Use [`frame::encode_frame`] when writing to a stream; this is the inner
    /// payload it wraps.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        bincode::serialize(self).map_err(|e| ProtocolError::Encode(e.to_string()))
    }

    /// Deserialize a message from raw bytes (payload without length prefix).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProtocolError> {
        bincode::deserialize(bytes).map_err(|e| ProtocolError::Decode(e.to_string()))
    }

    /// Scheduling priority for this message on the shared channel.
    ///
    /// Everything rides one encrypted link, so a bulk transfer (a clipboard
    /// image, a file) must never delay the mouse. The sender queues by this
    /// priority and drains highest-first, interleaving so latency-critical
    /// traffic jumps ahead of a large payload already in flight. The ordering,
    /// agreed with the user, is **input > audio > video > files**, with session
    /// control above all of it. See [`Priority`].
    pub fn priority(&self) -> Priority {
        match self {
            // Session integrity: tiny, and losing/delaying these desyncs or
            // stalls the whole link. Above even input.
            Message::Hello(_)
            | Message::PairRequest { .. }
            | Message::PairResponse { .. }
            | Message::PairConfirm { .. }
            | Message::PairFailed { .. }
            | Message::Heartbeat { .. }
            | Message::Bye
            | Message::Grab { .. }
            | Message::CrossEdge { .. }
            // Rare and a few bytes, but the reason it exists is to arrive
            // before the peer acts on stale state — a node announcing it just
            // went critical while queued behind a file transfer has said
            // nothing useful.
            | Message::NodeStatus(_)
            // Must reach the peer before the first message of a module it
            // starts, or that message arrives for a module not yet running
            // there and is dropped.
            | Message::Reconfigure(_) => Priority::Control,

            // The pointer/keyboard. The one thing that must never feel laggy.
            Message::Input(_) | Message::WindowInput(_) => Priority::Input,

            // Sound: low-latency and steady, but yields to input. (The module's
            // control traffic arrives as `Module` envelopes declaring the same
            // class, so an advertisement can't overtake the chunks it
            // introduces.)
            Message::AudioChunk(_) => Priority::Audio,

            // Streamed window pixels: heavier, and freshness-oriented, so below
            // audio. (The module's offer/ack/close arrive as `Module` envelopes
            // declaring the same class, so control stays ordered with frames.)
            Message::WindowFrame(_) => Priority::Video,

            // Bulk, loss-intolerant, latency-tolerant: raw opaque ferrying.
            // Drains last so it fills spare capacity without ever blocking the
            // classes above. (Clipboard payloads are here too, but arrive as
            // `Module` envelopes declaring `Priority::Bulk`.)
            Message::Opaque(_) => Priority::Bulk,

            // A 30 MB executable in 256 KiB pieces: it queues behind the
            // cursor like any file chunk, and nobody is waiting on any one
            // piece of it.
            Message::Update(_) => Priority::Bulk,

            // Enveloped module traffic ranks itself: the payload is opaque here,
            // so the sending module is the only thing that knows what the frame
            // is. Declared at the send site (see `Message::Module`).
            Message::Module { priority, .. } => *priority,
        }
    }

    /// Which module this message belongs to, or `None` for the session's own
    /// vocabulary (greeting, pairing, liveness, input and ownership).
    ///
    /// The receiver routes by this before anything decodes the payload, so a
    /// frame reaches exactly one module instead of being offered to each in
    /// turn. The first-class hot paths are named here too: a window frame is
    /// the window module's however it travels, and forgetting one would hand
    /// it to the session loop that the shared cursor rides.
    pub fn module_id(&self) -> Option<ModuleId> {
        match self {
            Message::Module { module, .. } => Some(*module),
            Message::AudioChunk(_) => Some(ModuleId::AUDIO),
            Message::WindowFrame(_) | Message::WindowInput(_) => Some(ModuleId::WINDOW),
            Message::Update(_) => Some(ModuleId::UPDATE),
            Message::Hello(_)
            | Message::PairRequest { .. }
            | Message::PairResponse { .. }
            | Message::PairConfirm { .. }
            | Message::PairFailed { .. }
            | Message::Heartbeat { .. }
            | Message::Bye
            | Message::NodeStatus(_)
            | Message::Reconfigure(_)
            | Message::Opaque(_)
            | Message::Input(_)
            | Message::Grab { .. }
            | Message::CrossEdge { .. } => None,
        }
    }

    /// Envelope a module's own message for the shared channel.
    ///
    /// The module's enum is serialized here and routed by `module` on the far
    /// side, so neither the session nor any other module needs to know its
    /// shape. See [`Message::Module`] for why `priority` is explicit.
    pub fn module<T: Serialize>(
        module: ModuleId,
        priority: Priority,
        payload: &T,
    ) -> Result<Self, ProtocolError> {
        Ok(Message::Module {
            module,
            priority,
            payload: bincode::serialize(payload)
                .map_err(|e| ProtocolError::Encode(e.to_string()))?,
        })
    }

    /// Decode an enveloped payload as this module's own message type.
    ///
    /// Returns `Ok(None)` when the frame belongs to a *different* module, so a
    /// [`SessionModule`] can claim-or-decline in one call. A decode failure for
    /// a frame that *is* ours is a real error: it means the peer's vocabulary
    /// differs despite negotiation agreeing on a version.
    ///
    /// [`SessionModule`]: https://docs.rs/cursio-core
    pub fn decode_module<T: for<'de> Deserialize<'de>>(
        &self,
        module: ModuleId,
    ) -> Result<Option<T>, ProtocolError> {
        match self {
            Message::Module {
                module: id,
                payload,
                ..
            } if *id == module => {
                let decoded = bincode::deserialize(payload)
                    .map_err(|e| ProtocolError::Decode(e.to_string()))?;
                Ok(Some(decoded))
            }
            _ => Ok(None),
        }
    }
}

/// Scheduling classes for the shared channel, highest first.
///
/// The sender keeps one queue per class and always drains a higher class before
/// a lower one, so latency-critical traffic (input) is never stuck behind a
/// large, latency-tolerant payload (a file). See [`Message::priority`].
/// Serialized because [`Message::Module`] carries it on the wire: an enveloped
/// payload is opaque to the session, so the sender's declared class is the only
/// thing the receiving scheduler can rank it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    /// Bulk, latency-tolerant: clipboard/file payloads. Drained last.
    Bulk,
    /// Small requests something is blocked on: a `stat`, a directory listing,
    /// an open.
    ///
    /// Above [`Priority::Bulk`] because the two are not the same kind of
    /// traffic even when they belong to the same module. A file chunk is
    /// 256 KiB and nothing waits on any particular one; a `stat` is tens of
    /// bytes and a process is asleep in a filesystem call until it is answered.
    /// Sharing one class meant that question waited out every chunk already
    /// queued — an `ls` during a copy stalled for as long as the queue was
    /// deep, which is what a mount reports to its caller as a hung drive.
    ///
    /// Below [`Priority::Video`] because it has no frame rate to keep, and the
    /// volume is self-limiting: these messages are small, and each answers a
    /// caller that cannot ask again until it has its answer.
    Metadata,
    /// Streamed window pixels.
    Video,
    /// Audio streams.
    Audio,
    /// Mouse/keyboard — must never lag.
    Input,
    /// Session control (handshake, heartbeat, grab, edge crossing). Above all.
    Control,
}

/// Errors from (de)serializing a [`Message`].
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("failed to encode message: {0}")]
    Encode(String),
    #[error("failed to decode message: {0}")]
    Decode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two variants every version must be able to tell apart without
    /// decoding anything else: an old peer's greeting, and the update lane.
    #[test]
    fn hello_and_update_keep_their_frozen_variant_indices() {
        let update = Message::Update(vec![0xab]).to_bytes().unwrap();
        assert_eq!(update, [17, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0xab]);
        let hello = Message::Hello(Hello::bare(PeerId::nil(), "h"))
            .to_bytes()
            .unwrap();
        assert_eq!(hello[..4], [0, 0, 0, 0]);
        assert_eq!(u16::from_le_bytes([hello[4], hello[5]]), PROTOCOL_VERSION);
    }

    #[test]
    fn hello_roundtrips_through_bytes() {
        let msg = Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            peer_id: PeerId::nil(),
            display_name: "test-host".to_string(),
            device_kind: DeviceKind::Desktop,
            modules: vec![
                ModuleAdvert::symmetric(ModuleId::AUDIO, 1),
                ModuleAdvert::new(ModuleId::INPUT, 1, ModuleRole::Source),
            ],
            position: Position::new(1, 0),
            service: ServiceOffer {
                fitness: Fitness::Full,
                wanted: Some(Quality::new(2560, 1440, 30)),
                show_window: true,
            },
            status: NodeStatus {
                battery_percent: Some(67),
                power: Some(node::PowerSource::Battery),
                network: Some(NetworkKind::Wifi),
                signal_dbm: Some(-48),
            },
        });
        let bytes = msg.to_bytes().unwrap();
        let back = Message::from_bytes(&bytes).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn heartbeat_roundtrips() {
        let msg = Message::Heartbeat { nonce: 42 };
        let bytes = msg.to_bytes().unwrap();
        assert_eq!(Message::from_bytes(&bytes).unwrap(), msg);
    }

    /// Module traffic names its module whether it is enveloped or rides a
    /// first-class hot path, and the session's own vocabulary names none.
    #[test]
    fn every_module_frame_names_its_module_and_session_frames_name_none() {
        let files = Message::Module {
            module: ModuleId::FILES,
            priority: Priority::Bulk,
            payload: Vec::new(),
        };
        assert_eq!(files.module_id(), Some(ModuleId::FILES));

        let chunk = Message::AudioChunk(audio::AudioChunk {
            id: 1,
            seq: 0,
            timestamp: 0,
            data: Vec::new(),
        });
        assert_eq!(chunk.module_id(), Some(ModuleId::AUDIO));

        let frame = Message::WindowFrame(window::WindowFrame {
            id: 1,
            seq: 0,
            dirty: window::Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            codec: window::TileCodec::RawBgra8888,
            data: Vec::new(),
        });
        assert_eq!(frame.module_id(), Some(ModuleId::WINDOW));

        for session in [
            Message::Heartbeat { nonce: 1 },
            Message::Bye,
            Message::Input(InputEvent::PointerMotion { dx: 1, dy: 0 }),
            Message::Grab { active: true },
        ] {
            assert_eq!(session.module_id(), None, "{session:?} is the session's");
        }
    }

    /// A reconfiguration is the session's, travels as control, and survives
    /// the wire whole — a dropped `restart` would leave a module half-rebuilt.
    #[test]
    fn a_reconfiguration_roundtrips_as_session_control() {
        let msg = Message::Reconfigure(Reconfigure {
            modules: vec![ModuleAdvert::symmetric(ModuleId::FILES, 4)],
            position: Position::new(-1, 0),
            restart: vec![ModuleId::WINDOW],
        });
        assert_eq!(msg.module_id(), None);
        assert_eq!(msg.priority(), Priority::Control);
        assert_eq!(Message::from_bytes(&msg.to_bytes().unwrap()).unwrap(), msg);
    }

    /// A stand-in for a module's own enum — the point being that `Message` has
    /// never heard of it.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum FakeModuleMsg {
        Ping(u32),
        Note(String),
    }

    #[test]
    fn an_enveloped_module_message_roundtrips() {
        let payload = FakeModuleMsg::Note("hello".into());
        let msg = Message::module(ModuleId(4242), Priority::Bulk, &payload).unwrap();

        let back = Message::from_bytes(&msg.to_bytes().unwrap()).unwrap();

        assert_eq!(
            back.decode_module::<FakeModuleMsg>(ModuleId(4242)).unwrap(),
            Some(payload)
        );
    }

    /// The routing rule: a module must never be handed another module's frame.
    /// `Ok(None)` (not an error) is what lets a module claim-or-decline.
    #[test]
    fn a_module_does_not_decode_another_modules_frame() {
        let msg =
            Message::module(ModuleId::AUDIO, Priority::Audio, &FakeModuleMsg::Ping(1)).unwrap();

        let other = msg
            .decode_module::<FakeModuleMsg>(ModuleId::CLIPBOARD)
            .unwrap();

        assert_eq!(
            other, None,
            "a frame addressed to audio was decoded by clipboard"
        );
    }

    /// The scheduler ranks an envelope without decoding it, so the declared
    /// class must survive the round trip — otherwise a bulk file transfer could
    /// be drained ahead of the mouse.
    #[test]
    fn an_envelope_keeps_its_declared_priority() {
        let msg = Message::module(ModuleId(7), Priority::Bulk, &FakeModuleMsg::Ping(9)).unwrap();
        assert_eq!(msg.priority(), Priority::Bulk);

        let back = Message::from_bytes(&msg.to_bytes().unwrap()).unwrap();
        assert_eq!(back.priority(), Priority::Bulk);
    }
}
