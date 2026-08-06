//! Linux audio backend via PipeWire.
//!
//! Implements both the source (capture, [`PwSource`]) and sink (playback,
//! [`PwSink`]) sides of [`crate`] on Linux. PipeWire runs its own thread-local
//! main loop that can't share a tokio task, so each stream owns a dedicated OS
//! thread running a [`pipewire::main_loop::MainLoopRc`]; it talks to the async
//! world only through channels — captured samples out, inbound samples in. This
//! mirrors how `cursio-input`'s evdev capture bridges a blocking source to tokio
//! channels.
//!
//! This first cut is **whole-device, RawPcm f32**: capture one sink's monitor,
//! and play one inbound stream into a virtual output through a small jitter
//! buffer. Per-app nodes (advertising each PipeWire stream by `application.name`)
//! and Opus come in later milestones; the trait shape already accommodates them.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use cursio_protocol::{
    AudioChunk, AudioCodec, AudioFormat, AudioStreamId, AudioStreamMeta, SampleKind, StreamRole,
};
use pipewire::{
    context::ContextRc,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        param::audio::{AudioFormat as SpaAudioFormat, AudioInfoRaw},
        pod::{serialize::PodSerializer, Object, Pod, Property, PropertyFlags, Value, ValueArray},
        sys::{
            SPA_PARAM_EnumFormat, SPA_PARAM_Props, SPA_PROP_channelVolumes, SPA_PROP_mute,
            SPA_TYPE_OBJECT_Format, SPA_TYPE_OBJECT_Props, SPA_AUDIO_CHANNEL_FL,
            SPA_AUDIO_CHANNEL_FR, SPA_AUDIO_MAX_CHANNELS,
        },
        utils::Direction,
    },
    stream::{StreamFlags, StreamRc},
};

use crate::AudioError;

/// Fixed capture/playback format for the first cut: 48 kHz stereo f32.
///
/// f32, not s16: PipeWire monitors are natively float, and requesting f32 avoids
/// a format conversion in the graph (and the earlier bug where the wire claimed
/// s16 while the buffers were actually float32). The Opus milestone will convert
/// to whatever the encoder wants; for RawPcm we carry the native float.
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
/// Bytes per interleaved frame (all channels) for f32 (4 bytes/sample).
const BYTES_PER_FRAME: usize = CHANNELS as usize * 4;

/// The neutral [`AudioFormat`] this backend produces/consumes.
fn wire_format() -> AudioFormat {
    AudioFormat {
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
        sample_kind: SampleKind::F32,
    }
}

