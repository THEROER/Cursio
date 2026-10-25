//! Playing a peer's stream: a node per stream, fed from a bounded jitter
//! buffer, filling exactly what each graph cycle asks for.

use crate::pipewire_backend::format_param_pod;
use crate::pipewire_backend::BYTES_PER_FRAME;
use crate::pipewire_backend::CHANNELS;
use crate::pipewire_backend::SAMPLE_RATE;
use crate::worker::{OnDrop, Worker};
use crate::AudioError;
use cursio_protocol::{AudioChunk, AudioStreamId};
use pipewire::{
    properties::properties,
    spa::{pod::Pod, utils::Direction},
    stream::{StreamFlags, StreamRc},
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

/// What the jitter buffer holds beyond one graph cycle before playback
/// starts, in bytes; see `playout` for why it is on top of the cycle.
///
/// A few chunks' worth (~20-30ms at 256-frame chunks) absorbs network jitter so
/// a late packet doesn't underrun. Small on purpose — audio favors freshness,
/// so we buffer only enough to stay gap-free, not to hide large delays.
pub(super) const JITTER_PREFILL_BYTES: usize = 4 * 256 * BYTES_PER_FRAME;

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
pub(super) const JITTER_MAX_BYTES: usize = 48_000 / 5 * BYTES_PER_FRAME;

/// How many frames of a playback buffer holding `capacity` to fill: what the
/// graph asked for this cycle, or the whole buffer when it named no amount.
///
/// The mapped buffer is not the cycle. A virtual microphone's buffer holds
/// 12288 frames, and filling all of it every cycle meant 256 ms a cycle —
/// past the jitter ceiling, so every cycle was 200 ms of a peer's voice and
/// 56 ms of padding, heard as the microphone cutting out four times a second.
pub(super) fn frames_to_write(requested: u64, capacity: usize) -> usize {
    match usize::try_from(requested) {
        Ok(0) | Err(_) => capacity,
        Ok(requested) => requested.min(capacity),
    }
}

/// A running PipeWire playback node (the **sink** side).
///
/// Inbound [`AudioChunk`]s are pushed into a byte queue drained by the PipeWire
/// `process` callback on its own thread. The queue is the jitter buffer: it
/// prefills a little before output begins, then feeds each buffer PipeWire asks
/// for, emitting silence on underrun rather than blocking.
pub struct PwSink {
    // First, so the thread is signalled before the feed and gain go.
    _worker: Worker,
    id: AudioStreamId,
    /// Samples to play, pushed from the async side, drained on the PW thread.
    feed: crate::playback_feed::Sender,
    /// Logical gain (linear, 1.0 = unity) applied to every sample as the PW
    /// thread fills the output buffer. Stored as the bit pattern of an `f32` in
    /// an atomic so the async side can update it (`set_gain`) without a lock —
    /// the process callback is real-time and must never block. See
    /// [`Message::AudioGain`](cursio_protocol::Message::AudioGain).
    gain: Arc<AtomicU32>,
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
}

impl PwSink {
    /// Open a playback node for a stream. Creates a virtual output that other
    /// tools (VoiceMeeter/Carla, or just the default sink) can route; audio
    /// pushed via [`play`](PwSink::play) comes out here.
    ///
    /// `node_name` names the node in the graph — e.g. `"Cursio: Spotify (PC)"`
    /// for a per-app stream — so it is addressable/routable on its own.
    pub fn open(id: AudioStreamId, node_name: String) -> Result<Self, AudioError> {
        Self::open_as(id, node_name, false, None)
    }

    /// Open a playback node the session manager links to the output named
    /// `target` (its node name) rather than to the default one; `None` is
    /// [`open`](PwSink::open).
    pub fn open_to(
        id: AudioStreamId,
        node_name: String,
        target: Option<String>,
    ) -> Result<Self, AudioError> {
        Self::open_as(id, node_name, false, target)
    }

    /// Open as a virtual **microphone**: the node is published with
    /// `media.class = Audio/Source`, so local apps can select it as a recording
    /// device and hear what the peer's microphone picks up.
    pub fn virtual_source(id: AudioStreamId, node_name: String) -> Result<Self, AudioError> {
        Self::open_as(id, node_name, true, None)
    }

    /// Shared body of [`open`](PwSink::open) and
    /// [`virtual_source`](PwSink::virtual_source) — the two differ only in
    /// whether the node is published as a source.
    fn open_as(
        id: AudioStreamId,
        node_name: String,
        as_source: bool,
        target: Option<String>,
    ) -> Result<Self, AudioError> {
        let (feed_tx, feed_rx) = crate::playback_feed::channel(JITTER_MAX_BYTES);

        // Unity gain to start; the atomic is shared with the PW thread so
        // `set_gain` takes effect on the next output buffer without a lock.
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let gain_thread = Arc::clone(&gain);
        let listening = Arc::new(AtomicBool::new(false));
        let listening_thread = Arc::clone(&listening);

        let worker = Worker::start(
            format!("cursio-pw-playback-{id}"),
            "playback thread",
            OnDrop::Detach,
            move |stop_rx, ready_tx| {
                run_playback_loop(
                    node_name,
                    as_source,
                    target,
                    feed_rx,
                    stop_rx,
                    ready_tx,
                    gain_thread,
                    listening_thread,
                )
            },
        )?;
        Ok(PwSink {
            _worker: worker,
            id,
            feed: feed_tx,
            gain,
            listening,
        })
    }

    /// Enqueue audio without blocking the async or PipeWire threads.
    pub fn play(&self, chunk: &AudioChunk) -> Result<(), AudioError> {
        self.feed.send(chunk)
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

/// The PipeWire main-loop body for playback. Owns the jitter buffer (a byte
/// `VecDeque` fed from `feed_rx`) and fills each requested output buffer from it.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_playback_loop(
    node_name: String,
    // Publish the node as `Audio/Source` (a virtual microphone) rather than an
    // ordinary playback stream.
    as_source: bool,
    // The output to be linked to, by node name; the default output when unset.
    target: Option<String>,
    feed_rx: crate::playback_feed::Receiver,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
    // Logical gain (f32 bits), updated from the async side by `set_gain`.
    gain: Arc<AtomicU32>,
    listening: Arc<AtomicBool>,
) {
    use std::cell::RefCell;

    let setup = (|| -> Result<_, String> {
        let (main_loop, core) = super::connect()?;

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
        if let Some(target) = &target {
            props.insert("target.object", target.as_str());
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

    let playout = RefCell::new(crate::playout::Playout::new(
        SAMPLE_RATE,
        CHANNELS as usize,
        JITTER_MAX_BYTES,
        JITTER_PREFILL_BYTES,
    ));
    // Start/underrun counters, logged on each transition. Cheap enough for the
    // RT callback (a Cell bump plus a `debug!` that costs nothing when the level
    // is off) and they turn "it stutters for the first few seconds" into a
    // timestamped count of exactly how often playback stopped and restarted.
    let starts = std::cell::Cell::new(0u32);
    let underruns = std::cell::Cell::new(0u32);
    let node_label = node_name.clone();
    // Atomics, not `Cell`s: with RT_PROCESS the callback runs on PipeWire's data
    // thread, while the loop below reads these from this one.
    let stopping = Arc::new(AtomicBool::new(false));
    let faded_out = Arc::new(AtomicBool::new(true));
    let (stopping_cb, faded_out_cb) = (Arc::clone(&stopping), Arc::clone(&faded_out));

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
            let mut playout = playout.borrow_mut();

            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested();
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let Some(slice) = data.data() else { return };

            let want_bytes = frames_to_write(requested, slice.len() / stride) * stride;
            feed_rx.set_cycle(want_bytes);
            let mut trimmed = 0;
            feed_rx.report(&node_label);
            while let Ok(packet) = feed_rx.try_recv() {
                trimmed += playout.push_at(&packet.data, packet.arrived, Some(packet.timestamp));
            }

            // Read once per buffer: the gain changes at knob speed, not audio
            // speed.
            let g = f32::from_bits(gain.load(Ordering::Relaxed));
            let fill = playout.fill(
                &mut slice[..want_bytes],
                g,
                stopping_cb.load(Ordering::Relaxed),
            );
            faded_out_cb.store(playout.is_silent(), Ordering::Relaxed);
            trimmed += fill.trimmed;
            if trimmed > 0 {
                tracing::debug!(
                    node = %node_label,
                    dropped_bytes = trimmed,
                    "jitter buffer over its limit; dropped stale audio to keep latency bounded"
                );
            }
            // Log every start, not just the first: a stream that keeps
            // re-starting is the buffer running dry and re-prefilling, which is
            // heard as audio cutting in and out. Pairing this with the underrun
            // below gives the on/off period directly.
            if fill.started {
                starts.set(starts.get() + 1);
                tracing::debug!(
                    node = %node_label,
                    start = starts.get(),
                    wanted_bytes = want_bytes,
                    "jitter buffer prefilled; playback starting"
                );
            }
            if fill.ran_dry {
                underruns.set(underruns.get() + 1);
                // `filled` says whether this buffer was satisfied or padded with
                // silence, which separates "arrived just in time" from a real gap.
                tracing::debug!(
                    node = %node_label,
                    underrun = underruns.get(),
                    filled_bytes = fill.played,
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
            // Destroying the stream mid-waveform is a click, and a stream is
            // usually closed while its last packets are still buffered. Let the
            // callback ramp to silence first; a node nobody drives never runs
            // it, hence the bound.
            stopping.store(true, Ordering::Relaxed);
            let deadline = std::time::Instant::now() + Duration::from_millis(100);
            while !faded_out.load(Ordering::Relaxed)
                && listening.load(Ordering::Relaxed)
                && std::time::Instant::now() < deadline
            {
                loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(5)));
            }
            break;
        }
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipewire_backend::BYTES_PER_FRAME;

    #[test]
    fn a_playback_cycle_fills_what_the_graph_requested_not_the_whole_buffer() {
        assert_eq!(frames_to_write(1024, 12_288), 1024);
        assert_eq!(
            frames_to_write(0, 12_288),
            12_288,
            "no request names no amount"
        );
        assert_eq!(
            frames_to_write(20_000, 12_288),
            12_288,
            "never past the buffer"
        );
    }

    /// A sink nobody drains must stop accepting audio.
    ///
    /// `JITTER_MAX_BYTES` is enforced in the process callback, which only runs
    /// while PipeWire drives the node. A virtual `Audio/Source` with no
    /// application recording from it is never driven, so before this check the
    /// peer's microphone audio accumulated in the feed channel for the life of
    /// the session — measured at 35 GB over ~24 h on one stream.
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
