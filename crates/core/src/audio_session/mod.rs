//! Audio sharing as a [`SessionModule`], not a session of its own.
//!
//! Audio used to own its whole loop: it split the transport, ran its own
//! reader task and its own `select!`. That made a session either *input* or
//! *audio* and never both, since only one owner can hold the single Noise
//! channel.
//!
//! [`AudioEngine`] holds only the audio *state* (captures, mirrors, playbacks)
//! and implements [`SessionModule`], so the session keeps the transport and
//! lends it a `tx`. Input and audio now share one encrypted channel,
//! multiplexed by message type.

use std::collections::{HashMap, VecDeque};

use cursio_protocol::{ModuleId, PeerId};

use crate::audio_gate::{Gate, MicGate};
use crate::audio_session::ids::id_base_for;
use crate::audio_session::intent_sink::IntentSinkOpener;
use crate::audio_session::intent_sink::OpenedIntentSink;
use crate::audio_session::intent_sink::OpenedTap;
use crate::audio_session::intent_sink::PendingTap;
use crate::audio_session::receive::InboundCalls;
use crate::audio_session::receive::OpenedPlayback;
use crate::audio_session::routing::RouteGroup;
pub use crate::mixer::LocalMixer;
use crate::module::SessionModule;

mod devices;
mod health;
mod ids;
mod intent_sink;
mod module;
mod receive;
mod relay;
mod routing;
mod send;
#[cfg(test)]
mod test_support;