/// Build the SPA `EnumFormat` POD describing our fixed f32 stereo 48k format,
/// which the capture stream negotiates against.
///
/// The channel **positions** matter as much as the count. `AudioInfoRaw::new()`
/// starts with the `UNPOSITIONED` flag set, and `set_channels` alone does not
/// clear it — only `set_position` with a non-zero first entry does. An
/// unpositioned stereo stream gets ports with an empty `audio.channel`, and then
/// nothing downstream can tell left from right:
///
/// - `relink_app` pairs app ports to ours by matching `audio.channel`, so with
///   empty labels every port fell through to positional pairing. When an app
///   exposed its ports in any other order — or exposed only one at the moment we
///   linked — both of its outputs could land on the *same* input port, so one
///   ear carried everything and the other stayed silent.
/// - The session manager cannot up/downmix a stream it has no layout for, so a
///   mono source is never spread across both ears.
///
/// Declaring `[FL, FR]` explicitly is what the null-sink already does (see
/// `audio.position` where it is created); this brings the streams in line.
fn format_param_pod() -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(SpaAudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);

    // Front-left / front-right, then zeroes for the unused slots. Setting this
    // also clears the UNPOSITIONED flag, which is the point.
    let mut position = [0u32; SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = SPA_AUDIO_CHANNEL_FL;
    position[1] = SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);

    let value = Value::Object(Object {
        type_: SPA_TYPE_OBJECT_Format,
        id: SPA_PARAM_EnumFormat,
        properties: info.into(),
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .expect("serialize audio format pod")
        .0
        .into_inner()
}

/// Handle to a running PipeWire capture stream (the **source** side).
///
/// Spawns a thread running the PipeWire loop; captured s16 buffers are chunked
/// into [`AudioChunk`]s and forwarded on the tokio channel the
/// [`AudioSource`](crate::AudioSource) trait exposes.
pub struct PwSource {
    id: AudioStreamId,
    adverts_rx: tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta>,
    chunks_rx: tokio::sync::mpsc::UnboundedReceiver<AudioChunk>,
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PwSource {
    /// Start capturing a sink's monitor (what's playing) as one whole-device
    /// stream with the given id.
    ///
    /// `target` optionally names a specific node to capture (a sink's monitor by
    /// node name); `None` lets PipeWire pick the default. The stream taps output
    /// via `stream.capture.sink`.
    pub fn start(id: AudioStreamId, target: Option<String>) -> Result<Self, AudioError> {
        Self::with_mode(id, CaptureMode::MonitorTap { target })
    }

    /// Start as a **virtual output device**: the system lists `description` as a
    /// switchable sink, and whatever is routed to it is captured and streamed —
    /// how a peer's device appears natively in this machine's output list.
    pub fn virtual_sink(
        id: AudioStreamId,
        name: String,
        description: String,
    ) -> Result<Self, AudioError> {
        Self::with_mode(id, CaptureMode::VirtualSink { name, description })
    }

    /// Record a real microphone / line-in, so the peer can use it as an input.
    ///
    /// `target` is the source node name from [`list_input_devices`]; `None`
    /// records the system default input.
    pub fn mic_tap(id: AudioStreamId, target: Option<String>) -> Result<Self, AudioError> {
        Self::with_mode(id, CaptureMode::MicTap { target })
    }

    /// Start capturing **one application's** audio, by its PipeWire node id (as
    /// returned by [`list_app_streams`]). The stream is advertised with
    /// `app_name` set, so the peer creates a per-app node for it.
    pub fn app_tap(
        id: AudioStreamId,
        node_id: String,
        app_name: String,
    ) -> Result<Self, AudioError> {
        Self::with_mode(id, CaptureMode::AppTap { node_id, app_name })
    }

    /// Shared constructor: spin up the capture thread for the given mode.
    fn with_mode(id: AudioStreamId, mode: CaptureMode) -> Result<Self, AudioError> {
        let (adverts_tx, adverts_rx) = tokio::sync::mpsc::unbounded_channel();
        let (chunks_tx, chunks_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        // Handshake so `start` fails synchronously if the PW loop can't build,
        // instead of leaving a silently dead thread.
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();

        // Advertise this stream before any chunk. An app tap carries the app
        // name so the peer creates a per-app node for it; the other modes are
        // whole-device streams.
        let app_name = match &mode {
            CaptureMode::AppTap { app_name, .. } => Some(app_name.clone()),
            _ => None,
        };
        // A microphone is offered as a *source* the peer can pull from; every
        // other mode is something we are playing. The peer branches on this to
        // decide whether to build a virtual speaker or a virtual microphone.
        let role = match &mode {
            CaptureMode::MicTap { .. } => StreamRole::Capture,
            _ => StreamRole::Playback,
        };
        let _ = adverts_tx.send(AudioStreamMeta {
            id,
            role,
            app_name,
            device: None,
            format: wire_format(),
            codec: AudioCodec::RawPcm,
        });

        let thread = std::thread::Builder::new()
            .name(format!("cursio-pw-capture-{id}"))
            .spawn(move || run_capture_loop(id, mode, chunks_tx, stop_rx, ready_tx))
            .map_err(|e| AudioError::Backend(format!("spawn capture thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(PwSource {
                id,
                adverts_rx,
                chunks_rx,
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(AudioError::Backend(e)),
            Err(_) => Err(AudioError::Backend(
                "capture thread exited before signalling readiness".into(),
            )),
        }
    }

    pub fn advertisements(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta> {
        &mut self.adverts_rx
    }

    pub fn chunks(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioChunk> {
        &mut self.chunks_rx
    }

    pub fn id(&self) -> AudioStreamId {
        self.id
    }
}

impl Drop for PwSource {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Intent Sink (device replica as a routing-declaration surface)
// ---------------------------------------------------------------------------
//
// See docs/audio-design.md "Redesign: one system". An Intent Sink is a real
// `Audio/Sink` that represents a peer's device. It is a SILENT FUNNEL: audio
// routed into it goes nowhere locally (it carries no PCM). Its only job is to let
// the user declare intent — "play this into the peer's device X" — by connecting a
// stream to it in their mixer. Cursio observes that membership and, elsewhere,
// opens a per-app Remote Stream that actually carries the audio to device X.
//
// Contrast with `CaptureMode::VirtualSink` (the old device mirror), which captured
// the sink's whole mix and shipped it — that flattened per-app separation and, via
// the default sink, produced duplicate audio.

/// A membership change on an [`PwIntentSink`]: an application's playback stream
/// was linked into the sink, or unlinked from it.
#[derive(Debug, Clone)]
pub enum IntentSinkEvent {
    /// A playback stream is now routed into this Intent Sink.
    Joined { app_node_id: u32, app_name: String },
    /// A playback stream is no longer routed into this Intent Sink.
    Left { app_node_id: u32 },
}

/// A device Intent Sink: a real local sink that carries no audio, only observes
/// which playback streams are funnelled into it (reported on [`Self::events`]).
pub struct PwIntentSink {
    id: AudioStreamId,
    events_rx: tokio::sync::mpsc::UnboundedReceiver<IntentSinkEvent>,
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PwIntentSink {
    /// Create an Intent Sink named `name`/`description` for a peer device. It
    /// appears in the local output list (so the user can route into it) but plays
    /// nothing; membership changes arrive on [`Self::events`].
    pub fn create(
        id: AudioStreamId,
        name: String,
        description: String,
    ) -> Result<Self, AudioError> {
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();

        let thread = std::thread::Builder::new()
            .name(format!("cursio-pw-intent-{id}"))
            .spawn(move || run_intent_sink_loop(name, description, events_tx, stop_rx, ready_tx))
            .map_err(|e| AudioError::Backend(format!("spawn intent-sink thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(PwIntentSink {
                id,
                events_rx,
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(AudioError::Backend(e)),
            Err(_) => Err(AudioError::Backend(
                "intent-sink thread exited before signalling readiness".into(),
            )),
        }
    }

    /// Membership changes: apps joining/leaving this Intent Sink.
    pub fn events(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<IntentSinkEvent> {
        &mut self.events_rx
    }

    pub fn id(&self) -> AudioStreamId {
        self.id
    }
}

impl Drop for PwIntentSink {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Main-loop body for an Intent Sink. Creates a null-audio-sink node (a real sink
/// that discards what it receives — no monitor, no capture stream), then watches
/// the registry: whenever a link is created into one of our input ports, resolve
/// the far-end output node to an application and emit `Joined`; on link removal,
/// emit `Left`.
fn run_intent_sink_loop(
    name: String,
    description: String,
    events_tx: tokio::sync::mpsc::UnboundedSender<IntentSinkEvent>,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
) {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    let setup = (|| -> Result<_, String> {
        let main_loop = MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
        let context = ContextRc::new(&main_loop, None).map_err(|e| format!("context: {e}"))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| format!("connect: {e}"))?;

        // A null-audio-sink is a real Audio/Sink the system lists as an output, but
        // it discards whatever is played into it — exactly a silent funnel. We do
        // NOT create a capture stream on it, so no PCM is ever shipped. Give it a
        // 2-channel stereo layout so ordinary stereo apps link cleanly.
        let sink = core
            .create_object::<pipewire::node::Node>(
                "adapter",
                &pipewire::properties::properties! {
                    "factory.name" => "support.null-audio-sink",
                    *pipewire::keys::MEDIA_CLASS => "Audio/Sink",
                    *pipewire::keys::NODE_NAME => name.as_str(),
                    *pipewire::keys::NODE_DESCRIPTION => description.as_str(),
                    "audio.position" => "[ FL FR ]",
                    // No monitor ports: nothing should tap this sink's output. It
                    // is a dead end by design.
                    "monitor.channel-volumes" => "false",
                },
            )
            .map_err(|e| format!("create null-sink: {e}"))?;

        Ok((main_loop, core, sink))
    })();

    let (main_loop, core, _sink) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let loop_ref = main_loop.loop_();

    // Let the null-sink register so its ports exist before we resolve our node id.
    for _ in 0..20 {
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(25)));
    }

    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("registry: {e}")));
            return;
        }
    };

    // State for interpreting link events. The registry delivers links, ports and
    // nodes in ANY order — a link can arrive before the ports it references, and a
    // port before its node — so we never discard a link we can't resolve yet.
    // Instead we keep everything raw and RE-RESOLVE membership from scratch each
    // time anything changes (and once per loop tick, to catch late arrivals).
    //
    //  - ports:  port_id -> owning node id
    //  - nodes:  node_id -> display name
    //  - links:  link_id -> (output_port, input_port)   [raw, kept until removed]
    //  - our_node: our sink's node id, once we've seen it
    //  - members: node_id -> the set of link_ids from it into our sink (last emit)
    let ports: Rc<RefCell<HashMap<u32, u32>>> = Rc::new(RefCell::new(HashMap::new()));
    let nodes: Rc<RefCell<HashMap<u32, String>>> = Rc::new(RefCell::new(HashMap::new()));
    let links: Rc<RefCell<HashMap<u32, (u32, u32)>>> = Rc::new(RefCell::new(HashMap::new()));
    let our_node: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
    // The set of app node ids we last reported as Joined, so re-resolving is
    // idempotent: we diff against it and emit only real transitions.
    let joined_set: Rc<RefCell<std::collections::HashSet<u32>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));

    // Recompute which app nodes are linked into our sink and emit Joined/Left for
    // the difference vs last time. Cheap and idempotent — safe to call often.
    let resolve = {
        let ports = ports.clone();
        let nodes = nodes.clone();
        let links = links.clone();
        let our_node = our_node.clone();
        let joined_set = joined_set.clone();
        let events = events_tx.clone();
        move || {
            let Some(sink_node) = *our_node.borrow() else {
                return; // don't know our own node id yet
            };
            let ports = ports.borrow();
            // Current app nodes whose output is linked into our sink's input.
            let mut current: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (out_port, in_port) in links.borrow().values() {
                let (Some(&out_node), Some(&in_node)) = (ports.get(out_port), ports.get(in_port))
                else {
                    continue; // ports not registered yet; try again next tick
                };
                if in_node == sink_node && out_node != sink_node {
                    current.insert(out_node);
                }
            }
            let mut last = joined_set.borrow_mut();
            // Joined: in current but not last.
            for &node in current.iter() {
                if !last.contains(&node) {
                    let app_name = nodes
                        .borrow()
                        .get(&node)
                        .cloned()
                        .unwrap_or_else(|| format!("node {node}"));
                    let _ = events.send(IntentSinkEvent::Joined {
                        app_node_id: node,
                        app_name,
                    });
                }
            }
            // Left: in last but not current.
            for &node in last.iter() {
                if !current.contains(&node) {
                    let _ = events.send(IntentSinkEvent::Left { app_node_id: node });
                }
            }
            *last = current;
        }
    };

    let ports_g = ports.clone();
    let nodes_g = nodes.clone();
    let our_node_g = our_node.clone();
    let links_g = links.clone();
    let sink_name = name.clone();
    let resolve_g = resolve.clone();
    let _listener = registry
        .add_listener_local()
        .global(move |obj| {
            let Some(props) = obj.props else { return };
            let mut changed = false;
            // Node: record a display name; detect our own sink by node.name.
            if let Some(node_name) = props.get("node.name") {
                let display = props
                    .get("application.name")
                    .or_else(|| props.get("node.description"))
                    .unwrap_or(node_name)
                    .to_string();
                nodes_g.borrow_mut().insert(obj.id, display);
                if node_name == sink_name && our_node_g.borrow().is_none() {
                    *our_node_g.borrow_mut() = Some(obj.id);
                }
                changed = true;
            }
            // Port: map port id -> owning node id.
            if let Some(node) = props.get("node.id").and_then(|v| v.parse::<u32>().ok()) {
                if props.get("port.direction").is_some() {
                    ports_g.borrow_mut().insert(obj.id, node);
                    changed = true;
                }
            }
            // Link: keep it raw (by its own object id) with the ports it joins.
            if let (Some(op), Some(ip)) = (
                props
                    .get("link.output.port")
                    .and_then(|v| v.parse::<u32>().ok()),
                props
                    .get("link.input.port")
                    .and_then(|v| v.parse::<u32>().ok()),
            ) {
                links_g.borrow_mut().insert(obj.id, (op, ip));
                changed = true;
            }
            if changed {
                resolve_g();
            }
        })
        .register();

    let links_r = links.clone();
    let resolve_r = resolve.clone();
    let _rm_listener = registry
        .add_listener_local()
        .global_remove(move |id| {
            // If a removed object was a link we tracked, forget it and re-resolve;
            // membership recompute will emit Left for any node that dropped to zero.
            if links_r.borrow_mut().remove(&id).is_some() {
                resolve_r();
            }
        })
        .register();

    let _ = ready_tx.send(Ok(()));

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(50)));
        // Re-resolve every tick too: a link may have arrived before its ports/node
        // were registered, so the event-time resolve saw nothing. This catches it.
        resolve();
    }
}

/// A change to a real device's volume/mute, observed by [`PwDeviceMonitor`].
///
/// The owner side of the "one system" model emits one of these whenever a
/// device it owns changes (the user turned the knob, or a remote request was
/// applied), so the session can broadcast the new [`DeviceState`] to every node.
#[derive(Debug, Clone)]
pub struct DeviceChange {
    /// The device's node name (its owner-side handle, matched to the peer's
    /// Intent Sink).
    pub node_name: String,
    /// Master volume, linear `0.0`..=`1.0` (already de-cubed from PipeWire's
    /// internal cubic `channelVolumes`).
    pub volume: f32,
    /// Whether the device is muted.
    pub mute: bool,
}

/// A command to set a real device's volume/mute, sent to a [`PwDeviceMonitor`].
enum DeviceCommand {
    SetVolume { node_name: String, volume: f32 },
    SetMute { node_name: String, mute: bool },
}

/// Watches every local audio **device** (sinks and sources), reports volume/mute
/// changes, and applies volume/mute sets — the backend half of device-state
/// replication (see docs/audio-design.md, "Device state is REPLICATED").
///
/// It runs one persistent PipeWire loop (its own thread, like the other
/// backends): it binds each device node as a proxy, subscribes to its `Props`
/// param, and translates PipeWire's cubic `channelVolumes` to/from the linear
/// `0..1` the protocol carries. The owner uses [`changes`](Self::changes) to
/// learn when to broadcast, and [`set_volume`]/[`set_mute`] to apply a request.
pub struct PwDeviceMonitor {
    changes_rx: tokio::sync::mpsc::UnboundedReceiver<DeviceChange>,
    commands: std_mpsc::Sender<DeviceCommand>,
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PwDeviceMonitor {
    /// Start the monitor. Fails only if the PipeWire connection can't be made.
    pub fn start() -> Result<Self, AudioError> {
        let (changes_tx, changes_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<DeviceCommand>();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();

        let thread = std::thread::Builder::new()
            .name("cursio-pw-device-monitor".to_string())
            .spawn(move || run_device_monitor_loop(changes_tx, cmd_rx, stop_rx, ready_tx))
            .map_err(|e| AudioError::Backend(format!("spawn device-monitor thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(PwDeviceMonitor {
                changes_rx,
                commands: cmd_tx,
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(AudioError::Backend(e)),
            Err(_) => Err(AudioError::Backend(
                "device-monitor thread exited before signalling readiness".into(),
            )),
        }
    }

    /// Observed device volume/mute changes.
    pub fn changes(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<DeviceChange> {
        &mut self.changes_rx
    }

    /// Apply a master volume (linear `0.0`..=`1.0`) to the named device.
    pub fn set_volume(&self, node_name: String, volume: f32) {
        let _ = self
            .commands
            .send(DeviceCommand::SetVolume { node_name, volume });
    }

    /// Apply a mute flag to the named device.
    pub fn set_mute(&self, node_name: String, mute: bool) {
        let _ = self
            .commands
            .send(DeviceCommand::SetMute { node_name, mute });
    }
}

impl Drop for PwDeviceMonitor {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// PipeWire stores per-channel volume on a **cubic** curve: the value a UI shows
/// as `v` is stored as `v³`. Convert between the linear `0..1` the protocol uses
/// and PipeWire's stored cube, so a "42%" set here reads back as 42% in
/// pavucontrol/helvum (the single-slider illusion depends on this matching).
fn linear_to_cubic(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    v * v * v
}
fn cubic_to_linear(v: f32) -> f32 {
    v.clamp(0.0, 1.0).cbrt()
}

/// Build a `Props` POD setting `channelVolumes` (all channels to `cubic`) and/or
/// `mute`. `channels` sizes the volume array to match the device.
fn props_pod(channels: usize, cubic: Option<f32>, mute: Option<bool>) -> Vec<u8> {
    let mut properties = Vec::new();
    if let Some(cubic) = cubic {
        properties.push(Property {
            key: SPA_PROP_channelVolumes,
            flags: PropertyFlags::empty(),
            value: Value::ValueArray(ValueArray::Float(vec![cubic; channels.max(1)])),
        });
    }
    if let Some(mute) = mute {
        properties.push(Property {
            key: SPA_PROP_mute,
            flags: PropertyFlags::empty(),
            value: Value::Bool(mute),
        });
    }
    let value = Value::Object(Object {
        type_: SPA_TYPE_OBJECT_Props,
        id: SPA_PARAM_Props,
        properties,
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .expect("serialize props pod")
        .0
        .into_inner()
}

/// One device the monitor is tracking: its proxy (kept alive to receive param
/// events), a param listener, its channel count, and last-seen state.
struct TrackedDevice {
    node: pipewire::node::Node,
    _listener: pipewire::node::NodeListener,
    channels: usize,
    last: Option<(f32, bool)>,
}

/// Persistent loop: bind every device node, watch its `Props`, and apply sets.
fn run_device_monitor_loop(
    changes_tx: tokio::sync::mpsc::UnboundedSender<DeviceChange>,
    cmd_rx: std_mpsc::Receiver<DeviceCommand>,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
) {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    let setup = (|| -> Result<_, String> {
        let main_loop = MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
        let context = ContextRc::new(&main_loop, None).map_err(|e| format!("context: {e}"))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| format!("connect: {e}"))?;
        let registry = core
            .get_registry_rc()
            .map_err(|e| format!("registry: {e}"))?;
        Ok((main_loop, core, registry))
    })();

    let (main_loop, _core, registry) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // node.name -> tracked proxy. Kept in an Rc<RefCell> so the registry callback
    // (which binds new devices) and the command/tick code can both reach it.
    let tracked: Rc<RefCell<HashMap<String, TrackedDevice>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let _reg_listener = {
        let tracked = tracked.clone();
        let registry_inner = registry.clone();
        let changes_tx = changes_tx.clone();
        registry
            .add_listener_local()
            .global(move |obj| {
                let Some(props) = obj.props else { return };
                let class = props.get("media.class").unwrap_or("");
                if class != "Audio/Sink" && class != "Audio/Source" {
                    return;
                }
                let Some(name) = props.get("node.name") else {
                    return;
                };
                // NOTE: unlike device *enumeration*, we do NOT skip our own
                // `cursio_*` nodes here. An Intent Sink is exactly the node the
                // non-owner side turns to drive the owner's device (magic media
                // keys), so its volume must be observable. The session decides
                // per node whether a change means "broadcast my device" (a real
                // local device) or "request the owner" (an Intent Sink); nodes
                // that are neither are simply ignored there.
                if tracked.borrow().contains_key(name) {
                    return; // already bound
                }
                let channels = props
                    .get("audio.channels")
                    .and_then(|c| c.parse::<usize>().ok())
                    .unwrap_or(2);

                let node: pipewire::node::Node = match registry_inner.bind(obj) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::debug!(device = %name, error = %e, "cannot bind device node");
                        return;
                    }
                };

                // Report Props changes for this node as DeviceChange events.
                let name_owned = name.to_string();
                let listener = {
                    let changes_tx = changes_tx.clone();
                    let tracked_cb = tracked.clone();
                    let name_for_cb = name_owned.clone();
                    node.add_listener_local()
                        .param(move |_seq, id, _index, _next, param| {
                            if id != pipewire::spa::param::ParamType::Props {
                                return;
                            }
                            let Some(pod) = param else { return };
                            let Some((volume, mute)) = parse_props(pod) else {
                                return;
                            };
                            // Deduplicate: PipeWire re-emits Props on unrelated
                            // changes, and re-broadcasting an unchanged value
                            // would be noise (and could ping-pong with the peer).
                            if let Some(dev) = tracked_cb.borrow_mut().get_mut(&name_for_cb) {
                                if dev.last == Some((volume, mute)) {
                                    return;
                                }
                                dev.last = Some((volume, mute));
                            }
                            let _ = changes_tx.send(DeviceChange {
                                node_name: name_for_cb.clone(),
                                volume,
                                mute,
                            });
                        })
                        .register()
                };

                // Ask for the current Props so we learn the initial state.
                node.subscribe_params(&[pipewire::spa::param::ParamType::Props]);

                tracked.borrow_mut().insert(
                    name_owned,
                    TrackedDevice {
                        node,
                        _listener: listener,
                        channels,
                        last: None,
                    },
                );
            })
            .global_remove({
                // A device disappearing is handled by the session (the advertise
                // for it goes away); here we just need the registry listener to
                // exist. We don't map global id -> name cheaply, so leave the
                // proxy to be dropped when the whole monitor stops; a re-added
                // device rebinds under its name above.
                move |_id| {}
            })
            .register()
    };

    let _ = ready_tx.send(Ok(()));

    let loop_ref = main_loop.loop_();
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        // Apply any pending set commands, building a Props pod per request.
        while let Ok(cmd) = cmd_rx.try_recv() {
            let t = tracked.borrow();
            match cmd {
                DeviceCommand::SetVolume { node_name, volume } => {
                    if let Some(dev) = t.get(&node_name) {
                        let pod = props_pod(dev.channels, Some(linear_to_cubic(volume)), None);
                        if let Some(p) = Pod::from_bytes(&pod) {
                            dev.node
                                .set_param(pipewire::spa::param::ParamType::Props, 0, p);
                        }
                    } else {
                        tracing::debug!(device = %node_name, "set_volume for an unknown device");
                    }
                }
                DeviceCommand::SetMute { node_name, mute } => {
                    if let Some(dev) = t.get(&node_name) {
                        let pod = props_pod(dev.channels, None, Some(mute));
                        if let Some(p) = Pod::from_bytes(&pod) {
                            dev.node
                                .set_param(pipewire::spa::param::ParamType::Props, 0, p);
                        }
                    }
                }
            }
        }
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(50)));
    }
}

/// Parse a `Props` POD, returning (linear volume, mute) if present.
///
/// Reads the *first* channel of `channelVolumes` as the master (all channels are
/// set together by [`props_pod`], so they agree) and de-cubes it to linear.
fn parse_props(pod: &Pod) -> Option<(f32, bool)> {
    use pipewire::spa::pod::deserialize::PodDeserializer;
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else {
        return None;
    };
    let mut volume: Option<f32> = None;
    let mut mute: Option<bool> = None;
    // NOTE: compare with `if`, not a `match` arm. `SPA_PROP_*` are runtime
    // constants whose names aren't UPPER_SNAKE, so a `match prop.key { SPA_PROP_x
    // => .. }` would treat them as *new bindings* that match anything — silently
    // wrong. Explicit `==` avoids that trap.
    for prop in obj.properties {
        if prop.key == SPA_PROP_channelVolumes {
            if let Value::ValueArray(ValueArray::Float(vols)) = prop.value {
                if let Some(&first) = vols.first() {
                    volume = Some(cubic_to_linear(first));
                }
            }
        } else if prop.key == SPA_PROP_mute {
            if let Value::Bool(m) = prop.value {
                mute = Some(m);
            }
        }
    }
    // Only report once we know both; a Props update carrying just one is merged
    // with the last-known by the caller's dedup, but for the initial event we
    // want a complete state. Default the missing half conservatively.
    match (volume, mute) {
        (Some(v), Some(m)) => Some((v, m)),
        (Some(v), None) => Some((v, false)),
        (None, Some(m)) => Some((1.0, m)),
        (None, None) => None,
    }
}

/// List PipeWire sink (output) nodes, for mirroring to the peer.
///
/// Walks the registry's initial dump — every global with
/// `media.class = Audio/Sink` is an output device — and returns its node name
/// plus a human description. Cursio's own mirror sinks are skipped so we never
/// mirror a mirror (which would loop audio between the machines).
///
/// Runs a short-lived PipeWire loop inline (the registry dump arrives within a
/// few iterations), rather than holding a connection open: enumeration is a
/// one-shot question, not a stream.
pub fn list_output_devices() -> Result<Vec<crate::OutputDevice>, AudioError> {
    list_devices_of_class("Audio/Sink")
}

/// Enumerate the machine's capture devices (microphones, line-ins).
///
/// The mirror image of [`list_output_devices`]: in PipeWire a microphone is just
/// a node whose `media.class` is `Audio/Source` instead of `Audio/Sink`, so both
/// share one implementation rather than two near-identical registry walks.
pub fn list_input_devices() -> Result<Vec<crate::OutputDevice>, AudioError> {
    list_devices_of_class("Audio/Source")
}

/// Walk the PipeWire registry for nodes of one `media.class`.
///
/// Our own mirrors are skipped (see [`crate::MIRROR_MARKER`]) so a mirrored
/// device is never mirrored back — that would loop audio between the machines.
fn list_devices_of_class(media_class: &str) -> Result<Vec<crate::OutputDevice>, AudioError> {
    use std::cell::RefCell;
    use std::rc::Rc;

    let media_class = media_class.to_string();

    let main_loop =
        MainLoopRc::new(None).map_err(|e| AudioError::Backend(format!("main loop: {e}")))?;
    let context = ContextRc::new(&main_loop, None)
        .map_err(|e| AudioError::Backend(format!("context: {e}")))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| AudioError::Backend(format!("connect: {e}")))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| AudioError::Backend(format!("registry: {e}")))?;

    let found: Rc<RefCell<Vec<crate::OutputDevice>>> = Rc::new(RefCell::new(Vec::new()));
    let found_cb = found.clone();

    let _listener = registry
        .add_listener_local()
        .global(move |obj| {
            let Some(props) = obj.props else { return };
            if props.get("media.class") != Some(media_class.as_str()) {
                return;
            }
            let Some(name) = props.get("node.name") else {
                return;
            };
            let description = props.get("node.description").unwrap_or(name).to_string();
            // Skip anything of ours, checking BOTH the node name and the
            // description. Our virtual microphones are named by their label
            // ("Cursio(peer): Mic"), so a name-only check can miss one — and
            // capturing our own virtual mic feeds the peer's audio straight back
            // to it, which the user heard as the microphone playing into their
            // headphones.
            if name.to_lowercase().contains(crate::MIRROR_MARKER)
                || description.to_lowercase().contains(crate::MIRROR_MARKER)
            {
                return;
            }
            found_cb.borrow_mut().push(crate::OutputDevice {
                id: name.to_string(),
                description,
            });
        })
        .register();

    // Pump the loop briefly so the registry's initial dump is delivered.
    let loop_ref = main_loop.loop_();
    for _ in 0..20 {
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(25)));
    }

    let devices = found.borrow().clone();
    Ok(devices)
}

