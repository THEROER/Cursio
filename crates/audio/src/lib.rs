//! Cursio audio sharing — a networked, per-application audio bus.
//!
//! Cursio carries audio between machines so the peer's sound appears in this
//! machine's audio graph as extra nodes, and vice versa — the
//! Mouse-Without-Borders model, for sound. It **replaces the network transport**
//! (AudioRelay + Bluetooth hops), *not* the mixer: VoiceMeeter (Windows) and
//! Carla (Linux) stay, and Cursio presents virtual devices/nodes they route in
//! their own graphs. The defining requirement is **per-application** streams:
//! each app on a peer is its own named node here, so it can be routed and
//! volume-controlled independently across machines.
//!
//! The neutral wire vocabulary lives in [`cursio_protocol::audio`]; this crate
//! is the platform layer that produces and consumes it, behind traits so
//! backends slot in the same way [`InputInjector`](cursio_input) does — Linux
//! (PipeWire: per-node capture, null-sink output, playback nodes) and Windows
//! (WASAPI render + process loopback) without touching the session code:
//!
//! - **Source** ([`AudioSource`]): enumerate/advertise local streams (devices
//!   and per-app nodes), capture each, emit [`AudioChunk`]s, and take rate-control
//!   feedback.
//! - **Sink** ([`AudioSink`]): create local nodes from a peer's advertisements,
//!   play inbound chunks through a jitter buffer, apply logical gain, and expose
//!   the nodes so VoiceMeeter/Carla can route them.
//!
//! No backend is wired yet: this milestone fixes the traits and the data flow so
//! the protocol and session integration can be built against a stable shape. See
//! `docs/audio-design.md` for the model, the latency/jitter budget, and the
//! milestones.

/// Opus compression for the wire format — used on every platform.
pub mod codec;

#[cfg(target_os = "android")]
pub mod android_backend;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
mod declick;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
mod jitter;
#[cfg(target_os = "linux")]
pub mod pipewire_backend;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
mod playback_feed;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod playout;
#[cfg(any(target_os = "windows", test))]
mod sample_clock;
/// Drives the Cursio virtual audio driver, which is what gives Windows the
/// same per-device parity Linux gets from PipeWire.
#[cfg(target_os = "windows")]
pub mod vad_client;
#[cfg(target_os = "windows")]
pub mod wasapi_backend;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod worker;

use cursio_protocol::{
    AudioChunk, AudioFrameAck, AudioStreamCloseReason, AudioStreamId, AudioStreamMeta,
};

/// Errors from an audio backend (source or sink).
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// The requested stream doesn't exist or is no longer available.
    #[error("audio stream not found or gone: {0}")]
    StreamGone(String),
    /// The platform audio facility failed or is unavailable here.
    #[error("audio backend failure: {0}")]
    Backend(String),
    /// A feature isn't implemented for this platform/backend yet.
    #[error("audio sharing not supported on this platform yet")]
    Unsupported,
}

