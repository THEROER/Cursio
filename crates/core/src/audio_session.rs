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

use async_trait::async_trait;
use cursio_protocol::{AudioMsg, Message, ModuleId, PeerId};
use cursio_transport::{TransportError, TransportTx};

use crate::audio_gate::{Gate, GateKind, MicGate};
pub use crate::mixer::LocalMixer;
use crate::module::SessionModule;

/// Put one audio *control* message on the wire, enveloped for this module.
///
/// Chunks do not go through here — they stay a first-class `Message::AudioChunk`
/// and are sent directly, because the per-chunk rate makes an extra serialize
/// the one cost the envelope cannot justify. See
/// [`AudioMsg`](cursio_protocol::AudioMsg).
async fn send_control(tx: &mut dyn TransportTx, msg: AudioMsg) -> Result<(), TransportError> {
    let framed = Message::module(ModuleId::AUDIO, AudioMsg::PRIORITY, &msg)?;
    tx.send(&framed).await
}

/// A tap whose open is in flight, remembered so the node's departure can be
/// handled even while its tap is still being built.
struct PendingTap {
    stream_id: u32,
    app_name: String,
    device: (String, String),
    /// Whether this reuses a tunnel the peer already holds. A resumed stream
    /// must not re-advertise: the peer would tear down and rebuild its playback
    /// node, which is the churn this whole path exists to avoid.
    resumed: bool,
    /// Set when the node leaves before its tap is ready, so the finished tap is
    /// dropped on arrival instead of being wired to a node that is gone.
    abandoned: bool,
}

/// The result of opening a tap on a worker thread.
struct OpenedTap {
    app_node_id: u32,
    result: Result<cursio_audio::Capture, String>,
}

/// The result of creating an Intent Sink on a worker thread.
///
/// Carries the identity the sink was created for, because the advertisement it
/// answers is long gone by the time it lands.
struct OpenedIntentSink {
    /// The peer's device stream id this sink represents.
    id: u32,
    /// PipeWire node name, and the peer device `(handle, description)` behind it.
    node: String,
    device: (String, String),
    result: Result<cursio_audio::IntentSink, cursio_audio::AudioError>,
}

/// Builds an Intent Sink from its stream id, node name and description.
type IntentSinkOpener =
    fn(u32, String, String) -> Result<cursio_audio::IntentSink, cursio_audio::AudioError>;

/// The result of opening a playback on a worker thread.
struct OpenedPlayback {
    /// Stream id the playback renders.
    id: u32,
    result: Result<cursio_audio::Playback, cursio_audio::AudioError>,
}

/// Whether this process has said it cannot publish a peer's devices.
static VIRTUAL_DEVICES_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// What to install so a peer's outputs and microphones appear here.
#[cfg(target_os = "windows")]
const VIRTUAL_DEVICES_HINT: &str =
    "install the Cursio virtual audio driver (native/cursio-vad) to get them";
#[cfg(not(target_os = "windows"))]
const VIRTUAL_DEVICES_HINT: &str = "this platform has no backend for them yet";

/// Say once per process that a peer's devices cannot be published here.
///
/// A peer's outputs (as Intent Sinks) and its microphone (as a recording
/// device) need a virtual audio endpoint, which Windows cannot create from
/// user mode. That is a capability this machine lacks, not a failure: every
/// device the peer advertises, on every reconnect, hits it again, and logging
/// each as an error buried the one line that says what to install. The
/// streams themselves are refused through demand either way, so nothing is
/// sent for them. Returns whether this call was the one that reported.
fn report_no_virtual_devices(reported: &std::sync::atomic::AtomicBool, what: &str) -> bool {
    if reported.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::debug!(%what, "peer audio device not published here (no virtual devices)");
        return false;
    }
    tracing::warn!(
        %what,
        "this machine cannot publish a peer's audio outputs or microphones; {VIRTUAL_DEVICES_HINT} \
         (application audio from the peer still plays)"
    );
    crate::health::audio_devices_unavailable(VIRTUAL_DEVICES_HINT, cfg!(target_os = "windows"));
    true
}

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
    /// can use them as its own inputs.
    mic_captures: HashMap<u32, cursio_audio::Capture>,
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
    last_ack_sent: HashMap<u32, std::time::Instant>,
}

/// Host-side group gain/mute for one Route (all streams funnelled into one target
/// device). Its `effective_gain` is fanned out to each member as a per-stream
/// [`Message::AudioGain`]. See docs/audio-design.md, "Route / Routing Group".
#[derive(Debug, Clone, Copy)]
struct RouteGroup {
    /// Group gain, linear (1.0 = unity).
    gain: f32,
    /// Whether the whole group is muted.
    mute: bool,
}

impl RouteGroup {
    /// The gain actually applied to each member: the group gain, or zero when the
    /// group is muted (mute wins over gain, and is reversible without losing the
    /// gain value).
    fn effective_gain(&self) -> f32 {
        if self.mute {
            0.0
        } else {
            self.gain
        }
    }
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
        let (tap_ready_tx, tap_ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let (intent_sink_ready_tx, intent_sink_ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let (playback_ready_tx, playback_ready_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            id_base,
            inbound_targets: HashMap::new(),
            mic_captures: HashMap::new(),
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
            last_ack_sent: HashMap::new(),
        }
    }
}

#[async_trait]
impl SessionModule for AudioEngine {
    fn name(&self) -> &'static str {
        "audio"
    }

    fn id(&self) -> ModuleId {
        ModuleId::AUDIO
    }

    /// Advertise everything this machine can offer: one stream per output
    /// device, one per input device (microphone), and one per playing
    /// application. There is no global "everything" stream — only apps and
    /// devices cross the boundary (see docs/audio-design.md "Redesign: one
    /// system").
    ///
    /// The peer mirrors each advertisement, so *its* output list shows our
    /// devices individually ("Cursio: Realtek …") and each of our apps arrives as
    /// its own volume-controllable node.
    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let id_base = self.id_base;

        // Blocking, like the app scan in `pump` — but this runs once in `start`,
        // before the session loop exists, so there is nothing to stall.
        let devices = cursio_audio::list_output_devices().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not list output devices; none offered");
            Vec::new()
        });
        tracing::info!(count = devices.len(), "advertising local output devices");

        for (i, dev) in devices.iter().enumerate() {
            let id = id_base + 1 + i as u32;
            self.inbound_targets.insert(id, dev.id.clone());
            // We advertised it ⇒ we OWN this device's replicated state.
            self.owned_device_nodes.insert(dev.id.clone());
            let meta = cursio_protocol::AudioStreamMeta {
                id,
                role: cursio_protocol::StreamRole::Playback,
                app_name: None,
                device: Some((dev.id.clone(), dev.description.clone())),
                format: cursio_protocol::AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                    sample_kind: cursio_protocol::SampleKind::F32,
                },
                codec: cursio_protocol::AudioCodec::Opus,
            };
            tracing::info!(id, device = %dev.description, "advertising device");
            send_control(tx, AudioMsg::StreamAdvertise(meta)).await?;
        }

        // Start the device monitor: it observes our real devices' volume/mute
        // (owner side, broadcast in `pump`) and applies replicas to our Intent
        // Sinks (non-owner side). Best-effort — if it fails, device-state
        // replication is simply absent; audio streaming is unaffected.
        match cursio_audio::DeviceMonitor::start() {
            Ok(m) => {
                self.device_monitor = Some(m);
                tracing::info!(
                    "device-state monitor up — device volumes replicate across machines"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "no device-state monitor; device volumes won't replicate");
            }
        }

        // Microphones, offered as sources the peer can record from. Same shape
        // as the outputs above, but advertised with `StreamRole::Capture` — that
        // is the flag telling the peer to publish a virtual *microphone* instead
        // of a virtual speaker.
        let inputs = cursio_audio::list_input_devices().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not list input devices; no microphones offered");
            Vec::new()
        });
        tracing::info!(count = inputs.len(), "advertising local input devices");

        for (i, dev) in inputs.iter().enumerate() {
            let id = id_base + MIC_ID_OFFSET + i as u32;
            // Start recording it now: the peer only ever *pulls* audio, so the
            // capture has to exist for chunks to flow once it opens the mirror.
            let cap = match cursio_audio::Capture::mic_tap(id, Some(dev.id.clone())) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, device = %dev.description, "cannot record this input");
                    continue;
                }
            };
            let meta = cursio_protocol::AudioStreamMeta {
                id,
                role: cursio_protocol::StreamRole::Capture,
                app_name: None,
                device: Some((dev.id.clone(), dev.description.clone())),
                format: cursio_protocol::AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                    sample_kind: cursio_protocol::SampleKind::F32,
                },
                codec: cursio_protocol::AudioCodec::Opus,
            };
            tracing::info!(id, device = %dev.description, "advertising microphone");
            send_control(tx, AudioMsg::StreamAdvertise(meta)).await?;
            self.mic_captures.insert(id, cap);
        }

        // NOTE: there is deliberately NO global "everything" stream. It used to
        // capture the whole default sink and ship it to the peer, but in the
        // unified "one system" model that double-ships every app (it already goes
        // per-app) and re-exports the peer's own device mirrors back to it — the
        // duplicate-audio and stray-mixer-row bug. Only apps and devices cross the
        // boundary now. See docs/audio-design.md "Redesign: one system".
        //
        // NOTE: we do NOT blindly tap every playing application. In the unified
        // model a per-app Remote Stream is created ONLY when the app is funnelled
        // into an Intent Sink (a peer device) — see `pump_intent_sinks`. Tapping
        // every app up-front (the old `scan_apps`) both duplicated audio (the app
        // also reached the peer through the default sink's Intent Sink) and filled
        // the peer's mixer with idle rows. Membership is the sole trigger now.

        // Send an initial RoutingState (empty for now) on the next pump, so a
        // reconnecting peer starts from a known, authoritative map rather than
        // stale routes from a prior session. It fills in as apps get funnelled.
        self.routing_dirty = true;

        tracing::info!("audio up — the peer's devices appear here as 'Cursio: <device>' outputs");
        Ok(())
    }

    /// Send whatever the local captures have produced since the last call.
    ///
    /// Takes at most a **few chunks per stream**, then returns. Draining each
    /// channel dry (`while let Ok(..)`) starves the caller's `select!`: with
    /// many live streams the audio never stops arriving, so the inbound branch
    /// is never reached and everything the peer sends is silently ignored (this
    /// really happened — the peer's advertisements never got read). The caller
    /// additionally polls `biased`, inbound first, for the same reason.
    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        // Runs every pump, not just when audio moves: a stall is precisely the
        // case where the send path below never executes, so checking only there
        // would stay silent exactly when a warning is wanted.
        self.log_health();

        // Take delivery of anything a worker finished building. First, so a node
        // that came up since the last pump is usable for the rest of this one.
        self.collect_opened_intent_sinks();
        self.collect_opened_playbacks();

        let generation = self.mixer.generation();
        if generation != self.mixer_seen {
            self.mixer_seen = generation;
            let ids: Vec<u32> = self.playbacks.keys().copied().collect();
            for id in ids {
                self.apply_gain(id);
            }
        }

        // Turn Intent-Sink membership into per-app Remote Streams: apps funnelled
        // into a peer device's Intent Sink get tapped and shipped, tagged to render
        // into that device; apps that left are torn down after a grace period.
        self.pump_intent_sinks(tx).await?;

        // Tell the owner which of its streams anything here is actually playing,
        // so it can stay quiet on the rest. See `AudioMsg::StreamDemand`.
        self.pump_demand(tx).await?;

        let mut budget = MAX_CHUNKS_PER_PUMP;

        // Every live stream gets an equal share of the pump.
        //
        // The budget used to be one pot each stream drained `while budget > 0`,
        // so the first stream served took everything it had queued and the rest
        // shared the remainder. In ordinary use the pot is nowhere near
        // exhausted (`over_budget` stays at zero), so this is not about
        // capacity — it is about order: the iteration comes from a `HashMap`,
        // so *which* stream is served first varies, and a burst from one stream
        // delays the others by however long its backlog takes.
        //
        // At least one each, so a stream is never handed a share of zero and
        // silenced outright by arithmetic; the whole-pump ceiling still bounds
        // how long this blocks the session loop.
        let live_streams = self.routed_apps.len() + self.mic_captures.len();
        let share = (MAX_CHUNKS_PER_PUMP / live_streams.max(1)).max(1);

        // `routed_apps` holds `(stream_id, Capture)` — the per-app Remote Streams
        // born from Intent-Sink membership. Pump them alongside our mic taps.
        // (Device Intent Sinks carry no audio; they only observe membership.)
        let mic_kind = GateKind::Mic(self.mic_gate.get());
        let routed = self
            .routed_apps
            .iter_mut()
            .map(|(_, (sid, cap))| (&*sid, cap, GateKind::Program));
        let mics = self
            .mic_captures
            .iter_mut()
            .map(|(id, cap)| (id, cap, mic_kind));
        for (id, cap, gate_kind) in routed.chain(mics) {
            // Stop as soon as the budget is gone, leaving the rest queued for
            // the next pump rather than discarding it. Dropping audible chunks
            // to stay under budget is itself heard as broken audio — the budget
            // exists to bound how long one pump blocks the session loop (input
            // shares this channel), not to throw sound away.
            if budget == 0 {
                break;
            }
            // This stream's slice of what is left. Capped by the pot so the sum
            // of the slices can never exceed the whole-pump ceiling.
            let allotted = share.min(budget);
            let mut budget_for_stream = allotted;
            // Nobody on the far side is playing this stream: drain what the
            // capture produced and send none of it.
            //
            // The capture keeps running (so the stream resumes instantly when a
            // consumer appears, with no renegotiation) but its output goes
            // nowhere. This is what stops a peer mirroring 13 endpoints from
            // streaming all 13 into nodes nothing has opened.
            if self.unwanted_streams.contains(id) {
                if let Some(pending) = self.pending_packets.remove(id) {
                    self.unwanted_dropped += pending.len() as u64;
                }
                let drained = drain_stale(cap.chunks(), 0);
                if drained > 0 {
                    self.unwanted_dropped += drained as u64;
                }
                continue;
            }
            // Discard a stale backlog before encoding anything.
            //
            // A capture starts producing the moment it is created and its channel
            // is unbounded, so between creation (or any pump that ran out of
            // budget) and now, chunks pile up. Sending them is worse than useless:
            // they are already too old to be wanted, yet they still cost link
            // capacity — and since input rides this same channel, that cost is
            // felt as mouse lag for as long as the backlog takes to drain. This is
            // what made the first seconds after connecting stutter.
            //
            // Same trade as the playback jitter buffer: for live audio freshness
            // beats completeness, so keep the newest and drop what is behind.
            let dropped = drain_stale(cap.chunks(), MAX_CHUNK_BACKLOG);
            if dropped > 0 {
                self.stale_dropped += dropped as u64;
            }
            while budget_for_stream > 0 {
                if let Some(packet) = self
                    .pending_packets
                    .get_mut(id)
                    .and_then(|packets| take_with_budget(packets, &mut budget_for_stream))
                {
                    let seq = packet.seq;
                    tx.send(&Message::AudioChunk(packet)).await?;
                    record_sent(&mut self.unacked, &mut self.sent_since_adapt, *id, seq);
                    self.sent += 1;
                    continue;
                }

                let Ok(chunk) = cap.chunks().try_recv() else {
                    break;
                };
                // A virtual sink produces chunks continuously whether or not
                // anything is playing into it, so most streams are pumping pure
                // silence. Sending it wastes the link and — because input rides
                // the same channel — is felt directly as mouse lag. Nothing is
                // lost by dropping it: silence reconstructs as silence, and the
                // receiver already fills gaps with silence on underrun. What
                // counts as silence differs for a microphone; see `audio_gate`.
                if !self
                    .gates
                    .entry(*id)
                    .or_default()
                    .admit(gate_kind, &chunk.data)
                {
                    self.silent_dropped += 1;
                    continue;
                }
                // Compress before sending. Raw f32 is ~3 Mbit/s per stream,
                // which saturates the link that input also rides on; Opus
                // carries the same audio at ~100 kbit/s. One encoder per
                // stream — Opus is stateful and interleaving frames from
                // different sources would corrupt both.
                let bitrate = self
                    .stream_bitrate
                    .get(id)
                    .copied()
                    .unwrap_or(BITRATE_START);
                let encoder = match self.encoders.entry(*id) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        // Built at the stream's *current* target bitrate, so a
                        // stream that restarts resumes at the rate the link was
                        // last judged able to carry. Later changes are applied to
                        // this encoder in place (see `adapt_bitrate`).
                        match cursio_audio::codec::OpusEncoder::with_bitrate(bitrate) {
                            Ok(enc) => e.insert(enc),
                            Err(err) => {
                                tracing::warn!(error = %err, id, "no encoder for stream");
                                break;
                            }
                        }
                    }
                };
                let packets = match encoder.encode(&chunk.data) {
                    Ok(p) => p,
                    Err(err) => {
                        tracing::warn!(error = %err, id, "opus encode failed");
                        continue;
                    }
                };
                // A chunk smaller than one Opus frame yields nothing yet: the
                // remainder is buffered in the encoder until it completes.
                // Larger chunks may yield several packets; enqueue every one
                // before the next loop iteration spends the remaining budget.
                let pending = self.pending_packets.entry(*id).or_default();
                pending.extend(packets.into_iter().map(|data| cursio_protocol::AudioChunk {
                    id: *id,
                    seq: chunk.seq,
                    timestamp: chunk.timestamp,
                    data,
                }));
                // (health logging happens once per pump, above — not per stream)
            }
            // Charge what this stream actually used against the whole-pump pot,
            // and hand back what it did not: a quiet stream leaves its share for
            // the others rather than reserving capacity it has no audio for.
            budget -= allotted - budget_for_stream;
            if budget_for_stream == 0
                && (self
                    .pending_packets
                    .get(id)
                    .is_some_and(|packets| !packets.is_empty())
                    || !cap.chunks().is_empty())
            {
                self.over_budget += 1;
            }
        }

        // Feed the sender's rate control: sparse acks for what we've received,
        // and a bitrate re-evaluation for what we send.
        self.send_acks(tx).await?;
        self.adapt_bitrate();

        // Device-state replication: drain monitor changes into broadcasts/requests
        // and flush anything queued by `handle_message`.
        self.pump_device_state(tx).await?;

        // Routing replication: re-broadcast the whole RoutingState whenever it
        // changed, so a reconnecting peer gets the full map in one idempotent
        // object instead of waiting for link events to re-fire.
        if self.routing_dirty {
            self.routing_dirty = false;
            let routes: Vec<cursio_protocol::RouteEntry> =
                self.routed_app_route.values().cloned().collect();
            tracing::info!(count = routes.len(), "broadcasting routing state");
            send_control(
                tx,
                AudioMsg::RoutingState(cursio_protocol::RoutingState { routes }),
            )
            .await?;
        }

        // Route group ops: fan out changed group gain/mute to members and
        // replicate each affected RouteState.
        self.pump_routes(tx).await?;
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        // Chunks first: they are the overwhelming majority of inbound audio
        // frames, and they are the one variant that stayed first-class. Checking
        // them before attempting an envelope decode keeps the hot path cheapest.
        if let Message::AudioChunk(chunk) = msg {
            self.on_chunk(chunk);
            return true;
        }
        let control = match msg.decode_module::<AudioMsg>(ModuleId::AUDIO) {
            Ok(Some(control)) => control,
            Ok(None) => return false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable audio message from peer");
                return true;
            }
        };
        match control {
            AudioMsg::StreamAdvertise(meta) => {
                self.on_advertise(&meta);
                true
            }
            AudioMsg::Gain { id, gain } => {
                self.on_gain(id, gain);
                true
            }
            AudioMsg::StreamDemand { id, listening } => {
                if listening {
                    self.unwanted_streams.remove(&id);
                } else {
                    self.unwanted_streams.insert(id);
                }
                tracing::info!(id, listening, "peer reported demand for one of our streams");
                true
            }
            AudioMsg::FrameAck(ack) => {
                self.on_ack(&ack);
                true
            }
            AudioMsg::DeviceStateSet { device, field } => {
                self.on_device_state_set(&device, field);
                true
            }
            AudioMsg::DeviceState { device, state } => {
                self.on_device_state_replica(&device, state);
                true
            }
            AudioMsg::StreamRoute(entry) => {
                self.on_stream_route(&entry);
                true
            }
            AudioMsg::RoutingState(state) => {
                self.on_routing_state(&state);
                true
            }
            AudioMsg::RouteOp { target_device, op } => {
                self.on_route_op(target_device, op);
                true
            }
            AudioMsg::RouteState(state) => {
                // Receiver side: a Route's group state, for a future UI. We hold
                // no UI yet, so just log it — reflecting it never mutates routing
                // (invariant #4: only the host writes it).
                tracing::info!(
                    device = %state.target_device.1,
                    members = state.members.len(),
                    gain = state.gain,
                    mute = state.mute,
                    "peer replicated a route's group state"
                );
                true
            }
            AudioMsg::StreamClose { id, reason } => {
                // The peer tore down a stream (e.g. an app left an Intent Sink).
                // Drop our playback and any per-stream state so its mixer row and
                // decoder go away.
                tracing::info!(id, ?reason, "peer closed a stream; dropping playback");
                self.playbacks.remove(&id);
                self.pending_labels.remove(&id);
                self.inbound_app_device.remove(&id);
                self.decoders.remove(&id);
                self.failed_playbacks.remove(&id);
                self.peer_streams.remove(&id);
                self.opening_playbacks.remove(&id);
                // Receiver-side rate-control/gain state for this stream — clear it
                // so a reused id never inherits a stale gain or ack cadence.
                self.gains.remove(&id);
                self.mixer.remove(id);
                self.recv_seq.remove(&id);
                self.demand_sent.remove(&id);
                self.unwanted_streams.remove(&id);
                self.last_ack_sent.remove(&id);
                true
            }
        }
    }
}