/// Live audio state for one session: what we capture and stream out, and what
/// we play from the peer.
pub struct AudioEngine {
    /// Base for stream ids allocated by this side (per-role, so the two
    /// directions never collide).
    id_base: u32,
    /// Stream ids we advertised, mapped to the local device to render into when
    /// the peer sends audio back on that id.
    inbound_targets: HashMap<u32, String>,
    /// Recordings of this machine's microphones, keyed by stream id, so the peer
    /// can use them as its own inputs. Open only while the peer may be
    /// listening; see `mic_devices`.
    mic_captures: HashMap<u32, cursio_audio::Capture>,
    /// Every microphone advertised to the peer, open or not, by stream id, so
    /// one the peer said nothing records from can be closed and reopened when
    /// something does. Unlike a program stream, a microphone kept running for
    /// an idle peer is not free: on a phone each link held its own recording
    /// open for the whole session — four links, four `AudioRecord`s and the
    /// CPU awake to gate their silence — and that was a large part of what the
    /// app cost in battery with nothing being said.
    mic_devices: HashMap<u32, String>,
    /// Intent Sinks we created for the peer's advertised devices: real local
    /// output nodes that carry NO audio, only observe which app streams are routed
    /// into them. Keyed by the peer's device stream id. See docs/audio-design.md
    /// "Redesign: one system".
    intent_sinks: HashMap<u32, cursio_audio::IntentSink>,
    /// The peer device each Intent Sink represents: its `(handle, description)`,
    /// so a Remote Stream born from that sink's membership can be tagged to render
    /// into that exact device on the peer. Keyed by the intent sink's stream id.
    intent_sink_device: HashMap<u32, (String, String)>,
    /// Active Remote Streams born from Intent-Sink membership: an app funnelled
    /// into an Intent Sink gets a per-app tap here. Keyed by the app's local
    /// PipeWire node id, mapping to (our stream id, the tap).
    routed_apps: HashMap<u32, (u32, cursio_audio::Capture)>,
    /// The route (stream id, app name, target device) for each active Remote
    /// Stream, keyed by STREAM id — the source of truth for the replicated
    /// [`RoutingState`] snapshot the host sends. Keyed by stream rather than node
    /// because the stream is what persists across the app replacing its node;
    /// kept in step with `app_routes`.
    routed_app_route: HashMap<u32, cursio_protocol::RouteEntry>,
    /// Whether the routing map changed since we last sent a [`RoutingState`], so
    /// `pump` re-broadcasts the whole object on any membership change (and once at
    /// startup) rather than on every tick.
    routing_dirty: bool,
    /// Group gain/mute per Route, keyed by the Route's target device
    /// `(handle, description)` — the host-side authority for a Route's own
    /// gain/mute (docs/audio-design.md, "Route / Routing Group"). Absent ⇒ the
    /// group is at unity and unmuted. A group op updates this and fans the
    /// effective gain out to each member stream. See [`AudioEngine::on_route_op`].
    route_groups: HashMap<(String, String), RouteGroup>,
    /// Target devices whose Route state changed since we last replicated it, so
    /// `pump` re-sends the affected [`RouteState`]s (for a future UI) without
    /// spamming one per tick.
    routes_dirty: std::collections::HashSet<(String, String)>,
    /// Apps pending teardown after leaving an Intent Sink: node id -> when the
    /// grace period expires. A brief grace avoids tearing down on a transient
    /// re-route (the mixer moving a stream unlinks then relinks).
    routed_app_expiry: HashMap<u32, std::time::Instant>,
    /// The user's routing INTENT, keyed by app name: app name -> (our stream id,
    /// peer device). Outlives any particular PipeWire node.
    ///
    /// "Firefox plays into Voicemeeter" stays true while Firefox is paused, and
    /// a node id is only one incarnation of a client, so the two must not be the
    /// same fact. Tying the tunnel to the node id meant every seek, every
    /// pause, every relink tore down the Remote Stream and built a new one —
    /// visible in the log as `dropped stale app links`/`re-linked` every couple
    /// of seconds. Worse, clients on the ALSA compatibility layer (`PipeWire
    /// ALSA [java]` — Minecraft with Simple Voice Chat) replace their node
    /// outright when the audio client reopens, so the old id never returned and
    /// the app stayed silent until re-routed by hand.
    ///
    /// Keyed by name, the stream id and the peer's advertisement survive: a node
    /// going away re-points the tap, it does not close the tunnel.
    app_routes: HashMap<String, (u32, (String, String))>,
    /// Node id -> app name, so a `Left` event (which carries only the id) can
    /// find the route it belongs to.
    routed_app_names: HashMap<u32, String>,
    /// When each routed node was first tapped, so a departure can be read as a
    /// pause or as the app discarding a node it barely opened. See
    /// [`NODE_SETTLED_AGE`].
    routed_app_since: HashMap<u32, std::time::Instant>,
    /// Taps being opened on a worker thread, and the channel they land on.
    ///
    /// Opening a tap connects to the PipeWire daemon and waits for the stream to
    /// come up — tens of milliseconds, measured at up to 100. Done inline that
    /// stalls the whole session loop, and since the cursor shares this channel
    /// it is felt directly as the mouse jerking. Apps that churn their nodes
    /// (browsers, Steam) open taps constantly, so this was the session's largest
    /// source of stalls. The open happens on a worker; the finished tap is
    /// collected by a later pump.
    opening_taps: HashMap<u32, PendingTap>,
    tap_ready_tx: tokio::sync::mpsc::UnboundedSender<OpenedTap>,
    tap_ready_rx: tokio::sync::mpsc::UnboundedReceiver<OpenedTap>,
    /// Intent Sinks being created on a worker, by the peer device stream id.
    ///
    /// Same reason as `opening_taps`, and the worse case of the two: creating a
    /// sink waits for a PipeWire node to come up, measured here at ~430 ms
    /// EACH — and the peer advertises its devices in one burst, so a machine
    /// running Voicemeeter announced six and stalled the session for over two
    /// seconds. Nothing moved in that window: not the cursor, not audio, and
    /// not the file mount, whose own 3 s deadline it very nearly outlasted —
    /// Explorer reported the peer's drive as an I/O error on a link that was
    /// perfectly healthy.
    ///
    /// Tracked so a second advertisement for a device already being created
    /// does not start a duplicate node.
    opening_intent_sinks: std::collections::HashSet<u32>,
    /// What actually builds an Intent Sink. A field so tests can exercise the
    /// bookkeeping around it without publishing a real node into the desktop
    /// session the tests happen to run in.
    intent_sink_opener: IntentSinkOpener,
    intent_sink_ready_tx: tokio::sync::mpsc::UnboundedSender<OpenedIntentSink>,
    intent_sink_ready_rx: tokio::sync::mpsc::UnboundedReceiver<OpenedIntentSink>,
    /// Playbacks being opened on a worker, by stream id.
    ///
    /// Opened when an application appears on the peer, or on a stream's first
    /// chunk, so unlike the other two this stall arrives mid-session —
    /// which is what made the mouse freeze during ordinary use rather than only
    /// at connect time.
    opening_playbacks: std::collections::HashSet<u32>,
    playback_ready_tx: tokio::sync::mpsc::UnboundedSender<OpenedPlayback>,
    playback_ready_rx: tokio::sync::mpsc::UnboundedReceiver<OpenedPlayback>,
    /// Apps whose tunnel is open but whose node is currently absent, and since
    /// when. Cleared when the app comes back; see [`TUNNEL_IDLE_TIMEOUT`].
    tunnel_idle_since: HashMap<String, std::time::Instant>,
    /// Playback ends for the peer's streams, opened lazily (see below).
    playbacks: HashMap<u32, cursio_audio::Playback>,
    /// Logical gain requested for a stream, remembered so it survives a playback
    /// that hasn't opened yet (gain can arrive before the first chunk). Applied
    /// when the playback opens and re-applied on every [`Message::AudioGain`].
    /// Absent ⇒ unity. See [`AudioEngine::on_gain`].
    gains: HashMap<u32, f32>,
    /// The name each advertised stream renders under, kept for as long as the
    /// peer offers the stream so a failed open can retry under the right name.
    pending_labels: HashMap<u32, String>,
    /// What each stream is called in this machine's mixer, and what kind it is.
    /// Apart from `pending_labels`, which holds the system node's name
    /// ("Cursio(peer): app"): the mixer names the peer beside the row, and a
    /// microphone has no app name at all, which is how its rows read
    /// "stream 900800".
    mixer_names: HashMap<u32, (String, crate::mixer::StreamKind)>,
    /// For an inbound per-app stream tagged with a target device (the peer routed
    /// an app into one of OUR devices via its Intent Sink), the device to render
    /// it into. Keyed by stream id. Absent ⇒ render into the default output.
    inbound_app_device: HashMap<u32, String>,
    /// Streams we already failed to open, so the failure is reported once
    /// instead of on every subsequent chunk.
    failed_playbacks: std::collections::HashSet<u32>,
    /// Every stream the peer has advertised to us, open or not.
    ///
    /// Demand is reported from here rather than from `playbacks`, and the
    /// difference is the whole point: a playback exists only once a stream has
    /// been opened, so a stream that *cannot* be opened had no entry anywhere
    /// and therefore produced no demand message at all — neither "listening"
    /// nor "not listening". The owner treats silence as consent and keeps
    /// sending, forever.
    ///
    /// That is not hypothetical. Windows cannot publish a peer microphone as a
    /// recording device (no driver; `Playback::virtual_source` reports
    /// `Unsupported`), so a Linux machine's microphone streamed to it at
    /// ~200 chunks/s for an entire session with `played=18` to show for it —
    /// the only outbound audio on the link, and every byte of it wasted on a
    /// stream with nowhere to land.
    peer_streams: std::collections::HashSet<u32>,
    /// Watches our real devices' volume/mute (owner side) and applies changes to
    /// our Intent Sinks (non-owner side). `None` if the backend has no monitor
    /// (non-Linux today) — device-state replication is simply inert there.
    device_monitor: Option<cursio_audio::DeviceMonitor>,
    /// Node names of the real local devices we OWN (we advertised them), so a
    /// monitor change on one is broadcast as our authoritative [`DeviceState`].
    /// Mirrors the values of `inbound_targets`, kept as a set for O(1) lookup.
    owned_device_nodes: std::collections::HashSet<String>,
    /// Last state we broadcast per owned device, to suppress re-broadcasting an
    /// unchanged value (the monitor already dedups, this guards the wire).
    owned_device_state: HashMap<String, cursio_protocol::DeviceState>,
    /// For each peer device we mirror, the Intent Sink's local node name keyed by
    /// the peer's device handle — so an inbound [`Message::AudioDeviceState`] can
    /// be applied to the right Intent Sink, and a local turn of that Intent Sink
    /// can be attributed back to the peer device it represents.
    sink_node_for_peer_device: HashMap<String, String>,
    peer_device_for_sink_node: HashMap<String, String>,
    /// State we last APPLIED to an Intent Sink from an inbound replica, so the
    /// monitor change it provokes is recognised as our own echo and not bounced
    /// back to the owner as a fresh request. Keyed by Intent Sink node name.
    replica_echo: HashMap<String, cursio_protocol::DeviceState>,
    /// Windows: render the peer's audio into this output device, by name.
    render_device: Option<String>,
    /// The peer's display name, so labels stay unambiguous with 3+ machines.
    peer_name: String,
    sent: u64,
    played: u64,
    /// Streams the peer has told us nothing is playing, so we capture but do not
    /// send them. See [`AudioMsg::StreamDemand`].
    unwanted_streams: std::collections::HashSet<u32>,
    /// What we last told the peer about each inbound stream's local demand, so
    /// only changes go on the wire.
    demand_sent: HashMap<u32, bool>,
    /// Chunks dropped unsent because no one on the far side is listening.
    unwanted_dropped: u64,
    /// Chunks skipped because they carried only silence.
    silent_dropped: u64,
    /// Each outbound stream's silence gate. See [`crate::audio_gate`].
    gates: HashMap<u32, Gate>,
    /// How this machine's microphones are gated; read every pump, so a
    /// change in the app applies without restarting the module.
    mic_gate: crate::live::Shared<MicGate>,
    /// The mic gate the last health line reported, so a change is logged once.
    mic_gate_logged: Option<MicGate>,
    /// The listener's own volume per stream playing here. See [`LocalMixer`].
    mixer: LocalMixer,
    /// The mixer generation last applied, so a change is applied once.
    mixer_seen: u64,
    /// Chunks deferred because a single pump ran out of send budget.
    over_budget: u64,
    /// Chunks discarded as a stale backlog before they were ever encoded.
    ///
    /// Distinct from `over_budget`: that counts audio deferred to the next pump
    /// and still sent, this counts audio judged too old to be worth sending.
    stale_dropped: u64,
    /// When the periodic health line was last emitted.
    ///
    /// Throughput used to be logged every N chunks, which at ~100 chunks/s buried
    /// everything else in the file — the actual reason a stall is hard to
    /// diagnose. Logging on a wall-clock interval instead keeps the rate constant
    /// no matter how much audio is flowing.
    last_health_log: Option<std::time::Instant>,
    /// When a chunk was last received from the peer, for stall detection.
    last_chunk_at: Option<std::time::Instant>,
    /// Whether we have already reported the current stall, so one silent link
    /// produces one warning instead of a line per check.
    stall_reported: bool,
    /// Next free slot in the per-app id range; only ever counts up, so a Remote
    /// Stream id is never reused as apps join and leave Intent Sinks.
    next_app_slot: u32,
    /// Per-stream Opus encoders (outbound). One per stream: Opus is stateful,
    /// so frames from different sources must never share an encoder.
    encoders: HashMap<u32, cursio_audio::codec::OpusEncoder>,
    /// Encoded packets waiting for the next send budget, keyed by stream.
    ///
    /// One capture chunk can produce several 5 ms Opus packets. Keeping the
    /// unsent tail here prevents a budget boundary from creating an audible
    /// hole after the source chunk has already been removed from its channel.
    pending_packets: HashMap<u32, VecDeque<cursio_protocol::AudioChunk>>,
    /// Per-stream Opus decoders (inbound), for the same reason.
    decoders: HashMap<u32, cursio_audio::codec::OpusDecoder>,
    /// The `seq` of every chunk sent per outbound stream and not yet
    /// acknowledged, oldest first. Its length is the backlog rate control acts
    /// on.
    ///
    /// Counted in chunks actually *sent*, not as a difference of seqs. The seq
    /// comes from the capture and keeps counting through silence, whose chunks
    /// are dropped rather than sent — so after a pause the first packet out
    /// carries a seq thousands past the last one acknowledged. Measured as
    /// `last_sent - acked`, a 55 s pause between two videos read as a backlog
    /// of 10993 chunks, and rate control walked the stream down to its floor
    /// and back within two seconds of the audio resuming.
    unacked: HashMap<u32, VecDeque<u32>>,
    /// Outbound streams that sent anything since rate control last looked at
    /// them. A stream that sent nothing has produced no new evidence about the
    /// link, and judging it again on the old evidence repeats the last verdict.
    sent_since_adapt: std::collections::HashSet<u32>,
    /// How many consecutive evaluations each outbound stream has found the link
    /// caught up; see [`BITRATE_RAISE_AFTER`].
    caught_up_streak: HashMap<u32, u32>,
    /// Highest chunk `seq` the peer reports having *played* per outbound stream,
    /// from [`Message::AudioFrameAck`]. See [`AudioEngine::on_ack`] /
    /// [`AudioEngine::adapt_bitrate`].
    acked_seq: HashMap<u32, u32>,
    /// Current target Opus bitrate per outbound stream, lowered under a sustained
    /// backlog and raised back when it clears — the design's degrade-not-buffer
    /// rule. Absent ⇒ the codec default.
    stream_bitrate: HashMap<u32, u32>,
    /// When rate control last re-evaluated each outbound stream, so it acts on a
    /// wall-clock cadence rather than per chunk.
    last_adapt: HashMap<u32, std::time::Instant>,
    /// Highest chunk `seq` we have *received* per inbound stream, and when we last
    /// sent an ack for it — so acks stay sparse (a few per second), not one per
    /// chunk. See [`AudioEngine::pump`].
    recv_seq: HashMap<u32, u32>,
    recv_timing: HashMap<u32, (std::time::Instant, u64)>,
    max_arrival_gap: std::time::Duration,
    max_arrival_lateness: std::time::Duration,
    max_decode_time: std::time::Duration,
    capture_seq_skipped: u64,
    last_ack_sent: HashMap<u32, std::time::Instant>,
    /// The peer, for sound made here to reach it ([`crate::audio_relay`]).
    /// `None` only in tests that build an engine without a link.
    peer: Option<PeerId>,
    /// Marks this link as one sound can be relayed over while it runs.
    _relay_mark: Option<crate::audio_relay::LinkMark>,
    /// Sound handed over in-process, sent to the peer as application streams.
    relays: HashMap<
        u32,
        (
            std::sync::Arc<crate::audio_relay::Relay>,
            tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::AudioChunk>,
        ),
    >,
    /// The first microphone the peer offered: what a relay that wants a
    /// microphone hears.
    peer_mic: Option<u32>,
    /// Streams from the peer carrying a phone call's voice, by the phone's
    /// name, so this machine's calls show where the voice is.
    inbound_calls: InboundCalls,
}

