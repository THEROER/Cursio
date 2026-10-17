//! Android audio through AAudio: a peer's streams play here, and this phone's
//! microphone is offered to the peer.
//!
//! A phone has one output and one input as far as an app can address them —
//! the system routes both to whatever is plugged in or paired, and moves them
//! when that changes. So unlike PipeWire or WASAPI there is no device list to
//! mirror: [`list_output_devices`] and [`list_input_devices`] each name one
//! device, "this phone's", and the system decides what that means at any
//! moment. When it changes its mind (headphones out, a Bluetooth headset
//! connecting) AAudio disconnects the stream rather than following, so every
//! stream here is owned by a thread that reopens it.
//!
//! An `AAudioStream` is not `Send`, and its callbacks run on a real-time thread
//! AAudio owns, where locking or blocking glitches the audio. So the owning
//! thread holds the stream and nothing else touches it; data crosses into the
//! playback callback over a channel the callback drains without blocking, and
//! out of the capture callback over an unbounded channel it never waits on —
//! the same arrangement the PipeWire backend uses inside its process callback.
//!
//! Playback keeps the PipeWire backend's jitter discipline exactly — a short
//! prefill, a hard ceiling that drops the *oldest* audio, short ramps at every
//! edge — because a phone on Wi-Fi sees more jitter than a desktop on a cable,
//! not less, and the reasons those rules exist (latency that grows without
//! bound, clicks at every underrun) are not platform-specific.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use cursio_protocol::{
    AudioChunk, AudioCodec, AudioFormat, AudioStreamId, AudioStreamMeta, SampleKind, StreamRole,
};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat as AaudioFormat, AudioInputPreset,
    AudioPerformanceMode, AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
};

use crate::{AudioError, OutputDevice};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const BYTES_PER_FRAME: usize = CHANNELS * 4;

/// The one output an app on Android addresses: whatever the system routes to.
pub(crate) const PHONE_OUTPUT: &str = "phone-output";
/// The one input: the microphone the system picks (built-in, or a headset's).
pub(crate) const PHONE_INPUT: &str = "phone-input";

/// Prefill before playback starts: ~21 ms, as on the PipeWire backend.
const JITTER_PREFILL_BYTES: usize = 4 * 256 * BYTES_PER_FRAME;
/// Ceiling on queued playback: ~200 ms. Past it the oldest audio goes, for the
/// reason given on the PipeWire backend's `JITTER_MAX_BYTES`.
const JITTER_MAX_BYTES: usize = SAMPLE_RATE as usize / 5 * BYTES_PER_FRAME;

/// How long a disconnected stream waits before reopening. The route change that
/// disconnected it is often still settling; reopening at once can land on the
/// device that is going away.
const REOPEN_DELAY: Duration = Duration::from_millis(300);

pub fn list_output_devices() -> Result<Vec<OutputDevice>, AudioError> {
    Ok(vec![OutputDevice {
        id: PHONE_OUTPUT.into(),
        description: "Speaker".into(),
    }])
}

pub fn list_input_devices() -> Result<Vec<OutputDevice>, AudioError> {
    Ok(vec![OutputDevice {
        id: PHONE_INPUT.into(),
        description: "Microphone".into(),
    }])
}

pub(crate) fn wire_format() -> AudioFormat {
    AudioFormat {
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS as u8,
        sample_kind: SampleKind::F32,
    }
}

/// What the owning thread waits on: the caller asking it to stop, or AAudio
/// reporting that the stream it holds is dead.
enum Wake {
    Stop,
    Disconnected,
}