/// How often the periodic audio health line is emitted.
///
/// Deliberately time-based, not chunk-based. Logging every N chunks scales with
/// throughput, so at ~100 chunks/s the file filled with thousands of identical
/// counters and buried the events that explain a problem.
const HEALTH_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// How long without a chunk from the peer counts as a stall worth warning about.
///
/// Well above any normal jitter (chunks arrive every ~10 ms), so this only fires
/// when audio has genuinely stopped rather than merely hiccuped.
const AUDIO_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

/// Grace before an app's capture tap is released after its node leaves an
/// Intent Sink.
///
/// The mixer moving a stream from one sink to another unlinks then relinks
/// within a frame; without a grace period that would drop and rebuild the tap
/// (and its audio) on every re-route. A short delay lets a relink cancel it.
///
/// This releases only the tap. The Remote Stream it feeds stays open — see
/// [`AudioEngine::app_routes`] — so a pause costs nothing beyond the tap.
const ROUTED_APP_GRACE: std::time::Duration = std::time::Duration::from_millis(800);

/// How long an app's tunnel is held open after its node disappears.
///
/// Distinguishes "paused, seeking, or reopening its audio client" from "quit".
/// Well beyond any of the former — a Minecraft world load or a Simple Voice Chat
/// reconnect replaces the node within seconds — while still releasing the stream
/// of an app the user actually closed, rather than holding it for the session.
const TUNNEL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a node must have lived before its departure counts as a pause
/// rather than the app discarding a node it never really used.
///
/// An app holding several nodes at once made every departure look like a
/// leftover, because *some* other node was always live — so the tunnel was
/// closed immediately and [`AudioEngine::app_routes`] lost its entry. Measured
/// over a day of real use, that path fired 72 times and the reuse it exists to
/// enable ("app returned; re-pointed its tap") fired **zero** times: Chromium
/// keeps a node per tab, so pausing a video always raced against a sibling.
///
/// Age separates the two cases the `still_live` test could not. A node the app
/// opens and drops while starting up (Minecraft, browsers probing devices) is
/// gone within a second or two; a node that actually played audio has lived far
/// longer by the time the user pauses it. Below this, the tunnel is a leftover
/// and closing it is right — that is what keeps a dead stream from answering
/// the peer's demand in place of the live one. Above it, the app is pausing and
/// the tunnel must survive.
const NODE_SETTLED_AGE: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a departed node's tunnel is a startup leftover, to be closed at once,
/// rather than an app pausing, whose tunnel must survive.
///
/// A leftover is silent but still advertised, and the peer picks by
/// advertisement order — so leaving one up had it answer demand in place of the
/// node actually producing sound. Being outlived by a sibling is not enough to
/// call it one, though: an app with a node per tab always has a sibling, which
/// made every pause look like a leftover. Age is the part that separates them.
fn is_startup_leftover(still_live: bool, age: std::time::Duration) -> bool {
    still_live && age < NODE_SETTLED_AGE
}

/// How often we return an [`AudioFrameAck`] per inbound stream.
///
/// The design says acks are "sent sparsely (a few per second), not per chunk":
/// they only drive slow bitrate decisions, so acking every ~10 ms chunk would
/// flood the shared channel (felt as input lag) for no benefit. 250 ms = ~4/s.
const ACK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// How often each outbound stream re-evaluates its bitrate.
///
/// Rate control must react slowly and hysteretically: reacting per chunk would
/// oscillate audibly. Half a second lets a real trend (a genuinely congested
/// link) show through transient jitter.
const ADAPT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Sent-minus-acked gap (in chunks) above which a stream is "behind" and should
/// shed bitrate. At ~200 chunks/s a gap of 100 is ~0.5 s of unplayed audio — a
/// real backlog, not ordinary jitter-buffer slack.
const BACKLOG_HIGH: u32 = 100;

/// Gap below which the link is considered caught up, so bitrate may climb back.
/// Kept well under [`BACKLOG_HIGH`] so the two thresholds don't chatter.
const BACKLOG_LOW: u32 = 20;

/// Consecutive caught-up evaluations before the bitrate may climb one step.
///
/// Down stays immediate — a backlog is felt as mouse lag, since input shares
/// the link — but up waits for the link to hold for 1.5 s. Climbing on the
/// first calm sample let a single hiccup flap a stream between two rates, which
/// the logs showed many times a day, seconds apart.
const BITRATE_RAISE_AFTER: u32 = 3;

/// Most unacknowledged chunks remembered per stream.
///
/// Only a bound on memory for a peer that stopped acking altogether: the
/// backlog is already far past [`BACKLOG_HIGH`] long before this is reached, so
/// forgetting the oldest changes no decision.
const UNACKED_CAP: usize = 4096;

/// Note that a chunk of stream `id` went out, for rate control.
///
/// One chunk becomes several Opus packets sharing its seq; it is recorded once,
/// so the backlog stays in the chunks [`BACKLOG_HIGH`] is written in.
fn record_sent(
    unacked: &mut HashMap<u32, VecDeque<u32>>,
    sent_since_adapt: &mut std::collections::HashSet<u32>,
    id: u32,
    seq: u32,
) {
    let queue = unacked.entry(id).or_default();
    if queue.back() != Some(&seq) {
        queue.push_back(seq);
        if queue.len() > UNACKED_CAP {
            queue.pop_front();
        }
    }
    sent_since_adapt.insert(id);
}

/// Bitrate floor and ceiling for adaptation (bits/s). The ceiling is the codec
/// default; the floor stays intelligible for speech/music while cutting the
/// link cost roughly in half at each step.
const BITRATE_MIN: u32 = 24_000;
const BITRATE_STEP: u32 = 24_000;

/// Bitrate a stream *starts* at, before any ack has proved what the link can
/// carry.
///
/// Deliberately below the codec default. Rate control is reactive: it needs an
/// ack ([`ACK_INTERVAL`], 250 ms) and then an adaptation tick
/// ([`ADAPT_INTERVAL`], 500 ms) before it can react at all, and it steps down
/// one [`BITRATE_STEP`] at a time — so starting at the ceiling means every
/// stream floods the link for the first seconds of a session, all at once,
/// while rate control slowly walks each one down. Input shares this channel, so
/// that opening burst is felt as mouse lag exactly when the user is reconnecting
/// and most likely to be moving the pointer.
///
/// Starting low inverts the risk: the first seconds are quiet-but-cheap, and the
/// climb back to [`DEFAULT_BITRATE`] happens within a few adaptation ticks once
/// acks show the link is keeping up. Under-shooting briefly costs some audio
/// quality; over-shooting costs pointer responsiveness, which is worse.
///
/// [`DEFAULT_BITRATE`]: cursio_audio::codec::DEFAULT_BITRATE
const BITRATE_START: u32 = 48_000;

