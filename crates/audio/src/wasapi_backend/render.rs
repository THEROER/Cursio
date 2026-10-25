//! Playing a peer's stream into an endpoint: a session per stream, kept
//! only a little ahead of the device, and an inaudible keepalive so the
//! endpoint does not idle between packets.

use crate::wasapi_backend::devices::find_render_device;
use crate::wasapi_backend::format::from_wire_format_stream;
use crate::wasapi_backend::ComGuard;
use crate::worker::{OnDrop, Worker};
use crate::AudioError;
use cursio_protocol::{AudioChunk, AudioStreamId};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, IAudioSessionControl, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

/// Playback handoff ceiling: 200 ms at the wire format. The render queue's
/// ceiling is calculated separately from the endpoint's rate and frame size.
/// Keep jitter below this ceiling: resetting a 100 ms backlog to 40 ms after
/// every burst repeatedly discards the reserve needed for the next late packet.
///
/// Mirrors the PipeWire sink — an unbounded queue converts a rate mismatch into
/// unbounded latency, because nothing ever removes the backlog. Beyond this the
/// OLDEST audio is dropped: live audio wants freshness, not completeness.
pub(super) const RENDER_MAX_BYTES: usize = 48_000 / 5 * 8;

/// How far ahead of the device the render loop writes, in milliseconds.
///
/// The endpoint buffer is 200 ms so a stalled loop does not underrun, but
/// writing all of it every pass kept it full — with keep-alive when nothing
/// arrived — so every sample that did arrive queued behind 200 ms already
/// written: the bulk of the delay between a phone and Voicemeeter. Only this
/// much is kept queued instead. Not less: the loop sleeps 5 ms but Windows'
/// default timer wakes it up to ~16 ms late, and the device must not run dry
/// in between.
pub(super) const RENDER_AHEAD_MS: u32 = 40;

/// A running WASAPI render node (the **sink** side).
///
/// Inbound [`AudioChunk`]s are pushed into a shared byte queue drained by the
/// render thread, which fills the device buffer's free space each pass and
/// writes silence on underrun. Symmetric to the PipeWire `PwSink`.
pub struct WasapiSink {
    // First, so the thread is signalled before the feed and gain go.
    _worker: Worker,
    id: AudioStreamId,
    feed: crate::playback_feed::Sender,
    /// Logical gain (linear, 1.0 = unity) applied to inbound audio, published to
    /// the render thread via an atomic (f32 bits) so `set_gain` never blocks it.
    /// The per-stream gain of the "one system" model — above the OS mixer,
    /// distinct from the device's single replicated volume. Applied to wire-format
    /// f32 samples before the device-format conversion, so it is independent of
    /// the endpoint's mix format.
    gain: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl WasapiSink {
    /// Open a render stream on the default output device.
    pub fn open(id: AudioStreamId, display_name: String) -> Result<Self, AudioError> {
        Self::open_inner(id, None, display_name)
    }

    /// Open a render stream on the output device whose friendly name contains
    /// `device_name` (case-insensitive) — e.g. `"VB-Audio Virtual Cable"`. This
    /// is how Cursio renders the peer's audio into a specific virtual endpoint
    /// on Windows (where user space can't create endpoints), so the peer's
    /// output behaves as that device in the Windows graph.
    pub fn open_device(
        id: AudioStreamId,
        device_name: String,
        display_name: String,
    ) -> Result<Self, AudioError> {
        Self::open_inner(id, Some(device_name), display_name)
    }

    fn open_inner(
        id: AudioStreamId,
        device_name: Option<String>,
        display_name: String,
    ) -> Result<Self, AudioError> {
        let (feed_tx, feed_rx) = crate::playback_feed::channel(RENDER_MAX_BYTES);

        let gain = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(1.0f32.to_bits()));
        let gain_thread = std::sync::Arc::clone(&gain);

        let worker = Worker::start(
            format!("cursio-wasapi-render-{id}"),
            "render thread",
            OnDrop::Detach,
            move |stop_rx, ready_tx| {
                run_render_loop(
                    device_name,
                    display_name,
                    feed_rx,
                    stop_rx,
                    ready_tx,
                    gain_thread,
                )
            },
        )?;
        Ok(WasapiSink {
            _worker: worker,
            id,
            feed: feed_tx,
            gain,
        })
    }