/// List currently-playing application streams, for per-app sharing.
///
/// PipeWire models every playing app as a node with
/// `media.class = Stream/Output/Audio`, so this is the same registry walk as
/// [`list_output_devices`] with a different class filter. The returned `id` is
/// the node's **numeric id** (as a string), not the app name: one application
/// can have several concurrent streams (two Firefox tabs are two nodes), and
/// only the node id distinguishes them.
///
/// Cursio's own streams are skipped so we never capture our own playback.
pub fn list_app_streams() -> Result<Vec<crate::AppStream>, AudioError> {
    use std::cell::RefCell;
    use std::rc::Rc;

    let main_loop =
        MainLoopRc::new(None).map_err(|e| AudioError::Backend(format!("main loop: {e}")))?;
    let context = ContextRc::new(&main_loop, None)
        .map_err(|e| AudioError::Backend(format!("context: {e}")))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| AudioError::Backend(format!("connect: {e}")))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| AudioError::Backend(format!("registry: {e}")))?;

    let found: Rc<RefCell<Vec<crate::AppStream>>> = Rc::new(RefCell::new(Vec::new()));
    let found_cb = found.clone();

    let _listener = registry
        .add_listener_local()
        .global(move |obj| {
            let Some(props) = obj.props else { return };
            if props.get("media.class") != Some("Stream/Output/Audio") {
                return;
            }
            // Skip voice/telephony streams. A Bluetooth headset mic under the
            // HFP/HSP profile registers as `Stream/Output/Audio` — the SAME class
            // as a media player — but with `media.role = "Communication"`. Without
            // this guard we tapped the headset MIC as if it were an app playing
            // audio: PipeWire then routed that "playback" into the default sink
            // (the user's Ubuntu speakers, which mirror to Windows), so speaking
            // into the headset came back through Windows and the user heard
            // themselves. Real players (Spotify, Firefox) carry role = "Music",
            // so filtering on the communication role is precise.
            if props.get("media.role") == Some("Communication") {
                return;
            }
            // Prefer the app name; fall back to the node name (some streams,
            // e.g. plain `java`, carry no application.name).
            let name = props
                .get("application.name")
                .or_else(|| props.get("node.name"))
                .unwrap_or("unknown");
            if name.to_lowercase().contains(crate::MIRROR_MARKER) {
                return; // our own playback
            }
            found_cb.borrow_mut().push(crate::AppStream {
                id: obj.id.to_string(),
                app_name: name.to_string(),
            });
        })
        .register();

    let loop_ref = main_loop.loop_();
    for _ in 0..20 {
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(25)));
    }

    let streams = found.borrow().clone();
    Ok(streams)
}