impl AudioEngine {
    /// Emit a throughput summary at most once per [`HEALTH_LOG_INTERVAL`], and
    /// warn when audio from the peer has stopped arriving.
    ///
    /// Both live here so a quiet link is visible in the log: silence used to be
    /// indistinguishable from "nothing to report", which is exactly the case
    /// where you most want a line.
    fn log_health(&mut self) {
        let now = std::time::Instant::now();

        // Stall detection: audio was flowing, then stopped.
        //
        // Only while something here is actually listening. A stream nobody
        // plays is refused on purpose (see `unwanted_streams`), and the peer
        // then correctly sends nothing — so after that refusal silence is the
        // system working, not a fault. Warning about it would train the reader
        // to ignore the one line that means a real stall.
        let anyone_listening = self.playbacks.values().any(|p| p.is_listening());
        if let Some(last) = self.last_chunk_at.filter(|_| anyone_listening) {
            let gap = now.duration_since(last);
            if gap >= AUDIO_STALL_AFTER && !self.stall_reported {
                self.stall_reported = true;
                tracing::warn!(
                    silent_for_ms = gap.as_millis() as u64,
                    played = self.played,
                    sent = self.sent,
                    "audio from the peer stalled — no chunks arriving"
                );
            }
        }

        let due = self
            .last_health_log
            .is_none_or(|t| now.duration_since(t) >= HEALTH_LOG_INTERVAL);
        if !due {
            return;
        }
        self.last_health_log = Some(now);

        tracing::info!(
            sent = self.sent,
            played = self.played,
            silent_dropped = self.silent_dropped,
            unwanted_dropped = self.unwanted_dropped,
            over_budget = self.over_budget,
            stale_dropped = self.stale_dropped,
            streams = self.playbacks.len(),
            "audio health"
        );

        // The automatic threshold is invisible otherwise, and it is the first
        // thing to look at when a peer says this mic cuts out.
        let mode = self.mic_gate.get();
        for id in self.mic_captures.keys() {
            if let Some(gate) = self.gates.get(id) {
                tracing::info!(
                    id,
                    ?mode,
                    floor_dbfs = gate.floor_db(),
                    auto_threshold_dbfs = gate.auto_threshold(),
                    "mic gate"
                );
            }
        }
        if self.mic_gate_logged != Some(mode) {
            self.mic_gate_logged = Some(mode);
            tracing::info!(?mode, "microphones gated");
        }
    }

    /// Turn Intent-Sink membership into per-app Remote Streams.
    ///
    /// For each device Intent Sink, drain its Joined/Left events:
    /// - **Joined**: an app was funnelled into the sink → open a per-app tap and
    ///   advertise it tagged with the sink's peer device, so the peer renders it
    ///   into that exact device (not its default). This is the audio path; the
    ///   Intent Sink itself carries nothing.
    /// - **Left**: schedule teardown after a short grace, so a transient re-route
    ///   (unlink+relink) doesn't kill and rebuild the stream.
    ///
    /// Then reap any taps whose grace has expired.
    async fn pump_intent_sinks(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        // Collect events first to avoid holding a borrow on `intent_sinks` while we
        // mutate `routed_apps`/allocate ids below.
        let mut joined: Vec<(u32, String, (String, String))> = Vec::new();
        let mut left: Vec<u32> = Vec::new();
        for (sink_id, sink) in self.intent_sinks.iter_mut() {
            let Some(device) = self.intent_sink_device.get(sink_id).cloned() else {
                continue;
            };
            while let Ok(ev) = sink.events().try_recv() {
                match ev {
                    cursio_audio::IntentSinkEvent::Joined {
                        app_node_id,
                        app_name,
                    } => joined.push((app_node_id, app_name, device.clone())),
                    cursio_audio::IntentSinkEvent::Left { app_node_id } => left.push(app_node_id),
                }
            }
        }

        for (app_node_id, app_name, device) in joined {
            // Cancel any pending teardown — the app is (still) routed here.
            self.routed_app_expiry.remove(&app_node_id);
            if self.routed_apps.contains_key(&app_node_id) {
                continue; // already streaming this app
            }
            if let Some(pending) = self.opening_taps.get_mut(&app_node_id) {
                // Its tap is still being built on a worker. The node coming back
                // before it lands just cancels the abandonment.
                pending.abandoned = false;
                continue;
            }
            // Reuse the stream this app already owns, if it has one. The tap is
            // bound to a node id so it must be rebuilt, but the tunnel it feeds —
            // stream id, encoder, the peer's advertisement — is keyed to the app
            // and stays up. That is what makes a pause or a seek inaudible
            // instead of a teardown.
            //
            // Only when the app has NO live tap, though. One application can hold
            // several nodes at once (Firefox opens one per playing tab), and they
            // arrive as separate Joined events. Reusing the id for a second live
            // node pointed the tunnel at the newcomer and orphaned the tap that
            // was actually producing sound — heard as the browser going silent
            // until it happened to recreate its nodes in a luckier order.
            // A tap still opening counts as live too: two nodes of one app can
            // arrive in the same pump, and without this the second would reuse
            // the id the first is already claiming.
            let already_live = self
                .routed_app_names
                .iter()
                .any(|(node, name)| *name == app_name && *node != app_node_id)
                || self
                    .opening_taps
                    .iter()
                    .any(|(node, p)| p.app_name == app_name && *node != app_node_id);
            let resumed = if already_live {
                None
            } else {
                self.app_routes.get(&app_name).map(|(id, _)| *id)
            };
            let id = match resumed {
                Some(id) => id,
                None => {
                    let id = self.id_base + 500 + self.next_app_slot;
                    self.next_app_slot += 1;
                    id
                }
            };
            // Open the tap on a worker. This connects to the PipeWire daemon and
            // waits for the stream to come up, which the session loop cannot
            // afford to sit through — see [`AudioEngine::opening_taps`].
            let ready = self.tap_ready_tx.clone();
            let node_arg = app_node_id.to_string();
            let app_arg = app_name.clone();
            std::thread::Builder::new()
                .name(format!("cursio-tap-open-{id}"))
                .spawn(move || {
                    let result = cursio_audio::Capture::app_tap(id, node_arg, app_arg)
                        .map_err(|e| e.to_string());
                    // The receiver only closes when the session is ending, in
                    // which case the tap is moot.
                    let _ = ready.send(OpenedTap {
                        app_node_id,
                        result,
                    });
                })
                .map_err(|e| {
                    TransportError::Io(std::io::Error::other(format!("spawn tap opener: {e}")))
                })?;
            self.opening_taps.insert(
                app_node_id,
                PendingTap {
                    stream_id: id,
                    app_name: app_name.clone(),
                    device: device.clone(),
                    resumed: resumed.is_some(),
                    abandoned: false,
                },
            );
            // Claim the app's state now rather than when the tap lands. The node
            // is routed the moment it joins; leaving these until the open
            // finished would let the same node be picked up again by the next
            // pump, and a `Left` arriving meanwhile would find nothing to cancel.
            self.routed_app_names.insert(app_node_id, app_name.clone());
            self.routed_app_since
                .insert(app_node_id, std::time::Instant::now());
            // The name -> stream mapping is what a returning node looks itself up
            // by, so it must keep pointing at the app's FIRST stream. A second
            // concurrent node gets its own id and must not overwrite that, or the
            // original tunnel becomes unreachable and its audio stops being sent.
            self.app_routes
                .entry(app_name.clone())
                .or_insert_with(|| (id, device.clone()));
            // Back in the graph: it is not an idle tunnel any more, so the reaper
            // must not collect it.
            self.tunnel_idle_since.remove(&app_name);
            // Record the route for the replicated RoutingState snapshot, and mark
            // it dirty so `pump` re-broadcasts the whole map. This is what lets
            // routing survive a reconnect without waiting on PipeWire to re-emit
            // link events. Keyed by stream id, which outlives the node the app
            // happens to be using.
            self.routed_app_route.insert(
                id,
                cursio_protocol::RouteEntry {
                    id,
                    app_name: Some(app_name.clone()),
                    device: Some(device.clone()),
                },
            );
            self.routing_dirty = true;
            // If this device's Route already has a non-default group gain/mute,
            // mark it dirty so the new member inherits it (the fan-out in
            // `pump_routes` re-applies to all members).
            if self.route_groups.contains_key(&device) {
                self.routes_dirty.insert(device.clone());
            }
        }

        self.collect_opened_taps(tx).await?;

        // Schedule teardown for apps that left (grace period below).
        for app_node_id in left {
            if self.routed_apps.contains_key(&app_node_id) {
                self.routed_app_expiry
                    .insert(app_node_id, std::time::Instant::now() + ROUTED_APP_GRACE);
            }
        }

        // Release the taps of nodes that went away, but KEEP their tunnels.
        //
        // The node is gone, so its tap can only produce silence and is dropped.
        // The Remote Stream is not: the app is merely paused, mid-seek, or
        // reopening its audio client, and the route it belongs to is still what
        // the user asked for. Holding the stream id, the encoder and the peer's
        // playback end open means the audio simply resumes when the app comes
        // back — no close, no re-advertise, no rebuilt playback node.
        //
        // The tunnel is torn down later, by `reap_idle_tunnels`, once the app has
        // stayed away long enough to count as closed rather than paused.
        let now = std::time::Instant::now();
        let expired: Vec<u32> = self
            .routed_app_expiry
            .iter()
            .filter(|(_, &t)| now >= t)
            .map(|(&n, _)| n)
            .collect();
        // Tunnels of nodes an app dropped while keeping others: closed below,
        // once the borrow of `routed_app_names` above is done.
        let mut orphaned: Vec<(u32, Option<String>)> = Vec::new();
        for app_node_id in expired {
            self.routed_app_expiry.remove(&app_node_id);
            // A node can leave while its tap is still being opened. The worker
            // cannot be called back, so mark it: `collect_opened_taps` drops the
            // tap when it lands. The tunnel is left alone — the app may well be
            // pausing, exactly as below.
            if let Some(pending) = self.opening_taps.get_mut(&app_node_id) {
                pending.abandoned = true;
            }
            if let Some((stream_id, cap)) = self.routed_apps.remove(&app_node_id) {
                cap.stop(); // stops the capture thread; the tunnel stays up
                let app = self.routed_app_names.remove(&app_node_id);
                let age = self
                    .routed_app_since
                    .remove(&app_node_id)
                    .map(|since| now.duration_since(since))
                    .unwrap_or_default();
                let still_live = app
                    .as_ref()
                    .is_some_and(|app| self.routed_app_names.values().any(|n| n == app));
                let leftover = is_startup_leftover(still_live, age);

                if leftover {
                    orphaned.push((stream_id, app.clone()));
                } else if let Some(app) = &app {
                    // The tunnel outlives this node. Start the clock that tells
                    // a pause from a quit — but only while the app has no other
                    // node, or a tab still playing would have the reaper
                    // counting down against a tunnel that is in active use.
                    if !still_live {
                        self.tunnel_idle_since
                            .insert(app.clone(), std::time::Instant::now());
                    }
                }

                tracing::info!(
                    id = stream_id,
                    app_node_id,
                    app = app.as_deref().unwrap_or("?"),
                    age_ms = age.as_millis() as u64,
                    orphaned = leftover,
                    "app's node went away; tap released"
                );
            }
        }

        for (stream_id, app) in orphaned {
            // `app_routes` maps a name to ONE stream; only drop the entry if it
            // is this one, or the surviving node loses its route.
            if let Some(app) = &app {
                if self
                    .app_routes
                    .get(app)
                    .is_some_and(|(id, _)| *id == stream_id)
                {
                    self.app_routes.remove(app);
                }
            }
            self.close_app_stream(tx, stream_id, "a node this app dropped")
                .await?;
        }

        self.reap_idle_tunnels(tx).await?;
        Ok(())
    }

    /// Take delivery of taps the workers finished opening.
    ///
    /// The advertisement is sent from here rather than at request time because
    /// only a tap that actually came up should make the peer build a playback
    /// node for it.
    async fn collect_opened_taps(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        while let Ok(opened) = self.tap_ready_rx.try_recv() {
            let Some(pending) = self.opening_taps.remove(&opened.app_node_id) else {
                continue;
            };
            let mut cap = match opened.result {
                Ok(cap) => cap,
                Err(e) => {
                    tracing::warn!(
                        app = %pending.app_name,
                        error = %e,
                        "cannot tap app routed into Intent Sink"
                    );
                    // Undo the state claimed when the open was requested, or the
                    // node would look routed with nothing behind it.
                    self.routed_app_names.remove(&opened.app_node_id);
                    self.routed_app_since.remove(&opened.app_node_id);
                    continue;
                }
            };
            if pending.abandoned {
                tracing::debug!(
                    id = pending.stream_id,
                    app = %pending.app_name,
                    "app's node left before its tap was ready; dropping it"
                );
                continue;
            }
            // Advertise it as a per-app stream, but tag the peer DEVICE so the
            // peer renders it into that exact device rather than its default
            // output. Only for a NEW stream: re-advertising one the peer already
            // holds would make it tear down and rebuild the playback end, which
            // is the very churn this avoids.
            if pending.resumed {
                // Drain the advertisement the tap emits on open; the peer already
                // has this stream and needs no second copy.
                let _ = cap.advertisements().try_recv();
                tracing::info!(
                    id = pending.stream_id,
                    app = %pending.app_name,
                    device = %pending.device.1,
                    "app returned; re-pointed its tap, tunnel kept open"
                );
            } else if let Ok(mut meta) = cap.advertisements().try_recv() {
                meta.device = Some(pending.device.clone());
                tracing::info!(
                    id = pending.stream_id,
                    app = %pending.app_name,
                    device = %pending.device.1,
                    "app routed into Intent Sink; opening Remote Stream"
                );
                send_control(tx, AudioMsg::StreamAdvertise(meta)).await?;
            }
            self.routed_apps
                .insert(opened.app_node_id, (pending.stream_id, cap));
        }
        Ok(())
    }