/// Run `open` on a dedicated thread, keep the stream it returns running, and
/// reopen it whenever AAudio disconnects it, until stopped.
///
/// Returns once the first open has succeeded or failed, so a caller learns at
/// once that, say, the microphone permission is missing.
fn spawn_owner<F>(name: String, mut open: F) -> Result<std_mpsc::Sender<Wake>, AudioError>
where
    F: FnMut(std_mpsc::Sender<Wake>) -> Result<AudioStream, String> + Send + 'static,
{
    let (wake_tx, wake_rx) = std_mpsc::channel::<Wake>();
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();
    let thread_wake = wake_tx.clone();
    std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            let mut ready = Some(ready_tx);
            loop {
                let stream = match open(thread_wake.clone()) {
                    Ok(stream) => stream,
                    Err(e) => {
                        match ready.take() {
                            Some(ready) => {
                                let _ = ready.send(Err(e));
                                return;
                            }
                            None => {
                                tracing::warn!(stream = %name, error = %e, "could not reopen the audio stream; retrying");
                            }
                        }
                        match wake_rx.recv_timeout(REOPEN_DELAY * 3) {
                            Ok(Wake::Stop) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                                return
                            }
                            _ => continue,
                        }
                    }
                };
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Ok(()));
                }
                let wake = wake_rx.recv();
                let _ = stream.request_stop();
                drop(stream);
                match wake {
                    Ok(Wake::Disconnected) => {
                        tracing::info!(stream = %name, "audio route changed; reopening the stream");
                        std::thread::sleep(REOPEN_DELAY);
                    }
                    Ok(Wake::Stop) | Err(_) => return,
                }
            }
        })
        .map_err(|e| AudioError::Backend(format!("spawn audio thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(wake_tx),
        Ok(Err(e)) => Err(AudioError::Backend(e)),
        Err(_) => Err(AudioError::Backend(
            "audio thread exited before opening its stream".into(),
        )),
    }
}

fn disconnect_reporter(wake: std_mpsc::Sender<Wake>) -> ndk::audio::AudioStreamErrorCallback {
    Box::new(move |_stream, error| {
        // Called on an AAudio thread that must not reopen the stream itself;
        // the owner does, on its own thread.
        tracing::debug!(?error, "aaudio stream error");
        let _ = wake.send(Wake::Disconnected);
    })
}

/// The one "application" a phone can route to a peer device: everything it
/// plays. Android lets an app capture other apps' playback only as a whole
/// mix (AudioPlaybackCapture), so there is no per-app list to offer.
pub const PHONE_PLAYBACK_NODE: u32 = 1;
pub const PHONE_PLAYBACK_NAME: &str = "Phone";

/// This phone's microphone or its playback, recorded for a peer.
pub struct AndroidSource {
    adverts_rx: tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta>,
    chunks_rx: tokio::sync::mpsc::UnboundedReceiver<AudioChunk>,
    /// The microphone's owning thread; `None` for the playback tap, which the
    /// JVM feeds and which therefore has no stream of its own to own.
    wake: Option<std_mpsc::Sender<Wake>>,
    /// Set for the playback tap, so dropping it stops what the JVM pushes
    /// from reaching a receiver nobody reads.
    playback: Option<AudioStreamId>,
}

/// Where the JVM's captured playback goes: the open playback tap, if any.
struct PlaybackSink {
    id: AudioStreamId,
    chunks: tokio::sync::mpsc::UnboundedSender<AudioChunk>,
    seq: u32,
    frames: u64,
}

static PLAYBACK: std::sync::Mutex<Option<PlaybackSink>> = std::sync::Mutex::new(None);

/// How loud this phone's playback is sent, linear, as `f32` bits.
///
/// Playback capture taps the mix before the phone's own volume, so what
/// reached a peer was always full scale whatever the rocker said — loud enough
/// to hurt through speakers set for a quieter source. This is the phone's knob
/// for it; the listener's own volume still multiplies on the far side.
static PLAYBACK_VOLUME: AtomicU32 = AtomicU32::new(0x3f80_0000);

/// The loudest the send volume goes: as captured, never boosted, since a
/// boost on top of a full-scale mix only clips.
pub const MAX_PLAYBACK_VOLUME: f32 = 1.0;

pub fn set_playback_volume(volume: f32) {
    let volume = if volume.is_finite() {
        volume.clamp(0.0, MAX_PLAYBACK_VOLUME)
    } else {
        MAX_PLAYBACK_VOLUME
    };
    PLAYBACK_VOLUME.store(volume.to_bits(), Ordering::Relaxed);
}

pub fn playback_volume() -> f32 {
    f32::from_bits(PLAYBACK_VOLUME.load(Ordering::Relaxed))
}

/// Hand in a block of what this phone is playing, as interleaved stereo f32 at
/// 48 kHz — the JVM's AudioPlaybackCapture output. Dropped when no peer device
/// is routed to, so capture can run ahead of the route without piling up.
pub fn push_playback(samples: &[f32]) {
    let mut slot = PLAYBACK.lock().unwrap_or_else(|p| p.into_inner());
    let Some(sink) = slot.as_mut() else {
        return;
    };
    let frames = (samples.len() / CHANNELS) as u64;
    let volume = playback_volume();
    let data = samples
        .iter()
        .flat_map(|s| (s * volume).to_le_bytes())
        .collect();
    let chunk = AudioChunk {
        id: sink.id,
        seq: sink.seq,
        timestamp: sink.frames,
        data,
    };
    sink.seq = sink.seq.wrapping_add(1);
    sink.frames += frames;
    if sink.chunks.send(chunk).is_err() {
        *slot = None;
    }
}

impl AndroidSource {
    /// What this phone plays, as the stream routed to a peer device.
    pub fn playback_tap(id: AudioStreamId) -> Result<Self, AudioError> {
        let (adverts_tx, adverts_rx) = tokio::sync::mpsc::unbounded_channel();
        let (chunks_tx, chunks_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = adverts_tx.send(AudioStreamMeta {
            id,
            role: StreamRole::Playback,
            app_name: Some(PHONE_PLAYBACK_NAME.into()),
            device: None,
            format: wire_format(),
            codec: AudioCodec::RawPcm,
        });
        *PLAYBACK.lock().unwrap_or_else(|p| p.into_inner()) = Some(PlaybackSink {
            id,
            chunks: chunks_tx,
            seq: 0,
            frames: 0,
        });
        Ok(Self {
            adverts_rx,
            chunks_rx,
            wake: None,
            playback: Some(id),
        })
    }
}

/// A peer device this phone can send its playback to — one per device a
/// linked peer advertised. The phone's version of a desktop's Intent Sink:
/// there is no system mixer to route an app into a device here, so the user
/// picks one in the app and [`route_playback`] turns that choice into the same
/// membership events a PipeWire Intent Sink produces.
pub struct AndroidIntentSink {
    key: u64,
    events_rx: tokio::sync::mpsc::UnboundedReceiver<crate::IntentSinkEvent>,
}

struct SinkEntry {
    key: u64,
    description: String,
    events: tokio::sync::mpsc::UnboundedSender<crate::IntentSinkEvent>,
}

#[derive(Default)]
struct Sinks {
    entries: Vec<SinkEntry>,
    routed: Option<u64>,
    /// The device the user chose, by name, kept while its peer is away.
    ///
    /// A key names one link's device and dies with the link, so a route that
    /// remembered only the key was gone for good the first time the laptop
    /// slept or the phone left the room. The name outlives both: the phone
    /// plays on itself while the device is away and goes back to it when it
    /// returns.
    wanted: Option<String>,
    next_key: u64,
}

static SINKS: std::sync::Mutex<Sinks> = std::sync::Mutex::new(Sinks {
    entries: Vec::new(),
    routed: None,
    wanted: None,
    next_key: 1,
});

impl Sinks {
    fn join(&mut self, key: u64) {
        if let Some(entry) = self.entries.iter().find(|e| e.key == key) {
            let _ = entry.events.send(crate::IntentSinkEvent::Joined {
                app_node_id: PHONE_PLAYBACK_NODE,
                app_name: PHONE_PLAYBACK_NAME.into(),
            });
            self.routed = Some(key);
        }
    }
}

fn sinks() -> std::sync::MutexGuard<'static, Sinks> {
    SINKS.lock().unwrap_or_else(|p| p.into_inner())
}

/// A device the phone's playback can be sent to, for the app to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackTarget {
    pub key: u64,
    /// As peers' devices are labelled everywhere: `Cursio(<peer>): <device>`.
    pub name: String,
    pub routed: bool,
}