/// How a capture stream sources its audio.
///
/// Both feed the identical `process` path (dequeue → chunk → send); they differ
/// only in the PipeWire node properties, so the loop below is shared.
pub enum CaptureMode {
    /// Tap an existing sink's monitor — capture "what's playing" on a device.
    /// `target` optionally names the monitor node; `None` = default.
    MonitorTap { target: Option<String> },
    /// Register as a real virtual OUTPUT DEVICE (`media.class = Audio/Sink`):
    /// the system lists it as a switchable sink, and audio routed to it arrives
    /// here as capture input. This is how a peer's device appears natively in
    /// this machine's output list. `name`/`description` name it in the graph/UI.
    VirtualSink { name: String, description: String },
    /// Tap **one application's** stream, by its PipeWire node id — the unit of
    /// per-app sharing. Unlike [`MonitorTap`](CaptureMode::MonitorTap) (a whole
    /// device's mix) this follows a single app, so the peer can route and
    /// volume-control that app on its own.
    AppTap { node_id: String, app_name: String },
    /// Record a real capture device — a microphone or line-in.
    ///
    /// Distinct from [`MonitorTap`](CaptureMode::MonitorTap), which listens to
    /// what a sink is *playing*: this pulls from an `Audio/Source`, so the peer
    /// can use our microphone as its own input. `target` names the source node;
    /// `None` takes the system default.
    MicTap { target: Option<String> },
}