    /// Tear down one per-app Remote Stream and tell the peer.
    ///
    /// Shared by the orphan sweep above and [`Self::reap_idle_tunnels`], which
    /// differ only in *why* the stream is finished, not in what has to be
    /// cleaned up.
    async fn close_app_stream(
        &mut self,
        tx: &mut dyn TransportTx,
        stream_id: u32,
        why: &str,
    ) -> Result<(), TransportError> {
        // Drop this stream's sender-side encoder and rate-control state, so a
        // later app starting on this id begins clean at the default bitrate
        // rather than inheriting a throttled encoder.
        self.encoders.remove(&stream_id);
        self.pending_packets.remove(&stream_id);
        self.gates.remove(&stream_id);
        self.unacked.remove(&stream_id);
        self.sent_since_adapt.remove(&stream_id);
        self.caught_up_streak.remove(&stream_id);
        self.acked_seq.remove(&stream_id);
        self.stream_bitrate.remove(&stream_id);
        self.last_adapt.remove(&stream_id);
        self.routed_app_route.remove(&stream_id);
        self.routing_dirty = true;
        tracing::info!(id = stream_id, why, "closing Remote Stream");
        send_control(
            tx,
            AudioMsg::StreamClose {
                id: stream_id,
                reason: cursio_protocol::AudioStreamCloseReason::ConsumerClosed,
            },
        )
        .await
    }

    /// Close tunnels whose app has been gone long enough to be closed, not paused.
    ///
    /// Keeping a tunnel open across a pause is the point (see the reaper above),
    /// but an app that was quit is never coming back, and its stream id, encoder
    /// and the peer's playback node would otherwise be held for the life of the
    /// session. [`TUNNEL_IDLE_TIMEOUT`] is the line between the two: long enough
    /// that no pause, seek or client restart crosses it, short enough that
    /// quitting an app frees its stream promptly.
    async fn reap_idle_tunnels(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let now = std::time::Instant::now();
        let dead: Vec<String> = self
            .tunnel_idle_since
            .iter()
            .filter(|(_, &since)| now.duration_since(since) >= TUNNEL_IDLE_TIMEOUT)
            .map(|(app, _)| app.clone())
            .collect();
        for app in dead {
            self.tunnel_idle_since.remove(&app);
            let Some((stream_id, _)) = self.app_routes.remove(&app) else {
                continue;
            };
            self.close_app_stream(tx, stream_id, "the app stayed away")
                .await?;
        }
        Ok(())
    }

    /// The peer offered a stream. Devices get a local mirror (so they show up
    /// in our output list) plus playback; apps get playback only — you don't
    /// route audio *into* someone else's application.
    fn on_advertise(&mut self, meta: &cursio_protocol::AudioStreamMeta) {
        // Recorded before any branch below can return early. Every advertised
        // stream owes the owner an answer about demand, including the ones this
        // machine turns out to be unable to play — those especially, since a
        // stream that never opens is the one whose silence the owner would
        // otherwise read as consent to keep sending.
        self.peer_streams.insert(meta.id);

        let peer_name = &self.peer_name;
        if let Some(app) = &meta.app_name {
            let label = format!("Cursio({peer_name}): {app}");
            // A per-app stream may carry a target device: the peer routed this app
            // into one of OUR devices (via its Intent Sink), so we must render it
            // into that exact device, not the default. The device handle is the one
            // WE advertised, i.e. a local device id/name.
            if let Some((dev, _desc)) = &meta.device {
                tracing::info!(id = meta.id, %label, device = %dev, "peer advertised an app stream routed to our device");
                self.inbound_app_device.insert(meta.id, dev.clone());
            } else {
                tracing::info!(id = meta.id, %label, "peer advertised an app stream");
            }
            // Open the playback NOW, on the advertisement, rather than waiting
            // for the stream's first chunk.
            //
            // Waiting made a mixer row appear only once an application actually
            // made a sound, which is too late to be useful: the row is where the
            // user sets that application's volume, and an application they have
            // not started playing yet is exactly when they want to set it. It
            // also made the set of rows look arbitrary — three applications
            // running, two rows, because the third had been quiet since the
            // session began. Measured here: a Java application's row appeared 90
            // seconds after it was advertised, when it finally played something.
            //
            // The cost is a node per advertised stream whether or not it ever
            // carries audio, which is what this used to avoid. That trade is now
            // the right way round: a silent row is a row the user can still
            // reach, while a missing row is a control they cannot find. No
            // filtering by name — a stream that looks like a service today
            // ("speech-dispatcher") is one that chimes tomorrow, and guessing
            // wrong hides a real application.
            self.pending_labels.insert(meta.id, label.clone());
            let target = self
                .inbound_app_device
                .get(&meta.id)
                .cloned()
                .or_else(|| self.render_device.clone());
            let id = meta.id;
            self.open_playback_off_loop(id, move || {
                cursio_audio::Playback::open_on(id, label, target)
            });
            return;
        }

        // A microphone the peer is offering. This is the mirror image of a
        // device: instead of a virtual speaker we publish a virtual *source*, so
        // local apps can select the peer's mic as a recording input. Opened
        // eagerly because it must appear in the input list before anything can
        // choose to record from it.
        if meta.role == cursio_protocol::StreamRole::Capture {
            let desc = meta
                .device
                .as_ref()
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| "Microphone".to_string());
            let label = format!("Cursio({peer_name}): {desc}");
            let id = meta.id;
            tracing::info!(id, %label, "peer advertised a microphone");
            // Published on a worker for the same reason as an Intent Sink: this
            // waits for a PipeWire node, and a peer offering several microphones
            // would otherwise stall the loop once per microphone.
            self.open_playback_off_loop(meta.id, move || {
                cursio_audio::Playback::virtual_source(id, label)
            });
            return;
        }

        // A device the peer offered. We create an INTENT SINK for it: a real local
        // output the user can route into, but which carries no audio itself. When
        // an app is funnelled into it we open a per-app Remote Stream tagged to
        // render into this peer device (see `pump_intent_sinks`). The old code made
        // a capturing `virtual_sink` here and shipped its whole mix — the source of
        // duplicate audio and lost per-app separation.
        let Some((dev_id, desc)) = &meta.device else {
            // No device handle ⇒ nothing to represent. In the unified model every
            // crossable device carries one; a deviceless playback advert is legacy
            // (the removed global stream) and is simply ignored.
            tracing::debug!(
                id = meta.id,
                "peer advertised a deviceless playback stream; ignoring"
            );
            return;
        };
        let node = format!("cursio_mirror_{}", sanitize_node(dev_id));
        let label = format!("Cursio({peer_name}): {desc}");
        tracing::info!(id = meta.id, %label, "peer advertised a device; creating Intent Sink");

        // Already have it, or already making it: a device is advertised again on
        // every reconnect, and creating a second node for it would leave the
        // user with duplicate outputs for one peer device.
        if self.intent_sinks.contains_key(&meta.id) || !self.opening_intent_sinks.insert(meta.id) {
            return;
        }