pub fn playback_targets() -> Vec<PlaybackTarget> {
    let sinks = sinks();
    sinks
        .entries
        .iter()
        .map(|e| PlaybackTarget {
            key: e.key,
            name: e.description.clone(),
            routed: sinks.routed == Some(e.key),
        })
        .collect()
}

/// The device this phone's sound is meant for while it is away, by name: what
/// the app shows instead of a route that silently stopped.
pub fn awaited_playback() -> Option<String> {
    let sinks = sinks();
    sinks
        .routed
        .is_none()
        .then(|| sinks.wanted.clone())
        .flatten()
}

/// Send this phone's playback to one device, or to none.
pub fn route_playback(target: Option<u64>) -> Result<(), AudioError> {
    let mut sinks = sinks();
    if let Some(key) = target {
        if !sinks.entries.iter().any(|e| e.key == key) {
            return Err(AudioError::StreamGone(format!("playback target {key}")));
        }
    }
    if target.is_none() {
        // Stopping also forgets a device that is away right now, or it would
        // take the sound back the moment its peer returned.
        sinks.wanted = None;
    }
    if sinks.routed == target {
        return Ok(());
    }
    if let Some(old) = sinks.routed.take() {
        if let Some(entry) = sinks.entries.iter().find(|e| e.key == old) {
            let _ = entry.events.send(crate::IntentSinkEvent::Left {
                app_node_id: PHONE_PLAYBACK_NODE,
            });
        }
    }
    sinks.wanted = target.and_then(|key| {
        sinks
            .entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.description.clone())
    });
    if let Some(key) = target {
        sinks.join(key);
    }
    Ok(())
}