/// The PipeWire main-loop body for capture. Runs on its own thread until the
/// stop channel fires; signals `ready` once the stream is set up (or the error).
fn run_capture_loop(
    id: AudioStreamId,
    mode: CaptureMode,
    chunks_tx: tokio::sync::mpsc::UnboundedSender<AudioChunk>,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
) {
    let setup = (|| -> Result<_, String> {
        let main_loop = MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
        let context = ContextRc::new(&main_loop, None).map_err(|e| format!("context: {e}"))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| format!("connect: {e}"))?;

        // Common props; the mode adds/overrides what distinguishes the two.
        let mut props = properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Music",
            // Small quantum so chunks are ~10ms (low-latency budget). 512 frames
            // @48k ≈ 10.7ms. PipeWire may clamp to its allowed range.
            *pipewire::keys::NODE_LATENCY => "512/48000",
        };
        match &mode {
            CaptureMode::MonitorTap { target } => {
                // Capturing a sink's output means tapping its monitor.
                props.insert(*pipewire::keys::STREAM_CAPTURE_SINK, "true");
                if let Some(t) = target {
                    // `target.object` key is feature-gated in the binding; use
                    // the literal key string, which PipeWire understands.
                    props.insert("target.object", t.as_str());
                }
            }
            CaptureMode::VirtualSink { name, description } => {
                // Register as a real output device. Audio the system routes to
                // this sink arrives in our `process` as input.
                props.insert(*pipewire::keys::MEDIA_CLASS, "Audio/Sink");
                props.insert(*pipewire::keys::NODE_NAME, name.as_str());
                props.insert(*pipewire::keys::NODE_DESCRIPTION, description.as_str());
                props.insert(*pipewire::keys::AUDIO_CHANNELS, "2");
            }
            CaptureMode::AppTap { app_name, .. } => {
                // Tap ONE application's stream. We do NOT set `target.object` and
                // the stream connects WITHOUT autoconnect (see the connect call):
                // the app's output ports are linked to us by hand in
                // `link_app_to_capture`. This is deliberate — letting the session
                // manager auto-link a capture stream pulls in the default source
                // (the microphone), which is exactly the echo bug. Manual linking
                // is the only way to tap precisely this app and nothing else.
                //
                // `stream.capture.sink` stays off — we tap a *stream*, not a
                // device monitor.
                props.insert(
                    *pipewire::keys::NODE_NAME,
                    format!("cursio-app-{app_name}").as_str(),
                );
                props.insert(*pipewire::keys::MEDIA_CLASS, "Stream/Input/Audio");
                props.insert(*pipewire::keys::NODE_DONT_RECONNECT, "true");
            }
            CaptureMode::MicTap { target } => {
                // A plain recording stream. `stream.capture.sink` must stay OFF:
                // setting it would ask for a sink's monitor (what is playing)
                // rather than the source itself, so we would record the speakers
                // instead of the microphone.
                if let Some(t) = target {
                    props.insert("target.object", t.as_str());
                }
                props.insert(*pipewire::keys::MEDIA_CLASS, "Stream/Input/Audio");
                // Name the node after the device it records.
                //
                // A single fixed name ("cursio-mic") was wrong twice over: every
                // microphone produced a node with the SAME name, and — because
                // the node was generic — the session manager happily linked it
                // to whatever it liked, including our own mirrors. The result
                // was a feedback path: mic → peer → mirror → the user's
                // headphones ("микрофон ссыт мне в уши").
                props.insert(
                    *pipewire::keys::NODE_NAME,
                    format!("cursio-mic-{}", target.as_deref().unwrap_or("default")).as_str(),
                );
                // Stay on the device we asked for. Without this the session
                // manager is free to move the stream to another source when
                // devices change, which is part of how it ended up recording
                // things it had no business recording.
                props.insert(*pipewire::keys::NODE_DONT_RECONNECT, "true");
            }
        }

        let stream = StreamRc::new(core.clone(), "cursio-capture", props)
            .map_err(|e| format!("stream: {e}"))?;
        Ok((main_loop, stream, core))
    })();

    let (main_loop, stream, core) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // Per-stream monotonic seq + sample-clock timestamp for the wire chunks.
    let seq = std::cell::Cell::new(0u32);
    let ts = std::cell::Cell::new(0u64);

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, ()| {
            while let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { break };
                // The valid samples are `chunk.size` bytes at `chunk.offset` —
                // NOT the whole mapped buffer, whose length is `maxsize`
                // (reading the full slice over-reads ~48x and duplicates stale
                // tail data). Snapshot offset/size before borrowing the bytes.
                let chunk = data.chunk();
                let offset = chunk.offset() as usize;
                let size = chunk.size() as usize;
                if size == 0 {
                    break;
                }
                let Some(bytes) = data.data() else { break };
                let end = (offset + size).min(bytes.len());
                let payload = &bytes[offset.min(bytes.len())..end];
                if payload.is_empty() {
                    break;
                }
                let n = seq.get();
                seq.set(n.wrapping_add(1));
                let frames = (payload.len() / BYTES_PER_FRAME) as u64;
                let t = ts.get();
                ts.set(t.wrapping_add(frames));
                let _ = chunks_tx.send(AudioChunk {
                    id,
                    seq: n,
                    timestamp: t,
                    data: payload.to_vec(),
                });
            }
        })
        .register();

    // Connect the stream against our fixed format.
    //
    // AUTOCONNECT is the root of the microphone-echo bug. With it set, PipeWire
    // asks the session manager to link this node to a "compatible" node — and for
    // a capture stream the compatible node is the default *source*, i.e. the
    // microphone. So every app tap got the mic wired into it (on TOP of the app,
    // which we link by hand in `link_app_to_capture`), and that mic rode every
    // per-app stream to the peer and came back as the user hearing themselves.
    // An app tap must NOT autoconnect: we wire it to exactly the app's ports
    // ourselves. Device monitors and mic taps still want autoconnect to reach
    // their target, so only the app tap drops the flag.
    let flags = match &mode {
        CaptureMode::AppTap { .. } => StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        _ => StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
    };
    let pod_bytes = format_param_pod();
    let mut params = [Pod::from_bytes(&pod_bytes).expect("valid format pod")];
    if let Err(e) = stream.connect(Direction::Input, None, flags, &mut params) {
        let _ = ready_tx.send(Err(format!("stream connect: {e}")));
        return;
    }

    let _ = ready_tx.send(Ok(()));

    let loop_ref = main_loop.loop_();

    // An app tap must be **explicitly linked** to the application's output
    // ports. Unlike a device monitor, `target.object` alone leaves our node
    // unconnected (ports exist, nothing flows), so we wire it up ourselves —
    // the same thing `pw-link` does. See docs/audio-design.md.
    //
    // Crucially this is **not one-shot**: browsers (Chromium, Firefox) tear down
    // an app's output ports on pause/seek and create fresh ones on resume — often
    // on the SAME node id, so no Joined/Left fires and the Remote Stream is never
    // rebuilt. Our original links then point at ports that no longer exist and
    // audio is silent forever (until a page *reload*, which does make a new node).
    // So we keep a live map of app-out-port -> link and re-link on a short
    // interval: drop links whose app port vanished, add links for new app ports.
    let mut app_links: std::collections::HashMap<u32, (u32, pipewire::link::Link)> =
        std::collections::HashMap::new();
    let app_id: u32 = if let CaptureMode::AppTap { node_id, .. } = &mode {
        node_id.parse().unwrap_or(u32::MAX)
    } else {
        u32::MAX
    };
    if let CaptureMode::AppTap { .. } = &mode {
        // Do the first link promptly, retrying while the graph settles (our input
        // ports appear a beat after connect without AUTOCONNECT).
        for attempt in 0..40 {
            for _ in 0..4 {
                loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(25)));
            }
            relink_app(&core, &loop_ref, app_id, &stream, &mut app_links, 20);
            if !app_links.is_empty() {
                tracing::info!(
                    app_id,
                    count = app_links.len(),
                    attempt,
                    "linked app to capture"
                );
                break;
            }
            if attempt == 39 {
                tracing::error!(app_id, "failed to link app to capture after retries");
            }
        }
    }

    // Drive the loop, polling the stop channel between iterations. PipeWire's
    // loop has no cross-thread "run until signalled", so we iterate with a short
    // timeout and re-check. For an app tap, re-link periodically so a pause/seek
    // that recreates the app's ports doesn't silence the stream permanently.
    let mut last_relink = std::time::Instant::now();
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(50)));
        if app_id != u32::MAX && last_relink.elapsed() >= APP_RELINK_INTERVAL {
            last_relink = std::time::Instant::now();
            // Short settle on the periodic pass: the registry is already cached,
            // and a long pump here would stall the capture loop (same thread).
            relink_app(&core, &loop_ref, app_id, &stream, &mut app_links, 4);
        }
    }
}

/// How often an app tap re-checks its links to the application's output ports.
///
/// A pause/seek that recreates the app's ports must heal quickly (the audio is
/// gone until it does), but re-walking the registry costs a little, so this is a
/// balance — fast enough that a resume is near-instant, slow enough to be cheap.
const APP_RELINK_INTERVAL: Duration = Duration::from_millis(400);