        // Create on a worker: this waits for a PipeWire node to come up (~430 ms
        // here), and the session loop cannot sit through that — see
        // [`AudioEngine::opening_intent_sinks`]. The finished sink is collected
        // by a later pump.
        let ready = self.intent_sink_ready_tx.clone();
        let id = meta.id;
        let device = (dev_id.clone(), desc.clone());
        let node_arg = node.clone();
        let label_arg = label.clone();
        let opener = self.intent_sink_opener;
        if let Err(e) = std::thread::Builder::new()
            .name(format!("cursio-pw-sink-open-{id}"))
            .spawn(move || {
                let result = opener(id, node_arg.clone(), label_arg);
                // The receiver only closes when the session is ending, in which
                // case the sink is moot.
                let _ = ready.send(OpenedIntentSink {
                    id,
                    node: node_arg,
                    device,
                    result,
                });
            })
        {
            self.opening_intent_sinks.remove(&id);
            tracing::error!(error = %e, %label, "cannot spawn the Intent Sink opener");
        }
    }

    /// Open a playback on a worker thread, to be collected by a later pump.
    ///
    /// Every kind of playback goes through here — a peer microphone published
    /// as a virtual source, an application stream opened on its advertisement,
    /// a device stream opened on its first chunk — because the cost being
    /// avoided is the same in all of them: bringing a PipeWire node up takes
    /// hundreds of milliseconds, and the session loop is single-threaded, so
    /// waiting inline stops input, audio and file traffic alike.
    fn open_playback_off_loop(
        &mut self,
        id: u32,
        open: impl FnOnce() -> Result<cursio_audio::Playback, cursio_audio::AudioError> + Send + 'static,
    ) {
        // One playback per stream. Chunks keep arriving at ~200/s while the
        // node comes up, and a stream is advertised again on every reconnect;
        // either would otherwise start another node for a stream that has one,
        // and the newcomer would replace — and so tear down — the row the user
        // had already set a volume on.
        if self.playbacks.contains_key(&id) || !self.opening_playbacks.insert(id) {
            return;
        }
        let ready = self.playback_ready_tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("cursio-pw-play-open-{id}"))
            .spawn(move || {
                let result = open();
                // The receiver only closes when the session is ending, in which
                // case the playback is moot.
                let _ = ready.send(OpenedPlayback { id, result });
            })
        {
            self.opening_playbacks.remove(&id);
            tracing::error!(error = %e, id, "cannot spawn the playback opener");
        }
    }

    /// Take delivery of playbacks whose worker has finished.
    fn collect_opened_playbacks(&mut self) {
        while let Ok(opened) = self.playback_ready_rx.try_recv() {
            // No longer wanted: the peer closed the stream while its node was
            // coming up. Keeping it would leave a mixer row for a stream that
            // will never be closed again, since its close has already come.
            if !self.opening_playbacks.remove(&opened.id) {
                continue;
            }
            match opened.result {
                Ok(p) => {
                    let name = self
                        .pending_labels
                        .get(&opened.id)
                        .cloned()
                        .or_else(|| self.inbound_targets.get(&opened.id).cloned())
                        .unwrap_or_else(|| format!("stream {}", opened.id));
                    self.mixer.add(opened.id, &self.peer_name, &name);
                    self.playbacks.insert(opened.id, p);
                    // Any gain that arrived while the node was coming up, so a
                    // knob turned during the open is not lost.
                    self.apply_gain(opened.id);
                }
                Err(e) => {
                    // Complain ONCE per stream: chunks arrive at ~200/s, and a
                    // stream that cannot open would otherwise log at that rate.
                    if !self.failed_playbacks.insert(opened.id) {
                        continue;
                    }
                    if matches!(e, cursio_audio::AudioError::Unsupported) {
                        let what = format!("stream {}", opened.id);
                        report_no_virtual_devices(&VIRTUAL_DEVICES_REPORTED, &what);
                    } else {
                        tracing::warn!(error = %e, id = opened.id,
                            "cannot open playback; ignoring this stream");
                    }
                }
            }
        }
    }

    /// Take delivery of Intent Sinks whose worker has finished.
    ///
    /// The bookkeeping lands here rather than at request time because a sink
    /// that failed to come up must leave no trace: a node name mapped to a sink
    /// that does not exist would silently swallow every device-state replica
    /// aimed at that peer device.
    fn collect_opened_intent_sinks(&mut self) {
        while let Ok(opened) = self.intent_sink_ready_rx.try_recv() {
            self.opening_intent_sinks.remove(&opened.id);
            let (dev_id, desc) = opened.device;
            match opened.result {
                Ok(sink) => {
                    self.intent_sinks.insert(opened.id, sink);
                    self.intent_sink_device
                        .insert(opened.id, (dev_id.clone(), desc));
                    // Remember the mapping both ways, so an inbound DeviceState replica
                    // reaches this Intent Sink and a local turn of the sink is
                    // attributed to the peer device it mirrors.
                    self.sink_node_for_peer_device
                        .insert(dev_id.clone(), opened.node.clone());
                    self.peer_device_for_sink_node.insert(opened.node, dev_id);
                }
                Err(cursio_audio::AudioError::Unsupported) => {
                    report_no_virtual_devices(&VIRTUAL_DEVICES_REPORTED, &desc);
                }
                Err(e) => {
                    tracing::error!(error = %e, device = %desc, "failed to create Intent Sink")
                }
            }
        }
    }

    /// The stream host (re)targeted one Remote Stream to a device. Update where we
    /// render it. Only the host may set this (invariant #4); we reflect it.
    ///
    /// `device` here is one of OUR local device handles (the host routed its app
    /// into our device via its Intent Sink), or `None` for our default output.
    fn on_stream_route(&mut self, entry: &cursio_protocol::RouteEntry) {
        self.apply_route(entry.id, entry.device.as_ref().map(|(d, _)| d.clone()));
    }

    /// The stream host replicated its whole routing map. Apply it idempotently:
    /// set each stream's target, and forget targets for streams no longer routed.
    ///
    /// This is the reconnect path — one object carries the full map, so we don't
    /// depend on the graph re-emitting per-link events.
    fn on_routing_state(&mut self, state: &cursio_protocol::RoutingState) {
        tracing::info!(
            count = state.routes.len(),
            "applying replicated routing state"
        );
        // The set of stream ids the host still routes; anything we had targeted
        // that isn't here has gone back to the default.
        let live: std::collections::HashSet<u32> = state.routes.iter().map(|r| r.id).collect();
        let stale: Vec<u32> = self
            .inbound_app_device
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in stale {
            self.apply_route(id, None);
        }
        for entry in &state.routes {
            self.apply_route(entry.id, entry.device.as_ref().map(|(d, _)| d.clone()));
        }
    }

    /// A peer requested a group operation on a Route we host (invariant #4: we own
    /// the member streams, so we apply it). Update the group's gain/mute and mark
    /// it dirty; `pump` fans the effective gain out to each member and replicates
    /// the [`RouteState`].
    ///
    /// A Route is identified by its target device; members are the Remote Streams
    /// funnelled into that device. Group gain/mute is a Route property, never the
    /// device's replicated volume (invariant #2).
    fn on_route_op(&mut self, target_device: (String, String), op: cursio_protocol::RouteOp) {
        let group = self
            .route_groups
            .entry(target_device.clone())
            .or_insert(RouteGroup {
                gain: 1.0,
                mute: false,
            });
        match op {
            cursio_protocol::RouteOp::SetGain(g) => {
                group.gain = g.clamp(0.0, 4.0);
                tracing::info!(device = %target_device.1, gain = group.gain, "route group gain set");
            }
            cursio_protocol::RouteOp::SetMute(m) => {
                group.mute = m;
                tracing::info!(device = %target_device.1, mute = m, "route group mute set");
            }
        }
        self.routes_dirty.insert(target_device);
    }

    /// Report which inbound streams have a local consumer, one message per
    /// change.
    ///
    /// Only transitions are sent: the owner holds the last value, so repeating
    /// it every pump (~100/s per stream) would flood the shared channel that
    /// input rides on to say nothing new.
    async fn pump_demand(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let changed: Vec<(u32, bool)> = self
            .peer_streams
            .iter()
            .filter_map(|&id| {
                // Three states, not two, and conflating the last two is fatal.
                //
                // - a playback that is open answers for itself;
                // - one that FAILED to open will never answer, so refuse the
                //   stream: this is the Windows microphone case, where nothing
                //   can publish it and the owner would otherwise stream to a
                //   machine that can never play it;
                // - one that has not opened *yet* must say nothing at all,
                //   because some playbacks open only on the stream's FIRST
                //   CHUNK. Refuse one of those and the owner stops sending, so
                //   the first chunk never arrives, so the playback never opens —
                //   the refusal makes itself permanently true.
                //
                // Silence is the right answer for the third: the owner keeps
                // sending, the playback opens, and the pump after that reports
                // the truth.
                let listening = match self.playbacks.get(&id) {
                    Some(p) => p.is_listening(),
                    None if self.failed_playbacks.contains(&id) => false,
                    None => return None,
                };
                (self.demand_sent.get(&id) != Some(&listening)).then_some((id, listening))
            })
            .collect();
        for (id, listening) in changed {
            self.demand_sent.insert(id, listening);
            tracing::info!(id, listening, "reporting stream demand to the owner");
            send_control(tx, AudioMsg::StreamDemand { id, listening }).await?;
        }
        Ok(())
    }

    /// Fan out any changed Route's group gain/mute to its members, and replicate
    /// its [`RouteState`]. Called from `pump` (it needs `tx`).
    ///
    /// For each dirty Route: gather the member streams (Remote Streams whose target
    /// is this device), send each an [`Message::AudioGain`] carrying the group's
    /// effective gain, then send one [`Message::AudioRouteState`] for the group.
    async fn pump_routes(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if self.routes_dirty.is_empty() {
            return Ok(());
        }
        let dirty: Vec<(String, String)> = self.routes_dirty.drain().collect();
        for device in dirty {
            let group = self
                .route_groups
                .get(&device)
                .copied()
                .unwrap_or(RouteGroup {
                    gain: 1.0,
                    mute: false,
                });
            let effective = group.effective_gain();
            // Members: Remote Streams currently targeting this device.
            let members: Vec<u32> = self
                .routed_app_route
                .values()
                .filter(|r| r.device.as_ref() == Some(&device))
                .map(|r| r.id)
                .collect();
            for &id in &members {
                send_control(
                    tx,
                    AudioMsg::Gain {
                        id,
                        gain: effective,
                    },
                )
                .await?;
            }
            tracing::info!(device = %device.1, members = members.len(), gain = group.gain, mute = group.mute, "applied route group op to members");
            send_control(
                tx,
                AudioMsg::RouteState(cursio_protocol::RouteState {
                    target_device: device,
                    members,
                    gain: group.gain,
                    mute: group.mute,
                }),
            )
            .await?;
        }
        Ok(())
    }

    /// Set a stream's target device and, if its playback is already open on a
    /// different device, drop it so the next chunk reopens it on the new target.
    ///
    /// Shared by [`on_stream_route`] and [`on_routing_state`]. `device = None`
    /// means "render into our default output".
    fn apply_route(&mut self, id: u32, device: Option<String>) {
        let previous = self.inbound_app_device.get(&id).cloned();
        if previous == device {
            return; // idempotent: nothing changed
        }
        match &device {
            Some(dev) => {
                self.inbound_app_device.insert(id, dev.clone());
            }
            None => {
                self.inbound_app_device.remove(&id);
            }
        }
        // If we're already rendering this stream, its target moved — reopen it on
        // the next chunk so it lands on the right device.
        if self.playbacks.remove(&id).is_some() {
            tracing::info!(
                id,
                ?device,
                "stream re-routed; playback will reopen on the new device"
            );
        }
    }

    /// Turn device-monitor changes into wire traffic (see docs/audio-design.md,
    /// "All changes go THROUGH the owner").
    ///
    /// Two cases per observed change, decided by which node changed:
    /// - a **real device we own** changed (the user turned its knob locally, or we
    ///   just applied a peer's request) → broadcast its authoritative
    ///   [`DeviceState`] so every node mirrors it.
    /// - **one of our Intent Sinks** changed (the user turned the mirror's slider,
    ///   e.g. a media key while it is the default sink) → this is a *request* to
    ///   the owner, not a local write (invariant #4) → send `AudioDeviceStateSet`.
    ///   Unless it matches the replica we just applied — then it is our own echo,
    ///   swallowed so state doesn't ping-pong.
    async fn pump_device_state(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        // Drain changes synchronously first, so we don't hold a borrow on the
        // monitor across the awaits below.
        let mut changes = Vec::new();
        if let Some(monitor) = &mut self.device_monitor {
            while let Ok(c) = monitor.changes().try_recv() {
                changes.push(c);
            }
        }

        for c in changes {
            let state = cursio_protocol::DeviceState {
                volume: c.volume,
                mute: c.mute,
            };
            if self.owned_device_nodes.contains(&c.node_name) {
                // Owner side: broadcast, but only on a real change.
                if self.owned_device_state.get(&c.node_name) == Some(&state) {
                    continue;
                }
                self.owned_device_state.insert(c.node_name.clone(), state);
                tracing::info!(device = %c.node_name, volume = c.volume, mute = c.mute, "broadcasting owned device state");
                send_control(
                    tx,
                    AudioMsg::DeviceState {
                        device: c.node_name,
                        state,
                    },
                )
                .await?;
            } else if let Some(peer_dev) = self.peer_device_for_sink_node.get(&c.node_name).cloned()
            {
                // Non-owner side: a turn of the Intent Sink is a request — unless
                // it is the echo of a replica we just applied.
                if self.replica_echo.get(&c.node_name) == Some(&state) {
                    self.replica_echo.remove(&c.node_name);
                    continue;
                }
                tracing::info!(device = %peer_dev, volume = c.volume, mute = c.mute, "Intent Sink turned locally; requesting owner");
                send_control(
                    tx,
                    AudioMsg::DeviceStateSet {
                        device: peer_dev.clone(),
                        field: cursio_protocol::DeviceStateField::Volume(c.volume),
                    },
                )
                .await?;
                send_control(
                    tx,
                    AudioMsg::DeviceStateSet {
                        device: peer_dev,
                        field: cursio_protocol::DeviceStateField::Mute(c.mute),
                    },
                )
                .await?;
            }
            // else: some other node (a Cursio playback/mic node); not a device.
        }
        Ok(())
    }

    /// The peer set a logical gain for one of the streams we play.
    ///
    /// This is the per-stream gain of the "one system" model — a property of the
    /// Stream/Route, applied *above* the OS mixer and separate from a device's
    /// single replicated volume (see docs/audio-design.md, "Separation of
    /// responsibilities"). It attenuates the peer's audio *as we play it*, which
    /// is exactly "make only the remote audio quieter".
    ///
    /// Remembered in `gains` so it also applies to a playback that hasn't opened
    /// yet (gain can arrive before the first chunk).
    fn on_gain(&mut self, id: u32, gain: f32) {
        tracing::info!(id, gain, "peer set stream gain");
        self.gains.insert(id, gain);
        self.apply_gain(id);
    }

    /// Set a playback's gain from both knobs that govern it: the owner's
    /// (`AudioGain`, kept in `gains`) and this machine's listener's (the
    /// [`LocalMixer`]). Multiplied, so neither overrides the other.
    fn apply_gain(&self, id: u32) {
        let Some(p) = self.playbacks.get(&id) else {
            return;
        };
        let owner = self.gains.get(&id).copied().unwrap_or(1.0);
        p.set_gain(owner * self.mixer.level(id));
    }

    /// The peer acked the newest chunk it has played for one of our streams.
    ///
    /// Feedback for adaptive rate control: the sent-vs-acked gap tells us the
    /// link is falling behind. The rule is the design's — **degrade rather than
    /// buffer unboundedly** — so a sustained backlog drops the stream's Opus
    /// bitrate; a recovered link lets it climb back. (Applied on the encoder in
    /// `adapt_bitrate`; here we only retire what the ack covers.)
    fn on_ack(&mut self, ack: &cursio_protocol::AudioFrameAck) {
        self.acked_seq.insert(ack.id, ack.acked_seq);
        if let Some(unacked) = self.unacked.get_mut(&ack.id) {
            while unacked.front().is_some_and(|&seq| seq <= ack.acked_seq) {
                unacked.pop_front();
            }
        }
    }

    /// A peer **requested** a change to a device we OWN (invariant #3: only the
    /// owner mutates `DeviceState`). Apply it to the real device via the monitor;
    /// the resulting monitor change is broadcast to everyone in `pump`, so the
    /// requester learns the applied value the same way every other node does.
    ///
    /// We deliberately do NOT broadcast here from the request itself — the real
    /// device is the source of truth, and echoing the *request* value could
    /// diverge from what the hardware actually accepted (a device may quantise or
    /// clamp). Applying and letting the observed change propagate keeps one path.
    fn on_device_state_set(&mut self, device: &str, field: cursio_protocol::DeviceStateField) {
        if !self.owned_device_nodes.contains(device) {
            // Not ours to mutate. Could be a device that has since disappeared, or
            // a misdirected request; ignore rather than guess.
            tracing::debug!(
                device,
                "device-state set for a device we don't own; ignoring"
            );
            return;
        }
        let Some(monitor) = &self.device_monitor else {
            return;
        };
        match field {
            cursio_protocol::DeviceStateField::Volume(v) => {
                tracing::info!(
                    device,
                    volume = v,
                    "applying peer's volume request to owned device"
                );
                monitor.set_volume(device.to_string(), v);
            }
            cursio_protocol::DeviceStateField::Mute(m) => {
                tracing::info!(
                    device,
                    mute = m,
                    "applying peer's mute request to owned device"
                );
                monitor.set_mute(device.to_string(), m);
            }
        }
    }

    /// The owner broadcast a device's authoritative state. We hold an Intent Sink
    /// mirroring that device, so apply the state to the sink node — that is what
    /// makes helvum/pavucontrol show the SAME slider value as the owner (the
    /// single-machine illusion). Record it as `replica_echo` so the monitor change
    /// this provokes is not bounced back to the owner as a fresh request.
    fn on_device_state_replica(&mut self, device: &str, state: cursio_protocol::DeviceState) {
        let Some(node) = self.sink_node_for_peer_device.get(device).cloned() else {
            // We don't mirror this device (no Intent Sink for it). Nothing to do.
            tracing::debug!(
                device,
                "device-state replica for a device we don't mirror; ignoring"
            );
            return;
        };
        let Some(monitor) = &self.device_monitor else {
            return;
        };
        tracing::info!(device, %node, volume = state.volume, mute = state.mute, "reflecting owner's device state on Intent Sink");
        self.replica_echo.insert(node.clone(), state);
        monitor.set_volume(node.clone(), state.volume);
        monitor.set_mute(node, state.mute);
    }

    /// Return a sparse [`AudioFrameAck`] for each inbound stream that has moved
    /// since we last acked it, at most once per [`ACK_INTERVAL`].
    ///
    /// This is the receiver half of rate control: the sender compares these acks
    /// against what it has sent to gauge how far behind the link is. Sent from
    /// `pump` (not `on_chunk`) because that is where `tx` lives and where the
    /// cadence is naturally throttled.
    async fn send_acks(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let now = std::time::Instant::now();
        // Collect first to avoid borrowing `recv_seq` across the await.
        let due: Vec<(u32, u32)> = self
            .recv_seq
            .iter()
            .filter(|(id, _)| {
                self.last_ack_sent
                    .get(id)
                    .is_none_or(|t| now.duration_since(*t) >= ACK_INTERVAL)
            })
            .map(|(&id, &seq)| (id, seq))
            .collect();
        for (id, acked_seq) in due {
            self.last_ack_sent.insert(id, now);
            send_control(
                tx,
                AudioMsg::FrameAck(cursio_protocol::AudioFrameAck { id, acked_seq }),
            )
            .await?;
        }
        Ok(())
    }

    /// Re-evaluate each outbound stream's Opus bitrate from its sent-vs-acked gap.
    ///
    /// The design's rule is **degrade rather than buffer unboundedly**: a
    /// sustained backlog (chunks sent but unacknowledged, over [`BACKLOG_HIGH`])
    /// steps the bitrate down; a link that has stayed caught up (under
    /// [`BACKLOG_LOW`]) for [`BITRATE_RAISE_AFTER`] evaluations steps it back up
    /// toward the default. A change is applied to the running encoder, never by
    /// replacing it: the peer's decoder carries the old encoder's state, and a
    /// fresh encoder's first packets decode against that state as a loud
    /// burst. Runs at most once per [`ADAPT_INTERVAL`] per stream, and only for
    /// a stream that sent something since the last look.
    fn adapt_bitrate(&mut self) {
        let now = std::time::Instant::now();
        // Only streams we're actively sending have an `unacked` entry.
        let ids: Vec<u32> = self.unacked.keys().copied().collect();
        for id in ids {
            let due = self
                .last_adapt
                .get(&id)
                .is_none_or(|t| now.duration_since(*t) >= ADAPT_INTERVAL);
            if !due || !self.sent_since_adapt.contains(&id) {
                continue;
            }
            // No ack yet ⇒ assume the link is fine and leave the rate alone; a
            // missing ack is not evidence of congestion.
            if !self.acked_seq.contains_key(&id) {
                continue;
            }
            self.last_adapt.insert(id, now);
            self.sent_since_adapt.remove(&id);
            let gap = self.unacked.get(&id).map_or(0, |q| q.len()) as u32;

            let current = self
                .stream_bitrate
                .get(&id)
                .copied()
                .unwrap_or(BITRATE_START);
            let streak = self.caught_up_streak.entry(id).or_insert(0);
            *streak = if gap < BACKLOG_LOW { *streak + 1 } else { 0 };
            let mut target = next_bitrate(current, gap);
            if target > current {
                if *streak < BITRATE_RAISE_AFTER {
                    target = current;
                } else {
                    *streak = 0;
                }
            }

            if target != current {
                // No encoder yet means nothing to change: `pump` builds the
                // first one at whatever `stream_bitrate` holds.
                if let Some(encoder) = self.encoders.get_mut(&id) {
                    if let Err(e) = encoder.set_bitrate(target) {
                        tracing::warn!(error = %e, id, from = current, to = target,
                            "cannot change the audio bitrate; keeping the current one");
                        continue;
                    }
                }
                tracing::info!(
                    id,
                    gap,
                    from = current,
                    to = target,
                    "adapting audio bitrate"
                );
                self.stream_bitrate.insert(id, target);
            }
        }
    }

    /// Audio for one of our streams: render it into the matching real device.
    fn on_chunk(&mut self, chunk: &cursio_protocol::AudioChunk) {
        let id = chunk.id;

        // Audio is flowing again: note the arrival, and if we had warned about a
        // stall, say that it recovered — a warning with no matching "resumed" is
        // impossible to interpret later.
        let now = std::time::Instant::now();
        if self.stall_reported {
            let gap = self.last_chunk_at.map(|t| now.duration_since(t));
            tracing::info!(
                stalled_for_ms = gap.map(|g| g.as_millis() as u64),
                "audio from the peer resumed"
            );
            self.stall_reported = false;
        }
        self.last_chunk_at = Some(now);

        // Track the newest seq we've received for this stream; `pump` turns this
        // into a sparse AudioFrameAck (a few per second) for the sender's rate
        // control. Chunks can arrive slightly out of order, so keep the max.
        // A gap here means chunks were lost or delayed on the WIRE, as opposed to
        // the receiver draining its buffer faster than they arrive. The two sound
        // identical (audio cutting in and out) but have opposite fixes, so log the
        // gap: matched against the sink's underruns it says which end is at fault.
        if let Some(&prev) = self.recv_seq.get(&id) {
            if chunk.seq > prev + 1 {
                tracing::debug!(
                    id,
                    missing = chunk.seq - prev - 1,
                    prev_seq = prev,
                    seq = chunk.seq,
                    "gap in the inbound chunk sequence"
                );
            }
        }
        self.recv_seq
            .entry(id)
            .and_modify(|s| *s = (*s).max(chunk.seq))
            .or_insert(chunk.seq);

        // Already coming up on a worker: drop this chunk and wait.
        //
        // Checked BEFORE the label is taken below, because taking it is
        // destructive. Bringing a node up takes hundreds of milliseconds and
        // chunks arrive at ~200/s, so without this the second chunk of a stream
        // finds `pending_labels` already emptied by the first, falls through to
        // the "never advertised" arm, and the stream loses the name it was
        // advertised under. Every app then rendered under whichever name did
        // survive — one mixer row on the peer, carrying every application's
        // audio at once.
        if self.opening_playbacks.contains(&id) {
            return;
        }

        if !self.playbacks.contains_key(&id) {
            // No playback for this stream yet. A stream rendering into one of
            // OUR devices opens here, on its first chunk; an application stream
            // was already opened on its advertisement and arrives here only if
            // that open failed, which makes this its retry.
            let peer_name = &self.peer_name;
            if let Some(dev) = self.inbound_targets.get(&id).cloned() {
                // A device WE advertised: render into that exact local device.
                let label = format!("Cursio({peer_name}): {dev}");
                self.open_playback_off_loop(id, move || {
                    cursio_audio::Playback::open_on(id, label, Some(dev))
                });
            } else if let Some(label) = self.pending_labels.get(&id).cloned() {
                // A stream the PEER advertised (its device or app). If it was
                // tagged with one of OUR devices (the peer routed this app into our
                // device via its Intent Sink), render into that device; otherwise
                // into our default output.
                //
                // Read, not taken: the label identifies the stream for as long as
                // the peer offers it, and a failed open must be able to try again
                // under the right name. It is dropped with the rest of the
                // stream's state on `StreamClose`.
                let target = self
                    .inbound_app_device
                    .get(&id)
                    .cloned()
                    .or_else(|| self.render_device.clone());
                tracing::info!(id, %label, ?target, "first audio; opening playback");
                self.open_playback_off_loop(id, move || {
                    cursio_audio::Playback::open_on(id, label, target)
                });
            } else if self.failed_playbacks.insert(id) {
                tracing::warn!(id, "audio for a stream that was never advertised; ignoring");
            }
            // The node is not up yet, so this chunk has nowhere to go. Dropping
            // it is the right answer and not a loss: playback opens on the first
            // chunk of a stream that has only just started, the receiver fills a
            // gap with silence anyway, and the alternative — holding chunks
            // until the node arrives — is the buffering that made audio latency
            // grow without bound.
            return;
        }
        // Decompress before playback. One decoder per stream, mirroring the
        // encoders — Opus carries state between frames.
        let decoder = match self.decoders.entry(id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                match cursio_audio::codec::OpusDecoder::new() {
                    Ok(d) => e.insert(d),
                    Err(err) => {
                        tracing::warn!(error = %err, id, "no decoder for stream");
                        return;
                    }
                }
            }
        };
        let pcm = match decoder.decode(&chunk.data) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(error = %err, id, "opus decode failed");
                return;
            }
        };
        let chunk = &cursio_protocol::AudioChunk {
            id,
            seq: chunk.seq,
            timestamp: chunk.timestamp,
            data: pcm,
        };

        if let Some(p) = self.playbacks.get(&id) {
            if let Err(e) = p.play(chunk) {
                tracing::warn!(error = %e, "playback failed");
            } else {
                self.played += 1;
            }
        }
    }
}