impl AudioEngine {
    /// Create the audio module. Nothing touches the wire or the audio graph
    /// until [`SessionModule::start`] runs.
    ///
    /// `local_id` is *this* node's stable [`PeerId`]; it namespaces the stream
    /// ids we allocate.
    pub fn new(
        local_id: PeerId,
        render_device: Option<String>,
        mic_gate: crate::live::Shared<MicGate>,
        mixer: LocalMixer,
        peer_name: String,
    ) -> Self {
        let id_base = id_base_for(local_id);
        let peer_name_for_calls = peer_name.clone();
        let (tap_ready_tx, tap_ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let (intent_sink_ready_tx, intent_sink_ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let (playback_ready_tx, playback_ready_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            id_base,
            inbound_targets: HashMap::new(),
            mic_captures: HashMap::new(),
            mic_devices: HashMap::new(),
            intent_sinks: HashMap::new(),
            intent_sink_device: HashMap::new(),
            routed_apps: HashMap::new(),
            routed_app_route: HashMap::new(),
            routing_dirty: false,
            route_groups: HashMap::new(),
            routes_dirty: std::collections::HashSet::new(),
            routed_app_expiry: HashMap::new(),
            app_routes: HashMap::new(),
            routed_app_names: HashMap::new(),
            routed_app_since: HashMap::new(),
            opening_taps: HashMap::new(),
            tap_ready_tx,
            tap_ready_rx,
            opening_intent_sinks: std::collections::HashSet::new(),
            intent_sink_opener: cursio_audio::IntentSink::create,
            intent_sink_ready_tx,
            intent_sink_ready_rx,
            opening_playbacks: std::collections::HashSet::new(),
            playback_ready_tx,
            playback_ready_rx,
            tunnel_idle_since: HashMap::new(),
            playbacks: HashMap::new(),
            gains: HashMap::new(),
            pending_labels: HashMap::new(),
            mixer_names: HashMap::new(),
            inbound_app_device: HashMap::new(),
            failed_playbacks: std::collections::HashSet::new(),
            peer_streams: std::collections::HashSet::new(),
            device_monitor: None,
            owned_device_nodes: std::collections::HashSet::new(),
            owned_device_state: HashMap::new(),
            sink_node_for_peer_device: HashMap::new(),
            peer_device_for_sink_node: HashMap::new(),
            replica_echo: HashMap::new(),
            render_device,
            peer_name,
            sent: 0,
            played: 0,
            silent_dropped: 0,
            gates: HashMap::new(),
            mic_gate,
            mic_gate_logged: None,
            mixer,
            mixer_seen: 0,
            unwanted_streams: std::collections::HashSet::new(),
            demand_sent: HashMap::new(),
            unwanted_dropped: 0,
            stale_dropped: 0,
            over_budget: 0,
            last_health_log: None,
            last_chunk_at: None,
            stall_reported: false,
            next_app_slot: 0,
            encoders: HashMap::new(),
            pending_packets: HashMap::new(),
            decoders: HashMap::new(),
            unacked: HashMap::new(),
            sent_since_adapt: std::collections::HashSet::new(),
            caught_up_streak: HashMap::new(),
            acked_seq: HashMap::new(),
            stream_bitrate: HashMap::new(),
            last_adapt: HashMap::new(),
            recv_seq: HashMap::new(),
            recv_timing: HashMap::new(),
            max_arrival_gap: std::time::Duration::ZERO,
            max_arrival_lateness: std::time::Duration::ZERO,
            max_decode_time: std::time::Duration::ZERO,
            capture_seq_skipped: 0,
            last_ack_sent: HashMap::new(),
            peer: None,
            _relay_mark: None,
            relays: HashMap::new(),
            peer_mic: None,
            inbound_calls: InboundCalls {
                peer_name: peer_name_for_calls,
                streams: HashMap::new(),
            },
        }
    }

    /// Take sound made on this machine for `peer` ([`crate::audio_relay`]).
    pub fn relaying_to(mut self, peer: PeerId) -> Self {
        self._relay_mark = Some(crate::audio_relay::LinkMark::new(peer, &self.peer_name));
        self.peer = Some(peer);
        self
    }
}

/// Builds [`AudioEngine`] when audio sharing is enabled.
pub struct AudioFactory {
    pub enabled: bool,
    /// Windows: render the peer's audio into the output device whose name
    /// contains this. Shared so a change reaches the next engine built; the
    /// node restarts the module on every link to make that now.
    pub render_device: crate::live::Shared<Option<String>>,
    /// How this machine's microphones are gated. Read live by every engine.
    pub mic_gate: crate::live::Shared<MicGate>,
    /// The listener's volumes, shared by every link's engine.
    pub mixer: LocalMixer,
}

/// Whether this build has an audio backend at all.
///
/// Unlike files — where serving is portable and only mounting needs the OS —
/// both halves of audio are the platform's: PipeWire on Linux, WASAPI plus the
/// virtual device on Windows, AAudio on Android, and nothing on macOS yet.
/// Advertising it anyway is the exact failure module negotiation exists to
/// prevent: the peer agrees to run audio, then every device enumeration comes
/// back empty and every inbound stream is refused one warning at a time.
const SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "android"
));

impl crate::module::ModuleFactory for AudioFactory {
    fn id(&self) -> ModuleId {
        ModuleId::AUDIO
    }

    fn supported(&self) -> bool {
        SUPPORTED
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        (self.enabled && SUPPORTED)
            .then(|| cursio_protocol::ModuleAdvert::symmetric(ModuleId::AUDIO, VERSION))
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(
            AudioEngine::new(
                ctx.local.peer_id,
                self.render_device.get(),
                self.mic_gate.clone(),
                self.mixer.clone(),
                ctx.peer_name().to_string(),
            )
            .relaying_to(ctx.peer.peer_id),
        ))
    }
}

/// Version of this module's wire vocabulary ([`AudioMsg`]). Bump when those
/// types change shape; peers run the module only on an exact match.
pub const VERSION: u16 = 1;