    /// Enqueue one inbound chunk for playback.
    pub fn play(&self, chunk: &AudioChunk) -> Result<(), AudioError> {
        self.feed.send(chunk)
    }

    /// Set the logical gain (linear, 1.0 = unity) applied to this node's audio.
    /// Lock-free: published to the render thread via an atomic. See the Linux
    /// `PwSink::set_gain` for the shared rationale.
    pub fn set_gain(&self, gain: f32) {
        let gain = gain.clamp(0.0, 4.0);
        self.gain
            .store(gain.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn id(&self) -> AudioStreamId {
        self.id
    }
}

// ---------------------------------------------------------------------------
// Device-state monitor (volume/mute replication — docs/audio-design.md phase 5)
// ---------------------------------------------------------------------------

/// Acquire a render device's `IAudioClient` for shared-mode playback, plus its
/// mix format and total buffer frame count. `device_name = None` uses the
/// default endpoint; `Some(name)` matches a device by friendly name.
///
/// `session` is the audio session this stream belongs to, and it must differ
/// per stream — see the call to `Initialize` below.
pub(super) unsafe fn init_render_client(
    device_name: Option<&str>,
    session: &windows::core::GUID,
) -> Result<(IAudioClient, WAVEFORMATEX, u32), String> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .map_err(|e| format!("enumerator: {e}"))?;
    let device = find_render_device(&enumerator, device_name)?;
    let client: IAudioClient = device
        .Activate::<IAudioClient>(CLSCTX_ALL, None)
        .map_err(|e| format!("activate: {e}"))?;

    let mix_ptr = client
        .GetMixFormat()
        .map_err(|e| format!("mix format: {e}"))?;
    let mix = *mix_ptr;

    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            0,
            200 * 10_000, // 200ms buffer, 100ns units
            0,
            mix_ptr,
            // The audio SESSION this stream joins, and the reason every remote
            // app gets its own mixer row.
            //
            // Passing `None` here does not mean "no session" — it means the
            // process's DEFAULT session, so every stream cursio renders lands in
            // the same one. The volume mixer shows a row per session, so three
            // remote applications appeared as a single row, and each stream's
            // `SetDisplayName` overwrote the shared name: the row ended up
            // labelled after whichever app opened last, carrying all of them.
            //
            // Distinct from the *grouping* param warned about below: grouping
            // yokes separate sessions to one slider, whereas this decides which
            // session a stream is in at all.
            Some(session),
        )
        .map_err(|e| format!("audio client init: {e}"))?;

    let buffer_frames = client
        .GetBufferSize()
        .map_err(|e| format!("buffer size: {e}"))?;

    Ok((client, mix, buffer_frames))
}