/// Most chunks to send in one [`SessionModule::pump`] call, across **all**
/// streams together.
///
/// This used to be a per-*stream* limit of 4, which stops limiting anything as
/// streams multiply: mirroring a Windows box with 14 audio endpoints meant up to
/// ~80 sends between two polls of the input channel. Since input shares this
/// one encrypted channel, that was felt directly as mouse lag — the session loop
/// cannot reach its `select!` while a module is pumping.
///
/// A whole-pump budget keeps the loop responsive no matter how many devices are
/// mirrored, so no functionality has to be given up to stay fast.
/// Anything beyond this waits for the next pump — it is *not* discarded (see
/// the pump body). Sized to carry a handful of genuinely active streams at
/// their natural rate while still returning to the session loop promptly.
const MAX_CHUNKS_PER_PUMP: usize = 32;

/// How many queued chunks a stream may hold before the excess counts as stale.
///
/// Chosen to sit just above what one pump can send ([`MAX_CHUNKS_PER_PUMP`]), so
/// a stream running at its natural rate is never trimmed — only one that has
/// genuinely fallen behind. At ~256-frame chunks this is roughly 250 ms of
/// audio, the same order as the playback jitter ceiling.
const MAX_CHUNK_BACKLOG: usize = 48;

/// Pop one queued packet only when the current pump can send it.
///
/// Checking the budget before `pop_front` is important: the capture channel
/// cannot put an item back, so taking first and discovering a zero budget later
/// silently lost audio at every boundary.
fn take_with_budget<T>(queue: &mut VecDeque<T>, budget: &mut usize) -> Option<T> {
    if *budget == 0 {
        return None;
    }
    let item = queue.pop_front()?;
    *budget -= 1;
    Some(item)
}

/// Drop all but the newest `keep` chunks waiting in `rx`, returning how many
/// were discarded.
///
/// An unbounded capture channel turns a *rate* mismatch into unbounded *latency*
/// exactly like the playback queue did: whatever the pump cannot send simply
/// accumulates, and every later chunk is sent that much later. Trimming from the
/// front bounds the age of what we do send.
///
/// Note this drains and re-queues nothing — `try_recv` already removes from the
/// front, so discarding the front *is* keeping the tail.
fn drain_stale(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::AudioChunk>,
    keep: usize,
) -> usize {
    let backlog = rx.len();
    if backlog <= keep {
        return 0;
    }
    let excess = backlog - keep;
    let mut dropped = 0;
    for _ in 0..excess {
        if rx.try_recv().is_err() {
            break;
        }
        dropped += 1;
    }
    dropped
}

/// Decide a stream's next Opus bitrate from its current rate and its sent-vs-
/// acked backlog (in chunks).
///
/// Hysteretic degrade-not-buffer: a backlog over [`BACKLOG_HIGH`] steps down
/// toward [`BITRATE_MIN`]; a link caught up under [`BACKLOG_LOW`] steps back up
/// toward the codec default; in between it holds. Pulled out of
/// [`AudioEngine::adapt_bitrate`] so the thresholds are unit-testable without a
/// live session.
fn next_bitrate(current: u32, gap: u32) -> u32 {
    if gap > BACKLOG_HIGH {
        current.saturating_sub(BITRATE_STEP).max(BITRATE_MIN)
    } else if gap < BACKLOG_LOW {
        (current + BITRATE_STEP).min(cursio_audio::codec::DEFAULT_BITRATE)
    } else {
        current
    }
}

/// Width of one node's stream-id range. A node allocates devices at
/// `base+1..`, per-app streams at `base+500..`, so the range must comfortably
/// exceed both. 1000 matches the original hand-assigned layout.
/// Where microphone stream ids start inside a node's range.
///
/// Devices take `base+1..`, per-app streams `base+500..`; microphones sit at
/// `base+800..` so the three kinds never collide inside one [`ID_RANGE`].
const MIC_ID_OFFSET: u32 = 800;

const ID_RANGE: u32 = 1_000;
/// How many distinct ranges exist before two nodes must share one.
///
/// Sized so a realistic mesh (a handful of machines) practically never
/// collides, while ids stay small enough to read in a log.
const ID_SLOTS: u32 = 1_000;

/// Pick the stream-id range for a node, from its stable [`PeerId`].
///
/// Was `Initiator => 1000 / Responder => 2000`, which only works for exactly
/// two machines: a third node is necessarily *some* link's initiator, so it
/// would reuse 1000 and its streams would collide with the node already there.
///
/// Hashing the id instead gives every node its own range regardless of who
/// dialled whom, and it's stable across reconnects — the same machine keeps the
/// same range, so a peer's stream ids don't churn when a link is re-established.
fn id_base_for(node: PeerId) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    node.hash(&mut h);
    // +1 so the range never starts at 0: id 0 is easy to confuse with "unset".
    (h.finish() as u32 % ID_SLOTS + 1) * ID_RANGE
}

/// Make a device id safe to use as a PipeWire node name (letters, digits, `_`).
fn sanitize_node(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_lowercase()
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
        Ok(Box::new(AudioEngine::new(
            ctx.local.peer_id,
            self.render_device.get(),
            self.mic_gate.clone(),
            self.mixer.clone(),
            ctx.peer_name().to_string(),
        )))
    }
}

