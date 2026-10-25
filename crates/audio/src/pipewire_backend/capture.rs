//! Capturing on PipeWire: a device's monitor, one application's
//! playback, or a microphone, chunked onto the wire format.

use crate::pipewire_backend::app_links::relink_app;
use crate::pipewire_backend::app_links::AppLinks;
use crate::pipewire_backend::app_links::PortWatch;
use crate::pipewire_backend::app_links::APP_LINK_PATIENCE;
use crate::pipewire_backend::app_links::APP_RELINK_INTERVAL;
use crate::pipewire_backend::format_param_pod;
use crate::pipewire_backend::wire_format;
use crate::pipewire_backend::BYTES_PER_FRAME;
use crate::worker::{OnDrop, Worker};
use crate::AudioError;
use cursio_protocol::{AudioChunk, AudioCodec, AudioStreamId, AudioStreamMeta, StreamRole};
use pipewire::{
    context::ContextRc,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{pod::Pod, utils::Direction},
    stream::{StreamFlags, StreamRc},
};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

/// Handle to a running PipeWire capture stream (the **source** side).
///
/// Spawns a thread running the PipeWire loop; captured s16 buffers are chunked
/// into [`AudioChunk`]s and forwarded on the tokio channel the
/// [`AudioSource`](crate::AudioSource) trait exposes.
pub struct PwSource {
    // First, so the thread is signalled before its channels close.
    _worker: Worker,
    id: AudioStreamId,
    adverts_rx: tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta>,
    chunks_rx: tokio::sync::mpsc::UnboundedReceiver<AudioChunk>,
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
    /// `target` is the source node name from [`list_input_devices`](crate::list_input_devices); `None`
    /// records the system default input.
    pub fn mic_tap(id: AudioStreamId, target: Option<String>) -> Result<Self, AudioError> {
        Self::with_mode(id, CaptureMode::MicTap { target })
    }

    /// Start capturing **one application's** audio, by its PipeWire node id (as
    /// returned by [`list_app_streams`](crate::list_app_streams)). The stream is advertised with
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

        let worker = Worker::start(
            format!("cursio-pw-capture-{id}"),
            "capture thread",
            OnDrop::Detach,
            move |stop_rx, ready_tx| run_capture_loop(id, mode, chunks_tx, stop_rx, ready_tx),
        )?;
        Ok(PwSource {
            id,
            adverts_rx,
            chunks_rx,
            _worker: worker,
        })
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
pub(super) fn run_capture_loop(
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
                // headphones — the mic came back into the user's own ears.
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
    // Crucially this is **not one-shot**: browsers (Chromium, Firefox) can tear
    // down an app's output ports and create fresh ones — often on the SAME node
    // id, so no Joined/Left fires and the Remote Stream is never rebuilt. Links to
    // the old ports are then dead and audio is silent until we relink.
    //
    // The ports are followed through ONE registry listener that lives as long as
    // the tap (see [`PortWatch`]). Each pass used to bind a fresh registry and
    // pump the loop for ~100 ms hoping the full dump had arrived. On a busy graph
    // it often had not, so ports that were perfectly alive were missing from the
    // pass, their links were judged stale and dropped, and the next pass ~450 ms
    // later linked them again: Firefox, with ports unchanged for hours, was cut
    // out of the capture that way dozens of times an hour, heard as a pop and a
    // gap on the peer. A port now leaves the table only when PipeWire says it was
    // removed, and a relink runs the moment the table changes.
    let app_id: u32 = if let CaptureMode::AppTap { node_id, .. } = &mode {
        node_id.parse().unwrap_or(u32::MAX)
    } else {
        u32::MAX
    };
    let watch = if app_id != u32::MAX {
        match PortWatch::new(&core, app_id) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::error!(app_id, error = %e, "cannot watch app ports; tap stays unlinked");
                None
            }
        }
    } else {
        None
    };
    let mut app_links: AppLinks = std::collections::HashMap::new();
    let tap_started = std::time::Instant::now();
    let mut first_link_logged = false;
    let mut link_failure_logged = false;

    // Drive the loop, polling the stop channel between iterations. PipeWire's
    // loop has no cross-thread "run until signalled", so we iterate with a short
    // timeout and re-check. `iterate` returns as soon as it has dispatched
    // anything, so an app port appearing is linked within the same wakeup —
    // everything the app plays before its link exists is lost, which used to cut
    // the attack off short event sounds.
    let mut last_relink = std::time::Instant::now();
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(50)));
        let Some(watch) = &watch else { continue };
        watch.our_node.set(stream.node_id());
        // The periodic pass is only a backstop for a change that arrived while
        // our own node id was still unknown; with the table held in memory it
        // costs nothing.
        if watch.dirty.replace(false) || last_relink.elapsed() >= APP_RELINK_INTERVAL {
            last_relink = std::time::Instant::now();
            relink_app(&core, app_id, stream.node_id(), watch, &mut app_links);
        }
        if !first_link_logged && !app_links.is_empty() {
            first_link_logged = true;
            tracing::info!(
                app_id,
                count = app_links.len(),
                after_ms = tap_started.elapsed().as_millis() as u64,
                "linked app to capture"
            );
        }
        if !first_link_logged && !link_failure_logged && tap_started.elapsed() >= APP_LINK_PATIENCE
        {
            link_failure_logged = true;
            tracing::error!(
                app_id,
                "app tap is still unlinked; no app output port reached our input"
            );
        }
    }
}