impl AndroidIntentSink {
    pub fn create(description: String) -> Self {
        let (events, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sinks = sinks();
        let key = sinks.next_key;
        sinks.next_key += 1;
        let returning =
            sinks.routed.is_none() && sinks.wanted.as_deref() == Some(description.as_str());
        sinks.entries.push(SinkEntry {
            key,
            description,
            events,
        });
        if returning {
            tracing::info!(
                key,
                "the chosen device is back; sending this phone's sound to it again"
            );
            sinks.join(key);
        }
        Self { key, events_rx }
    }

    pub fn events(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<crate::IntentSinkEvent> {
        &mut self.events_rx
    }
}

/// The peer's link went away: the device is no longer a target, and a route
/// to it ends rather than pointing at nothing. Its name stays wanted, so the
/// route comes back with the device.
impl Drop for AndroidIntentSink {
    fn drop(&mut self) {
        let mut sinks = sinks();
        sinks.entries.retain(|e| e.key != self.key);
        if sinks.routed == Some(self.key) {
            sinks.routed = None;
        }
    }
}

impl AndroidSource {
    pub fn mic_tap(id: AudioStreamId) -> Result<Self, AudioError> {
        let (adverts_tx, adverts_rx) = tokio::sync::mpsc::unbounded_channel();
        let (chunks_tx, chunks_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = adverts_tx.send(AudioStreamMeta {
            id,
            role: StreamRole::Capture,
            app_name: None,
            device: None,
            format: wire_format(),
            codec: AudioCodec::RawPcm,
        });

        // The sample clock survives a reopen, so the peer's seq/timestamp
        // checks see one continuous stream across a route change.
        let seq = Arc::new(AtomicU32::new(0));
        let frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let wake = spawn_owner(format!("cursio-aaudio-mic-{id}"), move |wake| {
            let chunks_tx = chunks_tx.clone();
            let seq = Arc::clone(&seq);
            let frames = Arc::clone(&frames);
            let builder = AudioStreamBuilder::new()
                .map_err(|e| format!("aaudio builder: {e}"))?
                .direction(AudioDirection::Input)
                .format(AaudioFormat::PCM_Float)
                .sample_rate(SAMPLE_RATE as i32)
                // Mono: a phone has one voice microphone worth recording, and
                // asking for two gets either a duplicated channel or a second
                // mic aimed at the room. It is spread to both channels below,
                // since a mono capture played as stereo-left-only was the
                // "sound in one ear" bug on Windows.
                .channel_count(1)
                .performance_mode(AudioPerformanceMode::LowLatency)
                .sharing_mode(AudioSharingMode::Shared)
                // Voice communication turns on the platform's echo canceller.
                // Without it a phone that is also playing the PC's audio sends
                // that audio straight back through its own microphone.
                .input_preset(AudioInputPreset::VoiceCommunication)
                .error_callback(disconnect_reporter(wake))
                .data_callback(Box::new(move |stream, data, num_frames| {
                    let channels = stream.channel_count().max(1) as usize;
                    let samples = num_frames.max(0) as usize * channels;
                    // SAFETY: AAudio hands an input callback `num_frames` frames
                    // of the stream's own format, which was opened as f32.
                    let input = unsafe { std::slice::from_raw_parts(data as *const f32, samples) };
                    let mut out = Vec::with_capacity(num_frames.max(0) as usize * BYTES_PER_FRAME);
                    for frame in input.chunks_exact(channels) {
                        let (l, r) = if channels == 1 {
                            (frame[0], frame[0])
                        } else {
                            (frame[0], frame[1])
                        };
                        out.extend_from_slice(&l.to_le_bytes());
                        out.extend_from_slice(&r.to_le_bytes());
                    }
                    let timestamp = frames.fetch_add(num_frames.max(0) as u64, Ordering::Relaxed);
                    let _ = chunks_tx.send(AudioChunk {
                        id,
                        seq: seq.fetch_add(1, Ordering::Relaxed),
                        timestamp,
                        data: out,
                    });
                    AudioCallbackResult::Continue
                }));
            let stream = builder.open_stream().map_err(|e| {
                format!("cannot open the microphone ({e}); is the RECORD_AUDIO permission granted?")
            })?;
            check_rate(&stream)?;
            stream
                .request_start()
                .map_err(|e| format!("cannot start the microphone: {e}"))?;
            tracing::info!(
                channels = stream.channel_count(),
                burst = stream.frames_per_burst(),
                "microphone open"
            );
            Ok(stream)
        })?;

        Ok(Self {
            adverts_rx,
            chunks_rx,
            wake: Some(wake),
            playback: None,
        })
    }

    pub fn advertisements(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta> {
        &mut self.adverts_rx
    }

    pub fn chunks(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioChunk> {
        &mut self.chunks_rx
    }
}

impl Drop for AndroidSource {
    fn drop(&mut self) {
        if let Some(wake) = &self.wake {
            let _ = wake.send(Wake::Stop);
        }
        if let Some(id) = self.playback {
            let mut slot = PLAYBACK.lock().unwrap_or_else(|p| p.into_inner());
            if slot.as_ref().is_some_and(|sink| sink.id == id) {
                *slot = None;
            }
        }
    }
}

/// A peer's stream playing on this phone.
pub struct AndroidSink {
    feed: std_mpsc::Sender<Vec<u8>>,
    /// Bytes handed to the callback and not yet taken into its buffer, so
    /// [`play`](Self::play) can refuse more once a full buffer is waiting even
    /// while the callback is not running (a stream paused by a route change).
    queued: Arc<AtomicUsize>,
    gain: Arc<AtomicU32>,
    wake: std_mpsc::Sender<Wake>,
}

impl AndroidSink {
    pub fn open(id: AudioStreamId, name: String) -> Result<Self, AudioError> {
        let (feed_tx, feed_rx) = std_mpsc::channel::<Vec<u8>>();
        let feed_rx = Arc::new(std::sync::Mutex::new(feed_rx));
        let queued = Arc::new(AtomicUsize::new(0));
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));

        let thread_queued = Arc::clone(&queued);
        let thread_gain = Arc::clone(&gain);
        let wake = spawn_owner(format!("cursio-aaudio-play-{id}"), move |wake| {
            // Each opened stream takes the receiver for its callback's life;
            // the lock is held by one callback at a time, never contended,
            // because the previous stream is stopped before the next opens.
            let feed_rx = Arc::clone(&feed_rx);
            let queued = Arc::clone(&thread_queued);
            let gain = Arc::clone(&thread_gain);
            let fade = crate::declick::fade_frames(SAMPLE_RATE);
            let mut declick = crate::declick::Declick::new(fade);
            let mut jitter: VecDeque<u8> = VecDeque::with_capacity(JITTER_MAX_BYTES * 2);
            let mut started = false;
            let builder = AudioStreamBuilder::new()
                .map_err(|e| format!("aaudio builder: {e}"))?
                .direction(AudioDirection::Output)
                .format(AaudioFormat::PCM_Float)
                .sample_rate(SAMPLE_RATE as i32)
                .channel_count(CHANNELS as i32)
                .performance_mode(AudioPerformanceMode::LowLatency)
                .sharing_mode(AudioSharingMode::Shared)
                .usage(AudioUsage::Media)
                .error_callback(disconnect_reporter(wake))
                .data_callback(Box::new(move |_stream, data, num_frames| {
                    let want = num_frames.max(0) as usize * BYTES_PER_FRAME;
                    // SAFETY: AAudio hands an output callback room for
                    // `num_frames` frames of the stream's format, opened as f32
                    // stereo, and the pointer is valid for this call.
                    let out = unsafe { std::slice::from_raw_parts_mut(data as *mut u8, want) };
                    if let Ok(rx) = feed_rx.try_lock() {
                        while let Ok(bytes) = rx.try_recv() {
                            queued.fetch_sub(bytes.len(), Ordering::Relaxed);
                            jitter.extend(bytes);
                        }
                    }
                    if jitter.len() > JITTER_MAX_BYTES {
                        let excess = jitter.len() - JITTER_MAX_BYTES;
                        let excess = excess - excess % BYTES_PER_FRAME;
                        crate::declick::skip_crossfaded(&mut jitter, excess, CHANNELS, fade);
                    }
                    if !started && jitter.len() >= JITTER_PREFILL_BYTES {
                        started = true;
                    }
                    let take = if started { want.min(jitter.len()) } else { 0 };
                    for (slot, byte) in out.iter_mut().zip(jitter.drain(..take)) {
                        *slot = byte;
                    }
                    declick.apply(&mut out[..take], CHANNELS, jitter.is_empty());
                    let g = f32::from_bits(gain.load(Ordering::Relaxed));
                    if (g - 1.0).abs() > f32::EPSILON {
                        for s in out[..take].as_chunks_mut::<4>().0 {
                            *s = (f32::from_le_bytes(*s) * g).to_le_bytes();
                        }
                    }
                    out[take..].fill(0);
                    // Ran dry: prefill again rather than play single frames as
                    // they trickle in, which is heard as crackle.
                    if started && jitter.is_empty() {
                        started = false;
                    }
                    AudioCallbackResult::Continue
                }));
            let stream = builder
                .open_stream()
                .map_err(|e| format!("cannot open audio output: {e}"))?;
            check_rate(&stream)?;
            stream
                .request_start()
                .map_err(|e| format!("cannot start audio output: {e}"))?;
            tracing::info!(
                name = %name,
                burst = stream.frames_per_burst(),
                "playback open"
            );
            Ok(stream)
        })?;

        Ok(Self {
            feed: feed_tx,
            queued,
            gain,
            wake,
        })
    }

    pub fn play(&self, chunk: &AudioChunk) -> Result<(), AudioError> {
        if self.queued.load(Ordering::Relaxed) > JITTER_MAX_BYTES {
            return Ok(());
        }
        self.queued.fetch_add(chunk.data.len(), Ordering::Relaxed);
        self.feed
            .send(chunk.data.clone())
            .map_err(|_| AudioError::Backend("playback thread gone".into()))
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain
            .store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }
}

impl Drop for AndroidSink {
    fn drop(&mut self) {
        let _ = self.wake.send(Wake::Stop);
    }
}

/// AAudio may open at the device's own rate instead of the one asked for. The
/// wire is 48 kHz with no resampler on this side, so anything else would play
/// at the wrong pitch: refuse it by name rather than ship that.
fn check_rate(stream: &AudioStream) -> Result<(), String> {
    let rate = stream.sample_rate();
    if rate == SAMPLE_RATE as i32 {
        Ok(())
    } else {
        Err(format!(
            "audio device opened at {rate} Hz, not {SAMPLE_RATE} Hz, and this build has no resampler"
        ))
    }
}