/// Produces local audio streams to share with the peer (the **source** side).
///
/// A backend implements this to advertise and capture what this machine can
/// share — whole devices *and* individual application nodes. New streams appear
/// on the [`advertisements`](AudioSource::advertisements) channel as apps start
/// playing or devices appear; their audio flows on the
/// [`chunks`](AudioSource::chunks) channel. Delivery is by channel, not
/// callback, so a slow link never stalls the audio thread — the same pattern as
/// [`CaptureHandle`](cursio_input) for input and the window capture path.
pub trait AudioSource {
    /// Stream announcements as they appear: each is a
    /// [`Message::AudioStreamAdvertise`](cursio_protocol::Message::AudioStreamAdvertise)
    /// to send before that stream's chunks. A `Some(app_name)` meta is a per-app
    /// node; `None` is a whole-device stream.
    fn advertisements(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta>;

    /// Captured audio for all advertised streams, tagged by
    /// [`AudioChunk::id`]. One multiplexed channel keeps ordering simple; the
    /// consumer routes by id.
    fn chunks(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioChunk>;

    /// Feed back the peer's latest [`AudioFrameAck`] so the encoder can adapt its
    /// Opus bitrate to the link (degrade rather than buffer unboundedly). A
    /// backend may ignore this during bring-up; the plumbing is here from the
    /// start so adaptation needs no protocol change later.
    fn on_ack(&mut self, ack: AudioFrameAck);

    /// Stop sharing a stream (its app stopped, or we choose to drop it).
    fn close_stream(&mut self, id: AudioStreamId, reason: AudioStreamCloseReason);
}

/// Plays streams advertised by the peer (the **sink** side).
///
/// A backend implements this to render the peer's audio locally. For each
/// advertised stream it creates a node — a *distinct named node per app*, so
/// VoiceMeeter/Carla and the volume knob can address each independently — and
/// plays that stream's chunks through a small jitter buffer. It also reports the
/// acks the source needs for rate control, and applies logical gain.
pub trait AudioSink {
    /// Create a local node for a newly advertised stream. For a per-app meta
    /// (`app_name = Some`) this should be its own named node.
    fn open_stream(&mut self, meta: &AudioStreamMeta) -> Result<(), AudioError>;

    /// Enqueue one inbound chunk for playback (into the stream's jitter buffer).
    fn play_chunk(&mut self, chunk: &AudioChunk) -> Result<(), AudioError>;

    /// Apply a logical gain (linear, 1.0 = unity) to a stream's node — the knob's
    /// per-app/master volume, applied above the OS mixer.
    fn set_gain(&mut self, id: AudioStreamId, gain: f32) -> Result<(), AudioError>;

    /// The ack this sink wants to send back for rate control, if one is due — the
    /// highest `seq` it has played for some stream since the last ack. Returns
    /// `None` when nothing new is worth reporting, so acks stay sparse (a few per
    /// second), not one per chunk. See [`AudioFrameAck`].
    fn take_ack(&mut self) -> Option<AudioFrameAck>;

    /// Tear down a stream's node (source stopped, or we dropped it).
    fn close_stream(&mut self, id: AudioStreamId, reason: AudioStreamCloseReason);
}

/// One local audio output device, as seen by this machine.
///
/// Cursio mirrors these to the peer so each of the peer's real devices shows up
/// there as its own selectable device — not just one global "remote output".
/// `id` is the platform's addressable handle (Linux: the PipeWire node name;
/// Windows: the endpoint's friendly name, matched case-insensitively) and is
/// what the far side passes back to target *this exact* device. `description`
/// is the human name to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDevice {
    /// Platform-addressable identifier used to open this device.
    pub id: String,
    /// Human-readable name, e.g. "Realtek High Definition Audio".
    pub description: String,
}

/// List this machine's audio output devices, for mirroring to the peer.
///
/// Cursio's *own* mirror devices are filtered out, so mirroring never feeds
/// itself (a mirror of a mirror would loop audio between the machines).
pub fn list_output_devices() -> Result<Vec<OutputDevice>, AudioError> {
    #[cfg(target_os = "linux")]
    {
        pipewire_backend::list_output_devices()
    }
    #[cfg(target_os = "windows")]
    {
        wasapi_backend::list_output_devices()
    }
    #[cfg(target_os = "android")]
    {
        android_backend::list_output_devices()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
    {
        Err(AudioError::Unsupported)
    }
}

/// List this machine's audio **input** devices (microphones, line-ins), so the
/// peer can use one of them as its own recording source.
///
/// The mirror image of [`list_output_devices`], and filtered the same way: our
/// own virtual microphones are skipped so a mirror is never mirrored back.
pub fn list_input_devices() -> Result<Vec<OutputDevice>, AudioError> {
    #[cfg(target_os = "linux")]
    {
        pipewire_backend::list_input_devices()
    }
    #[cfg(target_os = "windows")]
    {
        wasapi_backend::list_input_devices()
    }
    #[cfg(target_os = "android")]
    {
        android_backend::list_input_devices()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
    {
        Err(AudioError::Unsupported)
    }
}

/// One application currently playing audio on this machine.
///
/// The unit of **per-app sharing**: each of these becomes its own stream to the
/// peer, so the peer sees "Cursio: Spotify" as a separate node it can route and
/// volume-control independently — the thing a merged system stream can't offer.
///
/// `id` identifies the *stream*, not the app: one application can have several
/// concurrent streams (two browser tabs), and they must stay distinguishable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppStream {
    /// Platform handle for this specific stream (Linux: PipeWire node id;
    /// Windows: process id).
    pub id: String,
    /// Human-readable application name, e.g. "Firefox".
    pub app_name: String,
}

/// List applications currently playing audio, for per-app sharing.
///
/// Cursio's own streams are skipped so we never capture our own playback.
pub fn list_app_streams() -> Result<Vec<AppStream>, AudioError> {
    #[cfg(target_os = "linux")]
    {
        pipewire_backend::list_app_streams()
    }
    #[cfg(target_os = "windows")]
    {
        wasapi_backend::list_app_streams()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(AudioError::Unsupported)
    }
}

/// An application's own volume in the Windows mixer, by process name, and
/// setting it: what a player's volume is on Windows, whose media controls
/// expose none.
#[cfg(target_os = "windows")]
pub use wasapi_backend::{app_volume, set_app_volume};

/// Marker in a device name that identifies one of Cursio's own mirrors, so
/// enumeration can skip them (see [`list_output_devices`]).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) const MIRROR_MARKER: &str = "cursio";

// ---------------------------------------------------------------------------
// Platform-agnostic convenience wrappers
// ---------------------------------------------------------------------------
//
// A thin whole-device capture/playback façade that hides the `#[cfg]` split, so
// callers (the CLI's audio-share path, tests) get one type per role and the
// right backend is picked at compile time. This is the minimal surface the
// first vertical slice needs; the richer trait-based multi-stream API above is
// what per-app milestones build on.

/// A running whole-device capture, on whichever backend this platform has.
pub struct Capture {
    #[cfg(target_os = "linux")]
    inner: pipewire_backend::PwSource,
    #[cfg(target_os = "windows")]
    inner: WindowsCapture,
    #[cfg(target_os = "android")]
    inner: android_backend::AndroidSource,
}

/// Windows has two capture backends: WASAPI (loopback / process loopback) and
/// the Cursio virtual audio driver, which is the only one that can publish a
/// real output endpoint. They expose the same three accessors, so this enum
/// just dispatches between them.
#[cfg(target_os = "windows")]
enum WindowsCapture {
    Wasapi(wasapi_backend::WasapiSource),
    Vad(vad_client::VadSource),
}

#[cfg(target_os = "windows")]
impl WindowsCapture {
    fn advertisements(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta> {
        match self {
            WindowsCapture::Wasapi(s) => s.advertisements(),
            WindowsCapture::Vad(s) => s.advertisements(),
        }
    }

    fn chunks(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioChunk> {
        match self {
            WindowsCapture::Wasapi(s) => s.chunks(),
            WindowsCapture::Vad(s) => s.chunks(),
        }
    }
}

impl Capture {
    /// Start capturing this machine's default output (what's playing) as one
    /// whole-device stream with the given id. `target` is a backend hint (Linux:
    /// a monitor node name like `"virtual_speaker.monitor"`; ignored on Windows,
    /// which always taps the default render device's loopback).
    #[allow(unused_variables)]
    pub fn start(id: AudioStreamId, target: Option<String>) -> Result<Self, AudioError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Capture {
                inner: pipewire_backend::PwSource::start(id, target)?,
            })
        }
        #[cfg(target_os = "windows")]
        {
            Ok(Capture {
                inner: WindowsCapture::Wasapi(wasapi_backend::WasapiSource::start(id)?),
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Err(AudioError::Unsupported)
        }
    }

    /// Start as a **virtual output device** representing the peer: the local OS
    /// lists `description` as a switchable output, and whatever is routed to it
    /// is captured and (by the caller) streamed to the peer, which plays it on
    /// its real output. This is how the peer's speakers appear as a native
    /// device here.
    ///
    /// Linux creates a real `Audio/Sink` node. **Windows has no user-space way
    /// to create an output endpoint** (that needs a virtual audio driver), so
    /// there it falls back to loopback-capturing the default device and logs a
    /// warning — the peer still hears this machine's output, but it isn't yet a
    /// separately selectable device in the Windows output list.
    #[allow(unused_variables)]
    pub fn virtual_sink(
        id: AudioStreamId,
        name: String,
        description: String,
    ) -> Result<Self, AudioError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Capture {
                inner: pipewire_backend::PwSource::virtual_sink(id, name, description)?,
            })
        }
        #[cfg(target_os = "windows")]
        {
            // Windows can't publish an output endpoint from user space, so this
            // goes through the Cursio virtual audio driver, which creates one
            // native device per peer endpoint — the same parity PipeWire gives
            // us on Linux.
            //
            // There is deliberately NO fallback when the driver is absent. The
            // old one (loopback-capturing the default device) was actively
            // harmful: every "mirror" captured the SAME full system mix, so N
            // mirrors sent N copies of everything and the peer's per-app
            // separation collapsed into one blob. Unsupported is the honest
            // answer; install the driver for real mirrors.
            match vad_client::VadSource::virtual_sink(id, name, description) {
                Ok(inner) => Ok(Capture {
                    inner: WindowsCapture::Vad(inner),
                }),
                Err(AudioError::Unsupported) => {
                    tracing::warn!(
                        "virtual output devices need the Cursio virtual audio driver, which \
                         isn't installed; the peer's speakers won't appear as a selectable \
                         Windows output"
                    );
                    Err(AudioError::Unsupported)
                }
                Err(e) => Err(e),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Err(AudioError::Unsupported)
        }
    }

    /// Capture **one application's** audio, so the peer can route and
    /// volume-control that app on its own — the point of per-app sharing.
    ///
    /// `stream_id` is the platform handle from [`list_app_streams`] (Linux: the
    /// PipeWire node id; Windows: the process id). Linux taps the app's stream
    /// directly; Windows uses WASAPI **process loopback** (Win10 20348+/Win11).
    #[allow(unused_variables)]
    pub fn app_tap(
        id: AudioStreamId,
        stream_id: String,
        app_name: String,
    ) -> Result<Self, AudioError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Capture {
                inner: pipewire_backend::PwSource::app_tap(id, stream_id, app_name)?,
            })
        }
        #[cfg(target_os = "windows")]
        {
            let pid: u32 = stream_id
                .parse()
                .map_err(|_| AudioError::Backend(format!("bad pid {stream_id:?}")))?;
            Ok(Capture {
                inner: WindowsCapture::Wasapi(wasapi_backend::WasapiSource::app_tap(
                    id, pid, app_name,
                )?),
            })
        }
        // The phone's whole playback is its one routable "application"; see
        // `android_backend::PHONE_PLAYBACK_NODE`.
        #[cfg(target_os = "android")]
        {
            if stream_id != android_backend::PHONE_PLAYBACK_NODE.to_string() {
                return Err(AudioError::StreamGone(format!("app stream {stream_id}")));
            }
            Ok(Capture {
                inner: android_backend::AndroidSource::playback_tap(id)?,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
        {
            Err(AudioError::Unsupported)
        }
    }

    /// Record one of this machine's **microphones**, so the peer can use it as
    /// its own input.
    ///
    /// `target` is a device id from [`list_input_devices`]; `None` records the
    /// system default input. The resulting stream is advertised with
    /// [`StreamRole::Capture`](cursio_protocol::StreamRole::Capture), which is
    /// how the peer knows to publish a virtual *microphone* rather than a
    /// virtual speaker.
    #[allow(unused_variables)]
    pub fn mic_tap(id: AudioStreamId, target: Option<String>) -> Result<Self, AudioError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Capture {
                inner: pipewire_backend::PwSource::mic_tap(id, target)?,
            })
        }
        #[cfg(target_os = "windows")]
        {
            Ok(Capture {
                inner: WindowsCapture::Wasapi(wasapi_backend::WasapiSource::mic_tap(id, target)?),
            })
        }
        // One input to address; the system picks which microphone it is.
        #[cfg(target_os = "android")]
        {
            Ok(Capture {
                inner: android_backend::AndroidSource::mic_tap(id)?,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
        {
            Err(AudioError::Unsupported)
        }
    }

    /// The stream advertisement to send before any chunk.
    pub fn advertisements(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta> {
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
        {
            self.inner.advertisements()
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
        {
            unreachable!("unsupported platforms cannot construct Capture")
        }
    }

    /// Captured audio chunks.
    pub fn chunks(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioChunk> {
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
        {
            self.inner.chunks()
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
        {
            unreachable!("unsupported platforms cannot construct Capture")
        }
    }

    /// Stop this capture and release its platform stream immediately.
    pub fn stop(self) {}
}

/// A running whole-device playback node, on whichever backend this platform has.
pub struct Playback {
    #[cfg(target_os = "linux")]
    inner: pipewire_backend::PwSink,
    #[cfg(target_os = "windows")]
    inner: wasapi_backend::WasapiSink,
    #[cfg(target_os = "android")]
    inner: android_backend::AndroidSink,
}

impl Playback {
    /// Open a playback node named `node_name` (used as the graph node name on
    /// Linux; Windows renders to the default device).
    pub fn open(id: AudioStreamId, node_name: String) -> Result<Self, AudioError> {
        Self::open_on(id, node_name, None)
    }

    /// Open playback on the output `device` names — its id as
    /// [`list_output_devices`] gives it — on every platform, Linux included,
    /// where [`open_on`](Playback::open_on) leaves the node to the session
    /// manager. `None` is the default output.
    pub fn open_to(
        id: AudioStreamId,
        node_name: String,
        device: Option<String>,
    ) -> Result<Self, AudioError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Playback {
                inner: pipewire_backend::PwSink::open_to(id, node_name, device)?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::open_on(id, node_name, device)
        }
    }

    /// Open playback, optionally targeting a specific output device by friendly
    /// name (`device`). On **Windows**, `Some(name)` renders the peer's audio
    /// into the matching endpoint (e.g. a VB-Audio cable) — how a Linux device
    /// is represented as a controllable device on Windows. On **Linux**,
    /// `device` is ignored (the node itself *is* the device, via `node_name`).
    #[allow(unused_variables)]
    pub fn open_on(
        id: AudioStreamId,
        node_name: String,
        device: Option<String>,
    ) -> Result<Self, AudioError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Playback {
                inner: pipewire_backend::PwSink::open(id, node_name)?,
            })
        }
        #[cfg(target_os = "windows")]
        {
            let inner = match device {
                Some(name) => wasapi_backend::WasapiSink::open_device(id, name, node_name.clone())?,
                None => wasapi_backend::WasapiSink::open(id, node_name.clone())?,
            };
            Ok(Playback { inner })
        }
        // Whatever the system routes media to; there is no device to pick.
        #[cfg(target_os = "android")]
        {
            Ok(Playback {
                inner: android_backend::AndroidSink::open(id, node_name)?,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
        {
            Err(AudioError::Unsupported)
        }
    }

    /// Publish a virtual **microphone** carrying the peer's input.
    ///
    /// Where [`open_on`](Playback::open_on) creates something that *plays*,
    /// this creates something local apps can *record from*: the peer's
    /// microphone shows up in this machine's input list, so a call app or
    /// recorder can select it like any other source.
    ///
    /// Linux publishes an `Audio/Source` node directly. Windows cannot create
    /// an endpoint from user space, so it needs the Cursio virtual audio driver
    /// (see docs/audio-design.md) — until that path is wired up here it reports
    /// [`AudioError::Unsupported`] rather than silently doing something else.
    #[allow(unused_variables)]
    pub fn virtual_source(id: AudioStreamId, node_name: String) -> Result<Self, AudioError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Playback {
                inner: pipewire_backend::PwSink::virtual_source(id, node_name)?,
            })
        }
        #[cfg(target_os = "windows")]
        {
            let _ = (id, &node_name);
            Err(AudioError::Unsupported)
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Err(AudioError::Unsupported)
        }
    }

    /// Enqueue one inbound chunk for playback.
    pub fn play(&self, chunk: &AudioChunk) -> Result<(), AudioError> {
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
        {
            self.inner.play(chunk)
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
        {
            let _ = chunk;
            Err(AudioError::Unsupported)
        }
    }

    /// Whether anything on this machine is actually consuming this node.
    ///
    /// Lets the session ask the peer to stop sending a stream nobody is
    /// listening to, without unpublishing the node — the device stays visible
    /// and selectable, it simply carries no traffic until something opens it.
    ///
    /// Windows always answers `true`: WASAPI render endpoints expose no
    /// equivalent signal, and the virtual-source path there is still a stub, so
    /// the behaviour is unchanged on that side.
    pub fn is_listening(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.inner.is_listening()
        }
        #[cfg(not(target_os = "linux"))]
        {
            true
        }
    }

    /// Apply a logical gain (linear, 1.0 = unity) to this playback node.
    ///
    /// This is the per-stream gain of the "one system" model — a property of the
    /// Stream/Route, applied *above* the OS mixer and separate from the device's
    /// single replicated volume (see `docs/audio-design.md`, "Separation of
    /// responsibilities"). Driven by an inbound
    /// [`Message::AudioGain`](cursio_protocol::Message::AudioGain). Lock-free on
    /// both backends, so it never disturbs the real-time playback thread.
    #[allow(unused_variables)]
    pub fn set_gain(&self, gain: f32) {
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
        self.inner.set_gain(gain);
    }
}

/// A membership change on an [`IntentSink`]: an app's playback stream joined or
/// left the sink. See [`IntentSink`] and `docs/audio-design.md`.
#[cfg(target_os = "linux")]
pub use pipewire_backend::IntentSinkEvent;

/// The membership event type on non-Linux platforms, where Intent Sinks are not
/// implemented yet. Present so callers compile on every platform.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone)]
pub enum IntentSinkEvent {
    /// A playback stream is now routed into this Intent Sink.
    Joined { app_node_id: u32, app_name: String },
    /// A playback stream is no longer routed into this Intent Sink.
    Left { app_node_id: u32 },
}

/// A device **Intent Sink**: a real local output that represents a peer device but
/// carries NO audio. It exists only so the user can route streams into it; Cursio
/// observes that membership (via [`events`](IntentSink::events)) and opens per-app
/// Remote Streams that actually carry the audio to the peer's real device. See
/// `docs/audio-design.md` "Redesign: one system".
pub struct IntentSink {
    #[cfg(target_os = "linux")]
    inner: pipewire_backend::PwIntentSink,
    #[cfg(target_os = "android")]
    inner: android_backend::AndroidIntentSink,
    // Elsewhere `create` always errors, so this is never constructed; the
    // empty receiver only exists so `events()` compiles uniformly.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    _events_rx: tokio::sync::mpsc::UnboundedReceiver<IntentSinkEvent>,
}

impl IntentSink {
    /// Create an Intent Sink for a peer device, named `name`/`description`. On
    /// Linux this publishes a silent `Audio/Sink`; on Android it is a target
    /// the app lists for the phone's playback (no system mixer routes apps
    /// there); other platforms report [`AudioError::Unsupported`] until their
    /// backend grows a routing observer.
    #[allow(unused_variables)]
    pub fn create(
        id: AudioStreamId,
        name: String,
        description: String,
    ) -> Result<Self, AudioError> {
        #[cfg(target_os = "linux")]
        {
            Ok(IntentSink {
                inner: pipewire_backend::PwIntentSink::create(id, name, description)?,
            })
        }
        #[cfg(target_os = "android")]
        {
            Ok(IntentSink {
                inner: android_backend::AndroidIntentSink::create(description),
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            Err(AudioError::Unsupported)
        }
    }

    /// Membership changes: apps joining/leaving this Intent Sink.
    pub fn events(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<IntentSinkEvent> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            self.inner.events()
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            &mut self._events_rx
        }
    }
}

/// A real device's volume or mute changed (the user turned the knob, or a
/// remote request was applied), observed by a [`DeviceMonitor`] so the
/// session can broadcast the new [`DeviceState`] to every node.
#[derive(Debug, Clone)]
pub struct DeviceChange {
    /// The device's owner-side handle, matched to the peer's Intent Sink: its
    /// node name on PipeWire, its friendly name on Windows (what
    /// `list_output_devices` advertises as `id`).
    pub node_name: String,
    /// Master volume, linear `0.0`..=`1.0`. PipeWire's own cubic channel
    /// volumes are converted before this; WASAPI's scalar already is linear.
    pub volume: f32,
    /// Whether the device is muted.
    pub mute: bool,
}

/// What the owner asks a backend's device monitor to apply.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) enum DeviceCommand {
    SetVolume { node_name: String, volume: f32 },
    SetMute { node_name: String, mute: bool },
}

/// Watches this machine's real audio **devices** and replicates their state.
///
/// The backend half of the "one system" model's device-state replication (see
/// `docs/audio-design.md`): the owner of a device uses this to learn when its
/// volume/mute changes (so it can broadcast a [`DeviceState`]) and to apply a
/// change requested by another node. Backed by PipeWire on Linux and WASAPI's
/// [`IAudioEndpointVolume`] on Windows; other platforms report
/// [`AudioError::Unsupported`].
pub struct DeviceMonitor {
    // First, so it is stopped and joined while `commands` is still open.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    _worker: worker::Worker,
    changes_rx: tokio::sync::mpsc::UnboundedReceiver<DeviceChange>,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    commands: std::sync::mpsc::Sender<DeviceCommand>,
}

impl DeviceMonitor {
    /// Start monitoring local devices.
    pub fn start() -> Result<Self, AudioError> {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            let (changes_tx, changes_rx) = tokio::sync::mpsc::unbounded_channel();
            let (commands, commands_rx) = std::sync::mpsc::channel();
            #[cfg(target_os = "linux")]
            let worker = pipewire_backend::spawn_device_monitor(changes_tx, commands_rx)?;
            #[cfg(target_os = "windows")]
            let worker = wasapi_backend::spawn_device_monitor(changes_tx, commands_rx)?;
            Ok(DeviceMonitor {
                _worker: worker,
                changes_rx,
                commands,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Err(AudioError::Unsupported)
        }
    }

    /// Observed device volume/mute changes.
    pub fn changes(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<DeviceChange> {
        &mut self.changes_rx
    }

    /// Apply a master volume (linear `0.0`..=`1.0`) to the named device.
    #[allow(unused_variables)]
    pub fn set_volume(&self, node_name: String, volume: f32) {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        let _ = self
            .commands
            .send(DeviceCommand::SetVolume { node_name, volume });
    }

    /// Apply a mute flag to the named device.
    #[allow(unused_variables)]
    pub fn set_mute(&self, node_name: String, mute: bool) {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        let _ = self
            .commands
            .send(DeviceCommand::SetMute { node_name, mute });
    }
}