/// A session GUID derived from the name a stream renders under.
///
/// Stable on purpose: Windows remembers a volume setting per session GUID, so a
/// GUID that changed per run — from a stream id, say, which is allocated fresh
/// each session — would give Spotify a new mixer row every time cursio
/// reconnected, with the volume the user set last time left behind on a row
/// nothing plays into any more. Keyed by the display name instead, so the same
/// application lands on the same row and keeps its slider across restarts.
///
/// FNV-1a rather than a cryptographic hash: this needs to be stable and spread
/// out, not unpredictable, and it must not pull in a dependency for 16 bytes.
pub(super) fn session_guid(display_name: &str) -> windows::core::GUID {
    let mut hash: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    for byte in display_name.as_bytes() {
        hash ^= *byte as u128;
        hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    windows::core::GUID::from_u128(hash)
}

/// Peak amplitude of the keep-alive signal, as a fraction of full scale.
///
/// **-96 dBFS**, which is one step of 16-bit quantisation: at any sane playback
/// level this is below the noise floor of the equipment reproducing it, and it
/// is quieter than the dither an idle endpoint already carries. It is not zero,
/// because zero is what Windows watches for when deciding a session is idle.
pub(super) const KEEPALIVE_PEAK: f32 = 1.0 / 32768.0;

/// Fill a device-format buffer with a signal too quiet to hear.
///
/// Alternating sign rather than a constant: a fixed offset is a DC bias, which
/// some endpoints and every AC-coupled output stage handle badly, and which a
/// downstream mixer can accumulate across streams. Flipping each frame keeps the
/// mean at zero.
///
/// `phase` persists across calls so the alternation continues across buffers
/// instead of restarting — a restart would put two like-signed frames together
/// at every buffer boundary, which is a (still inaudible) impulse train.
///
/// The sign flips per *frame*, not per sample: flipping per sample on a stereo
/// endpoint gives every left sample one sign and every right sample the other,
/// which is exactly the DC bias this exists to avoid, once per channel.
pub(super) fn fill_inaudible(dst: &mut [u8], mix: &WAVEFORMATEX, phase: &mut bool) {
    // Only 32-bit samples are filled, since the render path treats the mix
    // format as f32 throughout (see `from_wire_format`). Anything else gets
    // zeros: correct, merely without the keep-alive.
    let frame_bytes = mix.nBlockAlign as usize;
    if mix.wBitsPerSample != 32 || frame_bytes == 0 {
        dst.fill(0);
        return;
    }
    for frame in dst.chunks_mut(frame_bytes) {
        let sample = if *phase {
            KEEPALIVE_PEAK
        } else {
            -KEEPALIVE_PEAK
        };
        *phase = !*phase;
        for slot in frame.as_chunks_mut::<4>().0 {
            *slot = sample.to_le_bytes();
        }
    }
}

pub(super) fn run_render_loop(
    device_name: Option<String>,
    display_name: String,
    feed_rx: crate::playback_feed::Receiver,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
    gain: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    use std::collections::VecDeque;

    let _span = tracing::debug_span!("wasapi_render", stream = %display_name).entered();

    let _com = match ComGuard::new() {
        Ok(g) => g,
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
            return;
        }
    };

    // SAFETY: all WASAPI calls run on this COM-initialized thread.
    unsafe {
        let session = session_guid(&display_name);
        let (client, mix, buffer_frames) =
            match init_render_client(device_name.as_deref(), &session) {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };

        let render: IAudioRenderClient = match client.GetService() {
            Ok(r) => r,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("render service: {e}")));
                return;
            }
        };

        // Label this stream's row in the Windows volume mixer.
        //
        // The row itself comes from the session GUID handed to `Initialize`
        // (see `session_guid`). Sndvol gives every session its OWN volume
        // slider *by default* — as long as its grouping
        // param stays GUID_NULL (the default). We must NOT set a grouping param:
        // per Microsoft's docs, sessions that SHARE a grouping GUID are yoked to a
        // single slider. An earlier attempt set a grouping GUID thinking it split
        // the rows; it did the opposite — it merged their volume control, so the
        // slider stopped working. Leaving grouping untouched is what keeps each
        // row independently adjustable.
        //
        // These are cross-process sessions (we render on behalf of a remote app),
        // so Windows has no app name/PID to auto-label them — SetDisplayName is
        // REQUIRED, else every row shows a meaningless "cursio.exe"/"RemoteApp".
        match client.GetService::<IAudioSessionControl>() {
            Ok(session) => {
                let wide: Vec<u16> = display_name
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                if let Err(e) =
                    session.SetDisplayName(windows::core::PCWSTR(wide.as_ptr()), std::ptr::null())
                {
                    tracing::warn!(error = %e, "could not set mixer display name");
                }
            }
            Err(e) => tracing::warn!(error = %e, "no session control; mixer row stays unnamed"),
        }

        let frame_bytes = mix.nBlockAlign as usize;
        let rate = mix.nSamplesPerSec;
        let max_jitter_bytes = rate as usize / 5 * frame_bytes;
        let mut render_phase = 0u64;
        let channel_count = mix.nChannels;
        tracing::info!(
            rate,
            channels = channel_count,
            frame_bytes,
            buffer_frames,
            ahead_ms = RENDER_AHEAD_MS,
            "WASAPI render initialized"
        );

        if let Err(e) = client.Start() {
            let _ = ready_tx.send(Err(format!("client start: {e}")));
            return;
        }
        let _ = ready_tx.send(Ok(()));

        let mut jitter: VecDeque<u8> = VecDeque::new();
        let mut started = false;
        let mut delay =
            crate::jitter::Delay::new(rate as usize, rate as usize * 40 / 1000, rate as usize / 5);
        let mut recovery = crate::jitter::Recovery::default();
        let mut accelerated_bytes = 0usize;
        // Sign of the next keep-alive frame; see `fill_inaudible`.
        let mut keepalive_phase = false;
        // The ramps assume f32 samples, as the rest of the render path does; on
        // any other mix format they stand aside rather than scale garbage.
        let channels = if mix.wBitsPerSample == 32 {
            mix.nChannels as usize
        } else {
            0
        };
        let fade = crate::declick::fade_frames(mix.nSamplesPerSec);
        let mut declick = crate::declick::Declick::new(fade);
        let mut stopping = false;
        let mut stop_deadline = std::time::Instant::now();
        let mut report_at = std::time::Instant::now();
        let mut last_loop = report_at;
        let mut last_feed = None;
        let mut max_loop_gap = Duration::ZERO;
        let mut max_feed_age = Duration::ZERO;
        let mut max_feed_gap = Duration::ZERO;
        let mut packets = 0u64;
        let mut underruns = 0u64;
        let mut dropped_bytes = 0usize;
        let mut api_errors = 0u64;
        let mut audio_frames = 0u64;
        let mut keepalive_frames = 0u64;

        loop {
            let now = std::time::Instant::now();
            max_loop_gap = max_loop_gap.max(now.duration_since(last_loop));
            last_loop = now;
            if now.duration_since(report_at) >= Duration::from_secs(10) {
                tracing::debug!(
                    packets,
                    underruns,
                    dropped_bytes,
                    api_errors,
                    accelerated_bytes,
                    target_ms = delay.frames() * 1000 / rate as usize,
                    audio_frames,
                    keepalive_frames,
                    max_loop_gap_ms = max_loop_gap.as_secs_f64() * 1000.0,
                    max_feed_age_ms = max_feed_age.as_secs_f64() * 1000.0,
                    max_feed_gap_ms = max_feed_gap.as_secs_f64() * 1000.0,
                    queued_ms = jitter.len() as f64 * 1000.0 / (frame_bytes as f64 * rate as f64),
                    "WASAPI render health"
                );
                report_at = now;
                max_loop_gap = Duration::ZERO;
                max_feed_age = Duration::ZERO;
                max_feed_gap = Duration::ZERO;
                packets = 0;
                underruns = 0;
                dropped_bytes = 0;
                api_errors = 0;
                accelerated_bytes = 0;
                audio_frames = 0;
                keepalive_frames = 0;
            }
            if !stopping && stop_rx.try_recv().is_ok() {
                // `Stop` halts the device mid-buffer, so a stream closed while
                // audio is still queued ends on a click. Write a fade-out, let
                // the device play what it holds, and only then stop it.
                stopping = true;
                stop_deadline = std::time::Instant::now() + Duration::from_millis(300);
            }
            if stopping {
                let padding = client.GetCurrentPadding().unwrap_or(0);
                if (declick.is_silent() && padding == 0)
                    || std::time::Instant::now() >= stop_deadline
                {
                    let _ = client.Stop();
                    break;
                }
            }

            feed_rx.report(&display_name);
            // Pull any newly arrived chunks into the jitter buffer, converting
            // from the wire format to whatever this device actually wants.
            // Inbound chunks are always 48k stereo f32; the endpoint may be
            // 16-channel or 44.1 kHz, and feeding wire bytes straight into it
            // misaligns every frame (heard as noise).
            while let Ok(packet) = feed_rx.try_recv() {
                let (mut bytes, arrived) = (packet.data, packet.arrived);
                delay.arrival(
                    arrived,
                    bytes.len() / 8 * rate as usize / 48000,
                    Some(packet.timestamp),
                );
                max_feed_age = max_feed_age.max(arrived.elapsed());
                if let Some(previous) = last_feed {
                    max_feed_gap = max_feed_gap.max(arrived.duration_since(previous));
                }
                last_feed = Some(arrived);
                packets += 1;
                // Apply the logical gain to the wire-format f32 samples before
                // converting to the device format, so gain is independent of the
                // endpoint's mix format. Read the atomic once per chunk (knob
                // speed, not audio speed); skip the multiply at unity.
                let g = f32::from_bits(gain.load(std::sync::atomic::Ordering::Relaxed));
                if (g - 1.0).abs() > f32::EPSILON {
                    for s in bytes.as_chunks_mut::<4>().0 {
                        *s = (f32::from_le_bytes(*s) * g).to_le_bytes();
                    }
                }
                jitter.extend(from_wire_format_stream(
                    &bytes,
                    mix.nChannels,
                    mix.nSamplesPerSec,
                    &mut render_phase,
                ));
            }
            // Bound the backlog by discarding the oldest audio — see
            // RENDER_MAX_BYTES. Trim whole frames so the stream stays aligned
            // and channels don't swap.
            if jitter.len() > max_jitter_bytes {
                let frame = (mix.nChannels as usize) * (mix.wBitsPerSample as usize / 8);
                let mut excess = jitter.len() - max_jitter_bytes;
                if frame > 0 {
                    excess -= excess % frame;
                }
                if excess > 0 {
                    dropped_bytes += excess;
                    crate::declick::skip_crossfaded(&mut jitter, excess, channels, fade);
                    tracing::debug!(
                        dropped_bytes = excess,
                        "render jitter buffer over its limit; dropped stale audio to keep latency bounded"
                    );
                }
            }
            if !started && jitter.len() >= delay.frames() * frame_bytes {
                started = true;
            }

            // Top the device up to RENDER_AHEAD_MS, not to its whole buffer.
            let padding = match client.GetCurrentPadding() {
                Ok(padding) => padding,
                Err(error) => {
                    api_errors += 1;
                    if api_errors == 1 {
                        tracing::warn!(%error, "WASAPI GetCurrentPadding failed");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
            };
            let ahead = (mix.nSamplesPerSec * RENDER_AHEAD_MS / 1000).min(buffer_frames);
            let mut free_frames = ahead.saturating_sub(padding);
            if stopping {
                // Only the fade-out itself is still written; anything after it
                // would just delay the stop.
                free_frames = if declick.is_silent() {
                    0
                } else {
                    free_frames.min(fade as u32)
                };
            }
            if free_frames > 0 {
                let buffer = render.GetBuffer(free_frames);
                if let Err(error) = &buffer {
                    api_errors += 1;
                    if api_errors == 1 {
                        tracing::warn!(%error, free_frames, padding, "WASAPI GetBuffer failed");
                    }
                }
                if let Ok(p_data) = buffer {
                    if !p_data.is_null() {
                        if started && !stopping {
                            accelerated_bytes += recovery.process(
                                &mut jitter,
                                channels,
                                &delay,
                                free_frames as usize,
                                free_frames as usize,
                                now,
                            );
                        }

                        let want = free_frames as usize * frame_bytes;
                        let take = if started { want.min(jitter.len()) } else { 0 };
                        audio_frames += (take / frame_bytes) as u64;
                        keepalive_frames += ((want - take) / frame_bytes) as u64;
                        if started && take < want && !stopping {
                            underruns += 1;
                        }
                        // SAFETY: WASAPI guarantees `p_data` addresses
                        // free_frames * frame_bytes writable bytes until
                        // ReleaseBuffer.
                        let dst = std::slice::from_raw_parts_mut(p_data, want);
                        for (i, b) in jitter.drain(..take).enumerate() {
                            dst[i] = b;
                        }
                        // Emptying the jitter buffer re-arms the prefill below,
                        // so what follows is keep-alive: this has to end at zero.
                        declick.apply(&mut dst[..take], channels, stopping || jitter.is_empty());
                        // Fill the remainder (underrun / not started) with a
                        // signal far below hearing rather than with zeros.
                        //
                        // This is what keeps the application's row in the Windows
                        // volume mixer. Windows hides a session that is not
                        // playing, so a remote application that happens to be
                        // quiet loses the very control the user wants in order to
                        // set its volume *before* it makes a sound — and the set
                        // of visible rows looked arbitrary, since it depended on
                        // which applications had made a noise recently.
                        //
                        // The alternative — having the peer send real silence
                        // over the link — fills the shared channel with audio
                        // nobody hears, felt as mouse lag because input rides the
                        // same link; the sender drops silent chunks for exactly
                        // that reason. Generating it here costs nothing on the
                        // wire.
                        //
                        // The flag matters as much as the samples: releasing the
                        // buffer with AUDCLNT_BUFFERFLAGS_SILENT tells Windows the
                        // content is silence, and it treats the session as idle no
                        // matter what the bytes hold. So it is never set here.
                        fill_inaudible(&mut dst[take..], &mix, &mut keepalive_phase);
                        if let Err(error) = render.ReleaseBuffer(free_frames, 0) {
                            api_errors += 1;
                            if api_errors == 1 {
                                tracing::warn!(%error, "WASAPI ReleaseBuffer failed");
                            }
                        }
                        if started && jitter.is_empty() {
                            started = false; // re-arm prefill after a drain
                        }
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasapi_backend::format::SAMPLE_RATE;

    /// One application keeps one session across reconnects, and two never
    /// share one.
    ///
    /// Windows files a volume setting under the session GUID. Deriving it from
    /// anything per-run — a stream id, a counter — would hand Spotify a fresh
    /// mixer row on every reconnect and strand the volume the user had set;
    /// deriving it from nothing at all (the `None` this replaced) put every
    /// remote application in the process's default session, which is what
    /// collapsed them into a single row.
    #[test]
    fn a_session_is_stable_per_application_and_distinct_between_them() {
        let spotify = session_guid("Cursio(Nocturne): spotify");
        assert_eq!(
            spotify,
            session_guid("Cursio(Nocturne): spotify"),
            "the same application must land on the same mixer row every run"
        );
        assert_ne!(
            spotify,
            session_guid("Cursio(Nocturne): Firefox"),
            "two applications must not share a row"
        );
    }

    /// The keep-alive has no DC offset on any channel, and stays continuous
    /// across buffers.
    #[test]
    fn keepalive_alternates_per_frame_so_no_channel_carries_dc() {
        let mix = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 2,
            nSamplesPerSec: SAMPLE_RATE,
            wBitsPerSample: 32,
            nBlockAlign: 8,
            nAvgBytesPerSec: SAMPLE_RATE * 8,
            cbSize: 0,
        };
        let mut phase = false;
        let mut first = vec![0u8; 8 * 3];
        let mut second = vec![0u8; 8 * 3];
        fill_inaudible(&mut first, &mix, &mut phase);
        fill_inaudible(&mut second, &mix, &mut phase);

        let bytes: Vec<u8> = first.iter().chain(&second).copied().collect();
        let samples: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
        let right: Vec<f32> = samples.iter().skip(1).step_by(2).copied().collect();

        assert_eq!(left, right, "both channels of a frame must carry one value");
        assert!(
            left.windows(2).all(|w| w[0] == -w[1] && w[0] != 0.0),
            "each channel must alternate sign frame to frame, across the buffer boundary too: {left:?}"
        );
    }
}