/// Wire an application's output ports to our capture node's input ports.
///
/// Walks the registry for both nodes' current ports, then:
/// - drops any link whose app output port has disappeared (a pause/seek tore it
///   down; the dead proxy is removed so the map reflects reality), and
/// - creates a link for any app output port not yet linked (a resume made new
///   ports), pairing by channel (FL→FL, FR→FR, falling back to declaration
///   order).
///
/// `links` maps an app output port id to the input port it was wired to and the
/// live link proxy. The input port is recorded so a later pass can tell which of
/// our inputs are already spoken for. Those proxies must
/// be **kept alive** — dropping one tears its link down, which is exactly how a
/// stale link is removed above. Idempotent and safe to call on a timer: with no
/// changes it walks the registry and does nothing.
///
/// This heals the pause/seek silence: browsers recreate an app's output ports on
/// resume (often on the same node id, so no Joined/Left fires), and without
/// re-linking our tap would stay wired to ports that no longer exist.
fn relink_app(
    core: &pipewire::core::CoreRc,
    loop_ref: &pipewire::loop_::Loop,
    app_id: u32,
    stream: &StreamRc,
    links: &mut std::collections::HashMap<u32, (u32, pipewire::link::Link)>,
    // How long to pump the loop so the registry dump arrives. The first link
    // needs a generous settle; a periodic re-check does not (objects are already
    // cached and re-listed fast), and a long pump here would block the capture
    // loop — which runs on this same thread — starving audio for ~half a second.
    settle_iters: u32,
) {
    use std::cell::RefCell;
    use std::rc::Rc;

    let our_node_id = stream.node_id();
    let Ok(registry) = core.get_registry_rc() else {
        return;
    };

    // Collect (node_id, port_id, direction, channel) for the two nodes we care
    // about, plus the name of a link factory.
    type Port = (u32, u32, String, String); // node, port, direction, channel
    let ports: Rc<RefCell<Vec<Port>>> = Rc::new(RefCell::new(Vec::new()));
    let factory: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let ports_cb = ports.clone();
    let factory_cb = factory.clone();

    let _listener = registry
        .add_listener_local()
        .global(move |obj| {
            let Some(props) = obj.props else { return };
            if props.get("factory.type.name") == Some("PipeWire:Interface:Link") {
                if let Some(name) = props.get("factory.name") {
                    *factory_cb.borrow_mut() = Some(name.to_string());
                }
                return;
            }
            let Some(node) = props.get("node.id").and_then(|v| v.parse::<u32>().ok()) else {
                return;
            };
            if node != app_id && node != our_node_id {
                return;
            }
            let Some(dir) = props.get("port.direction") else {
                return;
            };
            let channel = props.get("audio.channel").unwrap_or("").to_string();
            ports_cb
                .borrow_mut()
                .push((node, obj.id, dir.to_string(), channel));
        })
        .register();

    // Pump so the registry dump arrives (see `settle_iters`).
    for _ in 0..settle_iters {
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(25)));
    }

    // Snapshot everything the callback filled *now*, then drop the listener, so
    // no further registry event can fire into the `RefCell` while we hold a
    // borrow. The link-creation pump below (`loop_ref.iterate`) triggers globals
    // for the new Link objects; if the listener were still live and `ports` still
    // borrowed, its `borrow_mut()` would panic ("RefCell already borrowed").
    let ports: Vec<Port> = ports.borrow().clone();
    let factory = factory.borrow().clone();
    drop(_listener);

    let outs: Vec<&Port> = ports
        .iter()
        .filter(|(n, _, d, _)| *n == app_id && d == "out")
        .collect();
    let ins: Vec<&Port> = ports
        .iter()
        .filter(|(n, _, d, _)| *n == our_node_id && d == "in")
        .collect();

    // Drop links whose app output port is gone (pause/seek destroyed it). Removing
    // the proxy from the map drops it, which tears the (already dead) link down.
    let live_outs: std::collections::HashSet<u32> = outs.iter().map(|(_, p, _, _)| *p).collect();
    let before = links.len();
    links.retain(|out_port, _| live_outs.contains(out_port));
    if links.len() != before {
        tracing::info!(
            app_id,
            dropped = before - links.len(),
            "dropped stale app links"
        );
    }

    if outs.is_empty() || ins.is_empty() {
        return; // nothing to (re)link right now
    }
    let Some(factory) = factory else {
        return; // no link factory this pass; try again next tick
    };

    // Pair by channel name first, then positionally — but never send two app
    // outputs into the same input.
    //
    // The registry hands ports back in arrival order, not channel order, so
    // enumeration index is not a channel index: pairing on it could put FR into
    // our FL. Sort both sides by channel name so "positional" means the same
    // thing on each, and fall back to declaration order only for ports that
    // carry no channel label at all.
    let mut outs = outs;
    let mut ins = ins;
    outs.sort_by(|a, b| a.3.cmp(&b.3));
    ins.sort_by(|a, b| a.3.cmp(&b.3));

    // Inputs already spoken for: the ones surviving links point at, plus any
    // claimed earlier in this pass. Without this a clamped positional fallback
    // (`min(ins.len() - 1)`) silently piles every output onto the last input —
    // one ear gets both channels, the other gets nothing.
    //
    // The claim is read back out of `links` rather than tracked alongside it:
    // a second registry would have to be pruned in lockstep with the retain
    // above, and any drift between the two reintroduces exactly this bug.
    let mut taken: std::collections::HashSet<u32> =
        links.values().map(|(input_port, _)| *input_port).collect();

    // Create links for app output ports not already linked.
    let mut created = 0;
    for (i, out) in outs.iter().enumerate() {
        if links.contains_key(&out.1) {
            continue; // already linked this port
        }
        // Prefer a free same-channel input (FL→FL), else the next free one.
        let Some(input) = ins
            .iter()
            .find(|(_, p, _, ch)| !ch.is_empty() && *ch == out.3 && !taken.contains(p))
            .or_else(|| ins.iter().filter(|(_, p, _, _)| !taken.contains(p)).nth(0))
            .or_else(|| ins.get(i.min(ins.len() - 1)))
        else {
            continue;
        };
        taken.insert(input.1);
        match core.create_object::<pipewire::link::Link>(
            &factory,
            &pipewire::properties::properties! {
                "link.output.node" => out.0.to_string().as_str(),
                "link.output.port" => out.1.to_string().as_str(),
                "link.input.node" => input.0.to_string().as_str(),
                "link.input.port" => input.1.to_string().as_str(),
            },
        ) {
            Ok(link) => {
                links.insert(out.1, (input.1, link));
                created += 1;
            }
            Err(e) => tracing::debug!(app_id, error = %e, "create link failed"),
        }
    }
    if created > 0 {
        tracing::info!(app_id, created, "re-linked app output ports to capture");
        // Roundtrip so the server actually creates them.
        for _ in 0..10 {
            loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(25)));
        }
    }
}

// ---------------------------------------------------------------------------
// Playback (sink) side
// ---------------------------------------------------------------------------

/// Target for the jitter buffer to fill before playback starts, in bytes.
///
/// A few chunks' worth (~20-30ms at 256-frame chunks) absorbs network jitter so
/// a late packet doesn't underrun. Small on purpose — audio favors freshness,
/// so we buffer only enough to stay gap-free, not to hide large delays.
const JITTER_PREFILL_BYTES: usize = 4 * 256 * BYTES_PER_FRAME;

/// Hard ceiling on the jitter buffer: ~200 ms of audio.
///
/// The queue used to be unbounded, which turns a *rate* mismatch into unbounded
/// *latency*: if chunks arrive even slightly faster than the device drains them
/// — or arrive in a burst after the sender was briefly over budget — the excess
/// simply accumulates and every later sound plays that much later. It never
/// recovers on its own, because nothing ever removes the backlog. The user hit
/// close to a **minute** of delay this way.
///
/// Past this point the OLDEST audio is discarded, not the newest: for live
/// audio, freshness beats completeness — a listener would rather lose 200 ms
/// that is already stale than hear everything a minute behind. This is the same
/// trade the kernel-side ring makes (see CursioRing.h).
const JITTER_MAX_BYTES: usize = 48_000 / 5 * BYTES_PER_FRAME;

/// Whether `feed` already holds a full jitter buffer's worth of undrained audio,
/// so the next chunk should be dropped rather than enqueued.
///
/// Split out of [`PwSink::play`] only so it is reachable from tests without a
/// live PipeWire daemon — `play` calls exactly this.
fn queue_is_full(queued: &AtomicUsize) -> bool {
    queued.load(Ordering::Relaxed) > JITTER_MAX_BYTES
}