/// Version of this module's wire vocabulary ([`AudioMsg`]). Bump when those
/// types change shape; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport that keeps what was sent, so a test can assert on the wire
    /// traffic rather than on the engine's internals.
    #[derive(Default)]
    struct RecordingTx {
        sent: Vec<Message>,
    }

    impl RecordingTx {
        /// The `(id, listening)` pairs of every demand report sent, in order.
        fn demands(&self) -> Vec<(u32, bool)> {
            self.sent
                .iter()
                .filter_map(|m| match m.decode_module::<AudioMsg>(ModuleId::AUDIO) {
                    Ok(Some(AudioMsg::StreamDemand { id, listening })) => Some((id, listening)),
                    _ => None,
                })
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl TransportTx for RecordingTx {
        async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
            self.sent.push(msg.clone());
            Ok(())
        }

        fn queue_depth(
            &self,
            _priority: cursio_protocol::Priority,
        ) -> cursio_transport::QueueDepth {
            cursio_transport::QueueDepth::default()
        }
    }

    /// Two nodes must land in different ranges, or their stream ids overlap and
    /// audio gets routed to the wrong device. (Not guaranteed for *every*
    /// possible pair — it's a hash — but it must hold for ordinary distinct ids.)
    #[test]
    fn distinct_nodes_get_distinct_ranges() {
        let a = id_base_for(PeerId::new_v4());
        let b = id_base_for(PeerId::new_v4());
        assert_ne!(a, b, "two random nodes collided in the same id range");
    }

    /// Streams must not open at the codec ceiling: rate control cannot react for
    /// the first ~750 ms, so every stream starting at maximum is what floods the
    /// shared channel — felt as mouse lag — right after connecting.
    #[test]
    fn streams_start_below_the_ceiling() {
        const {
            assert!(
                BITRATE_START < cursio_audio::codec::DEFAULT_BITRATE,
                "opening bitrate must leave headroom for input"
            );
            assert!(
                BITRATE_START >= BITRATE_MIN,
                "opening bitrate should not start below the adaptation floor"
            );
        }
    }

    /// A healthy link must still reach full quality — starting low is a ramp, not
    /// a permanent cap.
    #[test]
    fn a_quiet_link_climbs_back_to_full_quality() {
        let mut rate = BITRATE_START;
        for _ in 0..10 {
            rate = next_bitrate(rate, 0);
        }
        assert_eq!(
            rate,
            cursio_audio::codec::DEFAULT_BITRATE,
            "a caught-up link should recover the codec default"
        );
    }

    /// Build a receiver holding `n` chunks, seq 0..n, for the backlog tests.
    fn queued(n: u32) -> tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::AudioChunk> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for seq in 0..n {
            tx.send(cursio_protocol::AudioChunk {
                id: 1,
                seq,
                timestamp: seq as u64,
                data: vec![0u8; 4],
            })
            .unwrap();
        }
        rx
    }

    /// A stream keeping up must never be trimmed — trimming healthy audio would
    /// be heard as dropouts.
    #[test]
    fn backlog_within_budget_is_left_alone() {
        let mut rx = queued(MAX_CHUNK_BACKLOG as u32);
        assert_eq!(drain_stale(&mut rx, MAX_CHUNK_BACKLOG), 0);
        assert_eq!(
            rx.len(),
            MAX_CHUNK_BACKLOG,
            "nothing should have been dropped"
        );
    }

    /// A stream that fell behind keeps only the NEWEST chunks: the stale head is
    /// what costs link capacity (and therefore mouse latency) for audio too old
    /// to be wanted.
    #[test]
    fn stale_backlog_drops_oldest_and_keeps_newest() {
        let total = MAX_CHUNK_BACKLOG as u32 + 20;
        let mut rx = queued(total);

        let dropped = drain_stale(&mut rx, MAX_CHUNK_BACKLOG);
        assert_eq!(dropped, 20, "should discard exactly the excess");
        assert_eq!(rx.len(), MAX_CHUNK_BACKLOG);

        // The survivor at the front must be the 21st chunk — everything older is
        // gone, and the newest is still queued.
        let front = rx.try_recv().expect("a chunk should remain");
        assert_eq!(front.seq, 20, "kept the wrong end of the queue");
    }

    #[test]
    fn exhausted_send_budget_keeps_encoded_audio_queued() {
        let mut packets = VecDeque::from([1, 2, 3]);
        let mut budget = 0;

        assert_eq!(take_with_budget(&mut packets, &mut budget), None);
        assert_eq!(packets, VecDeque::from([1, 2, 3]));

        budget = 2;
        assert_eq!(take_with_budget(&mut packets, &mut budget), Some(1));
        assert_eq!(take_with_budget(&mut packets, &mut budget), Some(2));
        assert_eq!(take_with_budget(&mut packets, &mut budget), None);
        assert_eq!(packets, VecDeque::from([3]));
    }

    /// The same machine must keep its range across reconnects, otherwise a
    /// peer's streams would be renumbered every time a link is re-established.
    #[test]
    fn range_is_stable_for_one_node() {
        let id = PeerId::new_v4();
        assert_eq!(id_base_for(id), id_base_for(id));
    }

    /// Ranges must not start at 0 (easily confused with "unset") and must leave
    /// room for the +500 per-app sub-range below the next node's base.
    #[test]
    fn range_layout_leaves_room_for_app_ids() {
        for _ in 0..100 {
            let base = id_base_for(PeerId::new_v4());
            assert!(base >= ID_RANGE, "range started at or below zero: {base}");
            assert_eq!(base % ID_RANGE, 0, "ranges should be aligned: {base}");
        }
    }

    /// A sustained backlog must lower the bitrate; a caught-up link must raise it;
    /// a mid-range gap must hold. This is the degrade-not-buffer rule in numbers.
    #[test]
    fn bitrate_adapts_with_backlog() {
        let def = cursio_audio::codec::DEFAULT_BITRATE;

        // Congested: step down, but never below the floor.
        let lower = next_bitrate(def, BACKLOG_HIGH + 1);
        assert!(lower < def, "a big backlog must reduce the bitrate");
        assert!(lower >= BITRATE_MIN, "must not drop below the floor");

        // Caught up: climb back, but never above the default ceiling.
        let higher = next_bitrate(lower, 0);
        assert!(higher > lower, "a cleared backlog must raise the bitrate");
        assert!(higher <= def, "must not exceed the default ceiling");

        // In the hysteresis band: hold, so it doesn't chatter on jitter.
        let held = next_bitrate(def, (BACKLOG_LOW + BACKLOG_HIGH) / 2);
        assert_eq!(
            held, def,
            "a mid-range gap must leave the bitrate unchanged"
        );
    }

    /// The floor and ceiling must be hard stops even after repeated steps.
    #[test]
    fn bitrate_saturates_at_floor_and_ceiling() {
        let def = cursio_audio::codec::DEFAULT_BITRATE;
        let mut b = def;
        for _ in 0..20 {
            b = next_bitrate(b, BACKLOG_HIGH + 1);
        }
        assert_eq!(
            b, BITRATE_MIN,
            "repeated congestion must settle exactly at the floor"
        );
        for _ in 0..20 {
            b = next_bitrate(b, 0);
        }
        assert_eq!(
            b, def,
            "repeated recovery must settle exactly at the default"
        );
    }

    /// Send chunks `seqs` on stream `id`, then run one rate-control evaluation
    /// as though its interval had elapsed.
    fn send_and_adapt(e: &mut AudioEngine, id: u32, seqs: std::ops::Range<u32>) {
        for seq in seqs {
            record_sent(&mut e.unacked, &mut e.sent_since_adapt, id, seq);
        }
        e.last_adapt.clear();
        e.adapt_bitrate();
    }

    fn ack(e: &mut AudioEngine, id: u32, acked_seq: u32) {
        e.on_ack(&cursio_protocol::AudioFrameAck { id, acked_seq });
    }

    /// Silence between two sounds is not a backlog.
    ///
    /// The capture's seq keeps counting through silence, whose chunks are
    /// dropped rather than sent, so the first chunk after a pause carries a seq
    /// far past the last one acknowledged. Read as `last_sent - acked`, 10 000
    /// chunks of silence between two videos looked like 50 s of unplayed audio,
    /// and the stream was stepped to its floor the moment sound came back.
    #[test]
    fn a_long_silence_followed_by_audio_does_not_lower_the_bitrate() {
        let mut e = engine();
        let def = cursio_audio::codec::DEFAULT_BITRATE;
        e.stream_bitrate.insert(7, def);

        send_and_adapt(&mut e, 7, 0..50);
        ack(&mut e, 7, 49);

        // Resume after 10 000 silent chunks; nothing new acknowledged yet.
        for tick in 0..6 {
            send_and_adapt(&mut e, 7, 10_050 + tick * 10..10_060 + tick * 10);
            assert_eq!(
                e.stream_bitrate.get(&7),
                Some(&def),
                "the first seconds after a pause must not read as a backlog"
            );
        }
    }

    /// A real backlog still steps the rate down, once per new piece of
    /// evidence rather than once per tick.
    #[test]
    fn a_real_backlog_lowers_the_bitrate_once_per_evaluation_with_new_sends() {
        let mut e = engine();
        let def = cursio_audio::codec::DEFAULT_BITRATE;
        e.stream_bitrate.insert(7, def);
        ack(&mut e, 7, 0);

        send_and_adapt(&mut e, 7, 1..BACKLOG_HIGH + 50);
        let lowered = *e.stream_bitrate.get(&7).unwrap();
        assert!(
            lowered < def,
            "a backlog over the threshold must shed bitrate"
        );

        // Nothing sent since: the same evidence must not be judged again.
        e.last_adapt.clear();
        e.adapt_bitrate();
        assert_eq!(
            e.stream_bitrate.get(&7),
            Some(&lowered),
            "an unchanged backlog must not step the rate down a second time"
        );
    }

    /// The rate climbs back only after the link has stayed caught up.
    #[test]
    fn bitrate_climbs_only_after_the_link_stays_caught_up() {
        let mut e = engine();
        e.stream_bitrate.insert(7, BITRATE_START);
        ack(&mut e, 7, 0);

        let mut seq = 1;
        for tick in 1..=BITRATE_RAISE_AFTER {
            send_and_adapt(&mut e, 7, seq..seq + 5);
            ack(&mut e, 7, seq + 4);
            seq += 5;
            let expected = if tick < BITRATE_RAISE_AFTER {
                BITRATE_START
            } else {
                BITRATE_START + BITRATE_STEP
            };
            assert_eq!(
                e.stream_bitrate.get(&7),
                Some(&expected),
                "tick {tick}: one calm sample must not raise the rate"
            );
        }
    }

    fn engine() -> AudioEngine {
        AudioEngine::new(
            PeerId::new_v4(),
            None,
            crate::live::Shared::new(MicGate::Auto),
            LocalMixer::default(),
            "peer".into(),
        )
    }

    fn route(id: u32, device: Option<&str>) -> cursio_protocol::RouteEntry {
        cursio_protocol::RouteEntry {
            id,
            app_name: Some("app".into()),
            device: device.map(|d| (d.to_string(), d.to_string())),
        }
    }

    /// A single re-route updates where a stream renders; `None` clears it back to
    /// the default. This is the per-stream `AudioStreamRoute` path.
    #[test]
    fn stream_route_sets_and_clears_target() {
        let mut e = engine();
        e.on_stream_route(&route(7, Some("Speakers")));
        assert_eq!(
            e.inbound_app_device.get(&7).map(String::as_str),
            Some("Speakers")
        );

        e.on_stream_route(&route(7, None));
        assert!(
            !e.inbound_app_device.contains_key(&7),
            "None clears the target"
        );
    }

    /// A RoutingState snapshot is applied as a WHOLE object: listed streams get
    /// their target, and any previously-targeted stream NOT in the snapshot is
    /// reset to the default. This is the reconnect guarantee (invariant #10).
    #[test]
    fn routing_state_is_applied_as_a_whole_object() {
        let mut e = engine();
        // Prior state: two streams targeted.
        e.on_stream_route(&route(1, Some("A")));
        e.on_stream_route(&route(2, Some("B")));

        // A new snapshot keeps 1 (retargeted), drops 2, adds 3.
        e.on_routing_state(&cursio_protocol::RoutingState {
            routes: vec![route(1, Some("A2")), route(3, Some("C"))],
        });

        assert_eq!(e.inbound_app_device.get(&1).map(String::as_str), Some("A2"));
        assert!(
            !e.inbound_app_device.contains_key(&2),
            "stream missing from the snapshot is reset"
        );
        assert_eq!(e.inbound_app_device.get(&3).map(String::as_str), Some("C"));
    }

    /// Mute wins over gain, and is reversible without losing the gain value —
    /// the group's effective per-member gain is 0 while muted, the stored gain
    /// again when unmuted.
    #[test]
    fn route_group_effective_gain_mute_wins() {
        let mut g = RouteGroup {
            gain: 0.6,
            mute: false,
        };
        assert_eq!(g.effective_gain(), 0.6);
        g.mute = true;
        assert_eq!(g.effective_gain(), 0.0, "muted group applies zero gain");
        g.mute = false;
        assert_eq!(g.effective_gain(), 0.6, "unmute restores the stored gain");
    }

    /// A group op updates the Route's stored gain/mute, clamps out-of-range gain,
    /// and marks the Route dirty so `pump` fans it out.
    #[test]
    fn route_op_updates_group_and_marks_dirty() {
        let mut e = engine();
        let dev = ("vaio3".to_string(), "VAIO3".to_string());

        e.on_route_op(dev.clone(), cursio_protocol::RouteOp::SetGain(0.5));
        assert_eq!(e.route_groups.get(&dev).unwrap().gain, 0.5);
        assert!(
            e.routes_dirty.contains(&dev),
            "a group op marks the route dirty"
        );

        e.on_route_op(dev.clone(), cursio_protocol::RouteOp::SetMute(true));
        let g = e.route_groups.get(&dev).unwrap();
        assert!(
            g.mute && g.gain == 0.5,
            "mute is independent of the stored gain"
        );

        // Out-of-range gain is clamped, never inverted or blown past headroom.
        e.on_route_op(dev.clone(), cursio_protocol::RouteOp::SetGain(-1.0));
        assert_eq!(e.route_groups.get(&dev).unwrap().gain, 0.0);
        e.on_route_op(dev.clone(), cursio_protocol::RouteOp::SetGain(99.0));
        assert_eq!(e.route_groups.get(&dev).unwrap().gain, 4.0);
    }

    /// Applying the same snapshot twice changes nothing the second time —
    /// idempotence is what makes resending safe on reconnect.
    #[test]
    fn routing_state_is_idempotent() {
        let mut e = engine();
        let snap = cursio_protocol::RoutingState {
            routes: vec![route(1, Some("A")), route(2, None)],
        };
        e.on_routing_state(&snap);
        let before: std::collections::BTreeMap<_, _> =
            e.inbound_app_device.clone().into_iter().collect();
        e.on_routing_state(&snap);
        let after: std::collections::BTreeMap<_, _> =
            e.inbound_app_device.clone().into_iter().collect();
        assert_eq!(
            before, after,
            "re-applying the same snapshot must be a no-op"
        );
        assert_eq!(before.get(&1).map(String::as_str), Some("A"));
        assert!(!before.contains_key(&2), "a None entry never sets a target");
    }

    /// An app that replaces its PipeWire node keeps its stream id.
    ///
    /// Pausing, seeking, or reopening the audio client swaps the node out. When
    /// the route was keyed on the node id, the replacement looked like a brand
    /// new app: the old tunnel closed and a second stream opened — or, for
    /// `PipeWire ALSA [java]` (Minecraft with Simple Voice Chat), nothing
    /// reopened at all and the app stayed silent. Keyed by name, the returning
    /// node inherits the stream that is already up.
    #[test]
    fn an_app_returning_on_a_new_node_keeps_its_stream() {
        let mut e = engine();
        let device = ("vm".to_string(), "Voicemeeter".to_string());

        // Routed once, on node 584.
        e.app_routes.insert("java".into(), (85533, device.clone()));
        e.routed_app_names.insert(584, "java".into());

        // The node goes away; the tunnel is held, not closed.
        e.routed_app_names.remove(&584);
        e.tunnel_idle_since
            .insert("java".into(), std::time::Instant::now());
        assert!(
            e.app_routes.contains_key("java"),
            "the tunnel must survive the node disappearing"
        );

        // It comes back as node 285 — same app, so the same stream id.
        let resumed = e.app_routes.get("java").map(|(id, _)| *id);
        assert_eq!(
            resumed,
            Some(85533),
            "the returning app must reuse its stream, not open a second one"
        );
    }

    /// An app holding several nodes at once must not have them collapsed onto
    /// one stream.
    ///
    /// Firefox opens a node per playing tab, delivered as separate Joined
    /// events. Reusing the app's stream id for the second live node re-pointed
    /// the tunnel at the newcomer and orphaned the tap that was actually
    /// producing sound, so the browser went silent until it happened to
    /// recreate its nodes in a luckier order.
    #[test]
    fn concurrent_nodes_of_one_app_keep_separate_streams() {
        let mut e = engine();
        let device = ("vm".to_string(), "Voicemeeter".to_string());

        // First node of the app takes the name -> stream mapping.
        e.routed_app_names.insert(100, "Firefox".into());
        e.app_routes
            .entry("Firefox".to_string())
            .or_insert_with(|| (85501, device.clone()));

        // A SECOND node of the same app shows up while the first is still live.
        let already_live = e
            .routed_app_names
            .iter()
            .any(|(node, name)| name == "Firefox" && *node != 200);
        assert!(already_live, "the first node must count as live");

        // So it must NOT reuse the first node's stream...
        e.routed_app_names.insert(200, "Firefox".into());
        e.app_routes
            .entry("Firefox".to_string())
            .or_insert_with(|| (85502, device.clone()));

        // ...and the original mapping must survive untouched.
        assert_eq!(
            e.app_routes.get("Firefox").map(|(id, _)| *id),
            Some(85501),
            "a second node must not steal the first node's stream"
        );
    }

    /// A node an app drops while keeping others must not leave its tunnel
    /// behind.
    ///
    /// Minecraft opens two nodes at startup and drops one a second later. The
    /// dead one's tunnel used to be kept "for its return" — but the app had not
    /// gone anywhere, and the peer, choosing between two advertised streams for
    /// the same app, reported `listening=true` for the dead one. The live node's
    /// audio then went nowhere: Minecraft silent after a restart.
    #[test]
    fn a_dropped_node_does_not_leave_a_tunnel_competing_for_the_peer() {
        let mut e = engine();
        let device = ("vm".to_string(), "Voicemeeter".to_string());

        // Two nodes of one app, each with its own stream.
        e.routed_app_names.insert(500, "java".into());
        e.routed_app_names.insert(429, "java".into());
        e.app_routes.insert("java".into(), (85505, device.clone()));

        // Node 429 goes away while 500 keeps playing.
        e.routed_app_names.remove(&429);
        let still_live = e.routed_app_names.values().any(|n| n == "java");
        assert!(
            still_live,
            "the surviving node must be recognised, so 429's tunnel is an orphan"
        );

        // An orphan must NOT start the idle clock — that path is for an app
        // that has genuinely gone quiet.
        assert!(
            !e.tunnel_idle_since.contains_key("java"),
            "an orphaned tunnel is closed outright, not left to expire"
        );

        // And the surviving node keeps its route.
        assert_eq!(
            e.app_routes.get("java").map(|(id, _)| *id),
            Some(85505),
            "closing the orphan must not strip the live node's route"
        );
    }

    /// The idle clock starts only when the app's LAST node goes away.
    #[test]
    fn a_tunnel_stays_while_any_node_of_the_app_lives() {
        let mut e = engine();
        e.routed_app_names.insert(100, "Firefox".into());
        e.routed_app_names.insert(200, "Firefox".into());

        // One node leaves; another is still playing.
        e.routed_app_names.remove(&100);
        let still_live = e.routed_app_names.values().any(|n| n == "Firefox");
        assert!(
            still_live,
            "the tunnel must not start expiring while a node still plays"
        );

        // The last one leaves: now it may expire.
        e.routed_app_names.remove(&200);
        assert!(!e.routed_app_names.values().any(|n| n == "Firefox"));
    }

    /// A node that played for a while and then stopped is a pause, not an
    /// orphan — even while another node of the same app is live.
    ///
    /// Chromium keeps a node per tab, so a paused video was always outlived by
    /// a sibling. Judging by that alone, every pause was read as a startup
    /// leftover: the tunnel closed, `app_routes` lost the app, and the sound did
    /// not come back until something forced a brand new node (seeking did,
    /// un-pausing did not). Over a day of real use the reuse path this exists to
    /// enable never ran once.
    #[test]
    fn a_settled_node_pausing_is_not_treated_as_a_startup_leftover() {
        let long_enough = NODE_SETTLED_AGE + std::time::Duration::from_secs(1);
        assert!(
            !is_startup_leftover(true, long_enough),
            "a node that played for a while is pausing; its tunnel must survive"
        );

        // The case the age test must not give away: Minecraft opens two nodes at
        // startup and drops one immediately. That one never settled, and leaving
        // it advertised had the peer answer demand for a silent stream.
        assert!(
            is_startup_leftover(true, std::time::Duration::from_millis(900)),
            "a node dropped seconds after opening is a leftover and must close"
        );

        // With no sibling the tunnel is the app's only one, so it is held for a
        // return however briefly the node lived.
        assert!(
            !is_startup_leftover(false, std::time::Duration::from_millis(10)),
            "an app's only tunnel is never an orphan"
        );
    }

    /// A stream this machine cannot play is reported as unwanted, not skipped.
    ///
    /// The owner only stops sending when told to, so a stream that produces no
    /// demand message is one the owner keeps streaming forever. Reporting from
    /// the playbacks alone could not say anything about a stream that has none —
    /// which is exactly the stream that most needs to be refused. A Linux
    /// microphone sent to a Windows peer, which has no way to publish it, ran
    /// this way for a whole session: 200 chunks/s, `played=18`.
    #[tokio::test]
    async fn an_unplayable_stream_is_reported_as_unwanted() {
        let mut e = engine();

        // The peer advertises a microphone this machine cannot publish. The
        // open was attempted and failed, so no playback will ever exist.
        e.peer_streams.insert(476800);
        e.failed_playbacks.insert(476800);

        let mut tx = RecordingTx::default();
        e.pump_demand(&mut tx)
            .await
            .expect("reporting demand must not fail");

        assert_eq!(
            tx.demands(),
            vec![(476800, false)],
            "a stream that failed to open must be refused, not passed over in silence"
        );

        // Only transitions go out: the owner holds the last value, and repeating
        // it every pump (~100/s per stream) would flood the channel input rides
        // on to say nothing new.
        let mut again = RecordingTx::default();
        e.pump_demand(&mut again)
            .await
            .expect("reporting demand must not fail");
        assert!(
            again.demands().is_empty(),
            "an unchanged demand must not be resent"
        );
    }

    /// A stream keeps the name it was advertised under while its node comes up.
    ///
    /// The label used to be *taken* out of `pending_labels` by the first chunk,
    /// which was safe only while opening blocked the loop: no second chunk
    /// could exist yet. Once the open moved to a worker, chunks kept arriving
    /// at ~200/s during the hundreds of milliseconds it takes, each finding the
    /// label gone and treating a properly advertised stream as one that was
    /// never announced. On the peer every application collapsed into a single
    /// mixer row bearing whichever name survived.
    #[test]
    fn an_advertised_label_survives_the_wait_for_a_node() {
        let mut e = engine();

        e.pending_labels
            .insert(85505, "Cursio(peer): spotify".into());

        // The open is in flight on a worker; chunks keep arriving meanwhile.
        e.opening_playbacks.insert(85505);
        for seq in 0..3 {
            e.on_chunk(&cursio_protocol::AudioChunk {
                id: 85505,
                seq,
                timestamp: 0,
                data: vec![0; 16],
            });
        }

        assert_eq!(
            e.pending_labels.get(&85505).map(String::as_str),
            Some("Cursio(peer): spotify"),
            "the label must outlive the open, so a retry uses the right name"
        );
    }

    /// A stream that has not opened *yet* is not refused.
    ///
    /// A stream into one of our devices opens its playback on its first chunk,
    /// so refusing one that has no playback yet stops the owner from sending,
    /// which stops the first chunk from arriving, which stops the playback from
    /// ever opening: the refusal makes itself true and the audio never comes
    /// back.
    #[tokio::test]
    async fn a_stream_whose_playback_has_not_opened_yet_is_left_alone() {
        let mut e = engine();

        // Advertised, never yet carried a chunk: no playback, and no failure.
        e.peer_streams.insert(476500);

        let mut tx = RecordingTx::default();
        e.pump_demand(&mut tx)
            .await
            .expect("reporting demand must not fail");

        assert!(
            tx.demands().is_empty(),
            "a stream still waiting for its first chunk must not be refused"
        );
    }

    /// Opening a playback never happens twice for one stream.
    ///
    /// The open is handed to a worker and the node takes hundreds of
    /// milliseconds to appear, during which chunks keep arriving at ~200/s. If
    /// each of those started another opener, one stream would spawn a node per
    /// chunk — the session-loop stall this moved off the loop, turned into a
    /// flood of PipeWire nodes instead.
    #[test]
    fn a_stream_opens_only_one_playback_while_its_node_comes_up() {
        let mut e = engine();

        for _ in 0..5 {
            e.open_playback_off_loop(7, || {
                Err(cursio_audio::AudioError::StreamGone("test".into()))
            });
        }

        assert_eq!(
            e.opening_playbacks.len(),
            1,
            "one stream must have exactly one open in flight"
        );
    }

    /// A machine that cannot publish virtual devices says so once, however
    /// many devices the peer advertises and however often it reconnects.
    #[test]
    fn a_missing_virtual_device_backend_is_reported_once() {
        let reported = std::sync::atomic::AtomicBool::new(false);
        assert!(report_no_virtual_devices(&reported, "Speakers"));
        assert!(!report_no_virtual_devices(&reported, "Virtual"));
        assert!(!report_no_virtual_devices(&reported, "stream 85800"));
    }

    /// A device this machine cannot publish leaves nothing behind that would
    /// swallow the peer's device-state replicas.
    #[test]
    fn an_unpublishable_device_leaves_no_intent_sink() {
        let mut e = engine();
        e.intent_sink_opener = |_, _, _| Err(cursio_audio::AudioError::Unsupported);
        e.on_advertise(&cursio_protocol::AudioStreamMeta {
            id: 43,
            app_name: None,
            device: Some(("dev-handle".into(), "Speakers".into())),
            role: cursio_protocol::StreamRole::Playback,
            format: cursio_protocol::AudioFormat {
                sample_rate: 48000,
                channels: 2,
                sample_kind: cursio_protocol::SampleKind::F32,
            },
            codec: cursio_protocol::AudioCodec::Opus,
        });
        for _ in 0..400 {
            e.collect_opened_intent_sinks();
            if e.opening_intent_sinks.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(e.opening_intent_sinks.is_empty());
        assert!(e.intent_sinks.is_empty());
        assert!(e.sink_node_for_peer_device.is_empty());
    }

    /// A device advertised twice does not become two local outputs.
    ///
    /// The peer re-advertises its devices on every reconnect, and creating a
    /// second Intent Sink for one device would leave the user picking between
    /// duplicate outputs that mirror the same thing.
    #[test]
    fn a_device_advertised_twice_creates_one_intent_sink() {
        let mut e = engine();
        e.intent_sink_opener = |_, _, _| Err(cursio_audio::AudioError::Unsupported);

        let meta = cursio_protocol::AudioStreamMeta {
            id: 42,
            app_name: None,
            device: Some(("dev-handle".into(), "Speakers".into())),
            role: cursio_protocol::StreamRole::Playback,
            format: cursio_protocol::AudioFormat {
                sample_rate: 48000,
                channels: 2,
                sample_kind: cursio_protocol::SampleKind::F32,
            },
            codec: cursio_protocol::AudioCodec::Opus,
        };
        e.on_advertise(&meta);
        e.on_advertise(&meta);

        assert_eq!(
            e.opening_intent_sinks.len(),
            1,
            "one device must have exactly one Intent Sink in flight"
        );
    }

    /// A stream nobody plays is captured but not sent — and resumes the moment
    /// something opens it.
    ///
    /// Advertising a device makes it appear in the peer's device list; it does
    /// not mean anyone opened it. Streaming all 13 mirrored endpoints regardless
    /// is what made the first seconds of a session drop audio, as the sender's
    /// backlog trim discarded what the link could not carry.
    #[test]
    fn a_stream_nobody_plays_is_not_sent() {
        let mut e = engine();

        // The peer reports no consumer for stream 7.
        e.unwanted_streams.insert(7);
        assert!(
            e.unwanted_streams.contains(&7),
            "a stream with no listener must be suppressed"
        );

        // Something opens it: it must start flowing again with no renegotiation.
        e.unwanted_streams.remove(&7);
        assert!(
            !e.unwanted_streams.contains(&7),
            "a stream must resume as soon as it has a listener"
        );
    }

    /// Demand is reported on change only — repeating it every pump would flood
    /// the channel input shares.
    #[test]
    fn demand_is_only_reported_when_it_changes() {
        let mut e = engine();
        e.demand_sent.insert(7, true);

        // Same value as last reported: nothing to send.
        assert_eq!(e.demand_sent.get(&7), Some(&true));

        // A closed stream forgets its demand, so a reused id re-reports.
        e.demand_sent.remove(&7);
        assert_eq!(
            e.demand_sent.get(&7),
            None,
            "a closed stream must not leave stale demand behind"
        );
    }

    /// The tunnel is kept for a pause, but not forever: an app that was quit
    /// must eventually release its stream.
    #[test]
    fn a_pause_is_kept_but_a_quit_is_collected() {
        assert!(
            TUNNEL_IDLE_TIMEOUT > ROUTED_APP_GRACE,
            "the tunnel must outlive the tap it feeds"
        );
        assert!(
            TUNNEL_IDLE_TIMEOUT >= std::time::Duration::from_secs(10),
            "must comfortably outlast a world load or a voice-chat reconnect"
        );
        assert!(
            TUNNEL_IDLE_TIMEOUT <= std::time::Duration::from_secs(300),
            "a quit app must not hold its stream for the session"
        );
    }
}