/// A running PipeWire playback node (the **sink** side).
///
/// Inbound [`AudioChunk`]s are pushed into a byte queue drained by the PipeWire
/// `process` callback on its own thread. The queue is the jitter buffer: it
/// prefills a little before output begins, then feeds each buffer PipeWire asks
/// for, emitting silence on underrun rather than blocking.
pub struct PwSink {
    id: AudioStreamId,
    /// Samples to play, pushed from the async side, drained on the PW thread.
    feed: std_mpsc::Sender<Vec<u8>>,
    /// Logical gain (linear, 1.0 = unity) applied to every sample as the PW
    /// thread fills the output buffer. Stored as the bit pattern of an `f32` in
    /// an atomic so the async side can update it (`set_gain`) without a lock —
    /// the process callback is real-time and must never block. See
    /// [`Message::AudioGain`](cursio_protocol::Message::AudioGain).
    gain: Arc<AtomicU32>,
    /// Bytes currently sitting in `feed` and not yet moved into the jitter
    /// buffer. Incremented by [`play`](PwSink::play), decremented on the PW
    /// thread as chunks are drained.
    ///
    /// This exists because [`JITTER_MAX_BYTES`] can only be enforced *inside*
    /// the process callback, and a node PipeWire never drives never runs that
    /// callback. A virtual `Audio/Source` with no application recording from it
    /// is exactly that case: the peer keeps sending microphone audio, `play`
    /// keeps accepting it, and nothing ever trims — 48 kHz stereo f32 accrues
    /// ~1.4 GB/hour per silent-but-fed stream and never comes back. Counting
    /// here bounds the queue whether or not the graph is draining it.
    queued: Arc<AtomicUsize>,
    /// Whether anything is actually consuming this node right now.
    ///
    /// PipeWire drives a stream only while something is linked to it, which it
    /// reports as `Streaming` (and `Paused` when the last consumer goes away).
    /// The node stays published in the graph either way, so a device the user
    /// can see in their mixer is NOT necessarily one anybody is listening to.
    ///
    /// Publishing 13 virtual microphones eagerly meant the peer streamed all of
    /// them from the moment it connected, whether or not a single application
    /// had them open — which is what made the first seconds of a session drop
    /// audio (the sender's backlog trim discarding chunks it could not keep up
    /// with) and what filled the undrained queues behind [`Self::queued`].
    listening: Arc<AtomicBool>,
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PwSink {
    /// Open a playback node for a stream. Creates a virtual output that other
    /// tools (VoiceMeeter/Carla, or just the default sink) can route; audio
    /// pushed via [`play`](PwSink::play) comes out here.
    ///
    /// `node_name` names the node in the graph — e.g. `"Cursio: Spotify (PC)"`
    /// for a per-app stream — so it is addressable/routable on its own.
    pub fn open(id: AudioStreamId, node_name: String) -> Result<Self, AudioError> {
        Self::open_as(id, node_name, false)
    }

    /// Open as a virtual **microphone**: the node is published with
    /// `media.class = Audio/Source`, so local apps can select it as a recording
    /// device and hear what the peer's microphone picks up.
    pub fn virtual_source(id: AudioStreamId, node_name: String) -> Result<Self, AudioError> {
        Self::open_as(id, node_name, true)
    }

    /// Shared body of [`open`](PwSink::open) and
    /// [`virtual_source`](PwSink::virtual_source) — the two differ only in
    /// whether the node is published as a source.
    fn open_as(id: AudioStreamId, node_name: String, as_source: bool) -> Result<Self, AudioError> {
        let (feed_tx, feed_rx) = std_mpsc::channel::<Vec<u8>>();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();

        // Unity gain to start; the atomic is shared with the PW thread so
        // `set_gain` takes effect on the next output buffer without a lock.
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let gain_thread = Arc::clone(&gain);
        let queued = Arc::new(AtomicUsize::new(0));
        let queued_thread = Arc::clone(&queued);
        let listening = Arc::new(AtomicBool::new(false));
        let listening_thread = Arc::clone(&listening);

        let thread = std::thread::Builder::new()
            .name(format!("cursio-pw-playback-{id}"))
            .spawn(move || {
                run_playback_loop(
                    node_name,
                    as_source,
                    feed_rx,
                    stop_rx,
                    ready_tx,
                    gain_thread,
                    queued_thread,
                    listening_thread,
                )
            })
            .map_err(|e| AudioError::Backend(format!("spawn playback thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(PwSink {
                id,
                feed: feed_tx,
                gain,
                queued,
                listening,
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(AudioError::Backend(e)),
            Err(_) => Err(AudioError::Backend(
                "playback thread exited before signalling readiness".into(),
            )),
        }
    }

    /// Enqueue one inbound chunk for playback (into the jitter buffer).
    ///
    /// Drops the chunk instead of enqueuing it once more than
    /// [`JITTER_MAX_BYTES`] is already waiting. The process callback applies the
    /// same ceiling to audio it has taken delivery of, but it only runs while
    /// the graph drives this node; the check here is what holds when it does
    /// not (see [`PwSink::queued`]). Dropping the newest chunk rather than the
    /// oldest is deliberate: the backlog ahead of it is what plays next, and a
    /// caller with nowhere to put audio should not be able to grow this queue.
    pub fn play(&self, chunk: &AudioChunk) -> Result<(), AudioError> {
        if queue_is_full(&self.queued) {
            return Ok(());
        }
        let len = chunk.data.len();
        self.feed
            .send(chunk.data.clone())
            .map_err(|_| AudioError::Backend("playback thread gone".into()))?;
        self.queued.fetch_add(len, Ordering::Relaxed);
        Ok(())
    }

    /// Set the logical gain (linear, 1.0 = unity) applied to this node's audio.
    ///
    /// This is the per-stream gain of the "one system" model — a property of the
    /// Stream/Route, *above* the OS mixer and separate from the device's single
    /// replicated volume. Lock-free: the value is published to the RT playback
    /// thread via an atomic, so it never blocks the process callback.
    pub fn set_gain(&self, gain: f32) {
        // Clamp to a sane, non-negative range: a runaway or negative multiplier
        // would either invert phase or blow the signal past full-scale into
        // clipping. 4.0 (+12 dB) is ample headroom for a "make it louder" knob.
        let gain = gain.clamp(0.0, 4.0);
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn id(&self) -> AudioStreamId {
        self.id
    }

    /// Whether anything is currently consuming this node — see
    /// [`listening`](Self::listening).
    pub fn is_listening(&self) -> bool {
        self.listening.load(Ordering::Relaxed)
    }
}

impl Drop for PwSink {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The PipeWire main-loop body for playback. Owns the jitter buffer (a byte
/// `VecDeque` fed from `feed_rx`) and fills each requested output buffer from it.
fn run_playback_loop(
    node_name: String,
    // Publish the node as `Audio/Source` (a virtual microphone) rather than an
    // ordinary playback stream.
    as_source: bool,
    feed_rx: std_mpsc::Receiver<Vec<u8>>,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
    // Logical gain (f32 bits), updated from the async side by `set_gain`.
    gain: Arc<AtomicU32>,
    // Bytes queued in `feed_rx` but not yet pulled into the jitter buffer; see
    // `PwSink::queued`. Decremented here as chunks are taken delivery of.
    queued: Arc<AtomicUsize>,
    // Set while something is linked to this node; see `PwSink::listening`.
    listening: Arc<AtomicBool>,
) {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    let setup = (|| -> Result<_, String> {
        let main_loop = MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
        let context = ContextRc::new(&main_loop, None).map_err(|e| format!("context: {e}"))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| format!("connect: {e}"))?;

        let mut props = properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Playback",
            *pipewire::keys::MEDIA_ROLE => "Music",
            *pipewire::keys::NODE_NAME => node_name.as_str(),
            *pipewire::keys::NODE_DESCRIPTION => node_name.as_str(),
            *pipewire::keys::NODE_LATENCY => "512/48000",
            // Mixers (pavucontrol, GNOME) label a playback *stream* by
            // application.name + media.name — NOT node.name/description. Without
            // these every stream showed up as an identical "cursio:
            // cursio-playback" row, which is useless when there are many.
            *pipewire::keys::APP_NAME => node_name.as_str(),
            *pipewire::keys::MEDIA_NAME => node_name.as_str(),
        };

        // A virtual MICROPHONE is the same playback node published as a source:
        // we write the peer's audio into it, and the system offers it to local
        // apps as a recording device. Without `media.class` it is merely a
        // playback stream and never appears in any input list — which is why the
        // peer's microphone was invisible on this machine.
        if as_source {
            props.insert(*pipewire::keys::MEDIA_CLASS, "Audio/Source");
            props.insert(*pipewire::keys::AUDIO_CHANNELS, "2");
        }

        // The stream name is also surfaced by some tools; use the real label.
        let stream =
            StreamRc::new(core, node_name.as_str(), props).map_err(|e| format!("stream: {e}"))?;
        Ok((main_loop, stream))
    })();

    let (main_loop, stream) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // Jitter buffer: raw bytes waiting to be played, and whether we've prefilled
    // enough to start (so the first underrun-y moments don't stutter).
    let jitter: RefCell<VecDeque<u8>> = RefCell::new(VecDeque::new());
    let started = std::cell::Cell::new(false);
    // Start/underrun counters, logged on each transition. Cheap enough for the
    // RT callback (a Cell bump plus a `debug!` that costs nothing when the level
    // is off) and they turn "it stutters for the first few seconds" into a
    // timestamped count of exactly how often playback stopped and restarted.
    let starts = std::cell::Cell::new(0u32);
    let underruns = std::cell::Cell::new(0u32);
    let node_label = node_name.clone();

    let stride = BYTES_PER_FRAME;

    let listening_cb = Arc::clone(&listening);
    let state_label = node_name.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_stream, (), _old, new| {
            // `Streaming` means something is linked and pulling audio; anything
            // else means nobody is listening, however visible the node remains.
            let now_listening = matches!(new, pipewire::stream::StreamState::Streaming);
            if listening_cb.swap(now_listening, Ordering::Relaxed) != now_listening {
                tracing::info!(
                    node = %state_label,
                    listening = now_listening,
                    "playback node consumer state changed"
                );
            }
        })
        .process(move |stream, ()| {
            // Pull any newly arrived chunks into the jitter buffer first.
            {
                let mut jb = jitter.borrow_mut();
                while let Ok(bytes) = feed_rx.try_recv() {
                    queued.fetch_sub(bytes.len(), Ordering::Relaxed);
                    jb.extend(bytes);
                }
                // Cap the backlog: drop the OLDEST audio, keep the newest.
                //
                // Without this a sustained arrival-rate surplus becomes permanent
                // latency — the queue only ever grows, so playback falls further
                // behind for as long as the session lasts. Trimming here bounds
                // the delay to JITTER_MAX_BYTES no matter what the sender does.
                if jb.len() > JITTER_MAX_BYTES {
                    let excess = jb.len() - JITTER_MAX_BYTES;
                    // Drop whole frames only, so the stream stays sample-aligned
                    // and channels don't swap.
                    let excess = excess - (excess % BYTES_PER_FRAME);
                    if excess > 0 {
                        jb.drain(..excess);
                        tracing::debug!(
                            dropped_bytes = excess,
                            "jitter buffer over its limit; dropped stale audio to keep latency bounded"
                        );
                    }
                }
                if !started.get() && jb.len() >= JITTER_PREFILL_BYTES {
                    started.set(true);
                    // Log every start, not just the first: a stream that keeps
                    // re-starting is the buffer running dry and re-prefilling,
                    // which is heard as audio cutting in and out. Pairing this
                    // with the underrun below gives the on/off period directly.
                    starts.set(starts.get() + 1);
                    tracing::debug!(
                        node = %node_label,
                        start = starts.get(),
                        buffered = jb.len(),
                        "jitter buffer prefilled; playback starting"
                    );
                }
            }

            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let Some(slice) = data.data() else { return };

            // How many whole frames fit in the buffer PipeWire gave us.
            let cap_frames = slice.len() / stride;
            let want_bytes = cap_frames * stride;

            let mut jb = jitter.borrow_mut();
            let filled = if started.get() {
                let take = want_bytes.min(jb.len());
                for (i, byte) in jb.drain(..take).enumerate() {
                    slice[i] = byte;
                }
                // Apply the logical gain to the samples we just wrote. Read the
                // atomic once per buffer (not per sample) — it changes at knob
                // speed, not audio speed. Skip the multiply at unity so the
                // common case stays a plain copy; the samples are f32LE, so we
                // scale each 4-byte lane in place over the frames we filled.
                let g = f32::from_bits(gain.load(Ordering::Relaxed));
                if (g - 1.0).abs() > f32::EPSILON {
                    for s in slice[..take].chunks_exact_mut(4) {
                        let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]) * g;
                        s.copy_from_slice(&v.to_le_bytes());
                    }
                }
                take
            } else {
                0
            };
            // Zero-fill the remainder (underrun / not started yet) so we output
            // silence rather than garbage or a stall.
            for b in slice.iter_mut().take(want_bytes).skip(filled) {
                *b = 0;
            }
            // If we drained the buffer dry after starting, re-arm prefill so a
            // sustained gap doesn't produce choppy single-frame outputs.
            if started.get() && jb.is_empty() {
                started.set(false);
                underruns.set(underruns.get() + 1);
                // `filled` says whether this buffer was satisfied or padded with
                // silence, which separates "arrived just in time" from a real gap.
                tracing::debug!(
                    node = %node_label,
                    underrun = underruns.get(),
                    filled_bytes = filled,
                    wanted_bytes = want_bytes,
                    "jitter buffer ran dry; muting until it prefills again"
                );
            }

            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as _;
            *chunk.size_mut() = want_bytes as _;
        })
        .register();

    let pod_bytes = format_param_pod();
    let mut params = [Pod::from_bytes(&pod_bytes).expect("valid format pod")];
    if let Err(e) = stream.connect(
        Direction::Output,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    ) {
        let _ = ready_tx.send(Err(format!("stream connect: {e}")));
        return;
    }

    let _ = ready_tx.send(Ok(()));

    let loop_ref = main_loop.loop_();
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipewire::spa::{pod::deserialize::PodDeserializer, sys::SPA_FORMAT_AUDIO_position};

    /// Our streams must declare FL/FR positions, not just "2 channels".
    ///
    /// `AudioInfoRaw::new()` starts UNPOSITIONED and `set_channels` does not
    /// clear that flag, so a stereo format is easy to build that carries no
    /// layout. Ports then come up with an empty `audio.channel`, `relink_app`
    /// cannot match FL→FL, and the graph has no basis to keep left and right
    /// apart — heard as everything arriving in one ear.
    #[test]
    fn format_pod_declares_channel_positions() {
        let pod = format_param_pod();
        let parsed = PodDeserializer::deserialize_any_from(&pod)
            .expect("pod parses")
            .1;

        let Value::Object(obj) = parsed else {
            panic!("format pod should be an object");
        };
        let position = obj
            .properties
            .iter()
            .find(|p| p.key == SPA_FORMAT_AUDIO_position)
            .expect("format pod must carry a channel position array");

        let Value::ValueArray(ValueArray::Id(ref ids)) = position.value else {
            panic!(
                "position should be an array of ids, got {:?}",
                position.value
            );
        };
        assert_eq!(
            ids.iter().map(|id| id.0).collect::<Vec<_>>(),
            vec![SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR],
            "stereo must be declared as front-left + front-right"
        );
    }

    /// A device has ONE volume, identical everywhere (docs/audio-design.md
    /// invariant #2). The cubic mapping is how our linear 0..1 stays the SAME
    /// number pavucontrol shows, so it must round-trip cleanly.
    #[test]
    fn cubic_linear_round_trips() {
        for &v in &[0.0f32, 0.1, 0.25, 0.42, 0.5, 0.8, 1.0] {
            let back = cubic_to_linear(linear_to_cubic(v));
            assert!((back - v).abs() < 1e-4, "{v} round-tripped to {back}");
        }
    }

    /// Out-of-range volumes are clamped, never wrapped or left to blow past
    /// full-scale.
    #[test]
    fn cubic_linear_clamps() {
        assert_eq!(linear_to_cubic(-1.0), 0.0);
        assert_eq!(linear_to_cubic(2.0), 1.0);
        assert_eq!(cubic_to_linear(-1.0), 0.0);
        assert_eq!(cubic_to_linear(2.0), 1.0);
    }

    /// The Props POD we build must be the exact one we parse back — this guards
    /// the pod format (keys, array-of-float volumes, bool mute) end to end, and
    /// specifically the earlier trap where a `match` on non-UPPER_SNAKE SPA
    /// constants silently matched everything.
    #[test]
    fn props_pod_round_trips() {
        let bytes = props_pod(2, Some(linear_to_cubic(0.42)), Some(true));
        let pod = Pod::from_bytes(&bytes).expect("valid pod");
        let (volume, mute) = parse_props(pod).expect("parses");
        assert!((volume - 0.42).abs() < 1e-3, "volume {volume} != 0.42");
        assert!(mute, "mute should be true");
    }

    /// A volume-only Props update parses with mute defaulted, and a mute-only one
    /// parses with volume defaulted — the monitor's dedup merges the halves, but
    /// each must decode on its own without dropping the update.
    #[test]
    fn props_pod_partial_updates_decode() {
        let vol_bytes = props_pod(2, Some(linear_to_cubic(0.7)), None);
        let vol_only = Pod::from_bytes(&vol_bytes).unwrap();
        let (v, m) = parse_props(vol_only).expect("volume-only parses");
        assert!((v - 0.7).abs() < 1e-3);
        assert!(!m, "mute defaults false when absent");

        let mute_bytes = props_pod(2, None, Some(true));
        let mute_only = Pod::from_bytes(&mute_bytes).unwrap();
        let (v, m) = parse_props(mute_only).expect("mute-only parses");
        assert_eq!(v, 1.0, "volume defaults to unity when absent");
        assert!(m);
    }

    /// A sink nobody drains must stop accepting audio.
    ///
    /// `JITTER_MAX_BYTES` is enforced in the process callback, which only runs
    /// while PipeWire drives the node. A virtual `Audio/Source` with no
    /// application recording from it is never driven, so before this check the
    /// peer's microphone audio accumulated in the feed channel for the life of
    /// the session — measured at 35 GB over ~24 h on one stream.
    #[test]
    fn undrained_queue_stops_accepting_audio() {
        let queued = AtomicUsize::new(0);
        assert!(!queue_is_full(&queued), "an empty queue accepts audio");

        // Simulate `play` filling the channel while nothing drains it.
        queued.store(JITTER_MAX_BYTES, Ordering::Relaxed);
        assert!(
            !queue_is_full(&queued),
            "a queue exactly at the ceiling still accepts"
        );

        queued.fetch_add(1, Ordering::Relaxed);
        assert!(
            queue_is_full(&queued),
            "past the ceiling the sink must drop, not grow without bound"
        );

        // Once the callback drains, the sink takes audio again.
        queued.store(0, Ordering::Relaxed);
        assert!(!queue_is_full(&queued), "draining re-opens the queue");
    }

    /// The ceiling must be a live-audio buffer, not an unbounded backlog.
    #[test]
    fn jitter_ceiling_is_bounded_and_small() {
        let seconds = JITTER_MAX_BYTES as f64 / (48_000.0 * BYTES_PER_FRAME as f64);
        assert!(
            (0.05..=1.0).contains(&seconds),
            "jitter ceiling should be a fraction of a second, got {seconds}s"
        );
    }
}
