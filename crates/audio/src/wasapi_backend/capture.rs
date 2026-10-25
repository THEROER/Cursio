//! Capturing on WASAPI: the default output's loopback, one process's
//! loopback, or a microphone, converted onto the wire format.

use crate::wasapi_backend::devices::find_capture_device;
use crate::wasapi_backend::format::to_wire_format_stream;
use crate::wasapi_backend::format::wire_format;
use crate::wasapi_backend::format::CHANNELS;
use crate::wasapi_backend::format::SAMPLE_RATE;
use crate::wasapi_backend::sessions::activate_process_loopback;
use crate::wasapi_backend::ComGuard;
use crate::worker::{OnDrop, Worker};
use crate::AudioError;
use cursio_protocol::{AudioChunk, AudioCodec, AudioStreamId, AudioStreamMeta, StreamRole};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

/// Handle to a running WASAPI loopback capture (the **source** side).
pub struct WasapiSource {
    // First, so the thread is signalled before its channels close.
    _worker: Worker,
    id: AudioStreamId,
    adverts_rx: tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta>,
    chunks_rx: tokio::sync::mpsc::UnboundedReceiver<AudioChunk>,
}

impl WasapiSource {
    /// Start capturing the default render device's loopback (what's playing).
    pub fn start(id: AudioStreamId) -> Result<Self, AudioError> {
        Self::with_target(id, None, None)
    }

    /// Capture **one application's** audio via WASAPI process loopback, so the
    /// peer gets that app as its own stream. `pid` comes from
    /// [`list_app_streams`]. Requires Win10 20348+/Win11.
    pub fn app_tap(id: AudioStreamId, pid: u32, app_name: String) -> Result<Self, AudioError> {
        Self::with_target(id, Some(pid), Some(app_name))
    }

    /// Record a specific microphone (`Some(name)`, `Some("")` = default input),
    /// so the peer can use this machine's input as its own.
    pub fn mic_tap(id: AudioStreamId, target: Option<String>) -> Result<Self, AudioError> {
        // `Some("")` selects the default input; a named device is matched by
        // friendly name, the same way render devices are.
        Self::with_target_mic(id, None, None, Some(target.unwrap_or_default()))
    }

    fn with_target(
        id: AudioStreamId,
        pid: Option<u32>,
        app_name: Option<String>,
    ) -> Result<Self, AudioError> {
        Self::with_target_mic(id, pid, app_name, None)
    }

    fn with_target_mic(
        id: AudioStreamId,
        pid: Option<u32>,
        app_name: Option<String>,
        mic: Option<String>,
    ) -> Result<Self, AudioError> {
        let (adverts_tx, adverts_rx) = tokio::sync::mpsc::unbounded_channel();
        let (chunks_tx, chunks_rx) = tokio::sync::mpsc::unbounded_channel();

        // A microphone is offered as a source the peer can pull from; anything
        // else is something we are playing.
        let role = if mic.is_some() {
            StreamRole::Capture
        } else {
            StreamRole::Playback
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
            format!("cursio-wasapi-capture-{id}"),
            "capture thread",
            OnDrop::Detach,
            move |stop_rx, ready_tx| run_capture_loop(id, pid, mic, chunks_tx, stop_rx, ready_tx),
        )?;
        Ok(WasapiSource {
            _worker: worker,
            id,
            adverts_rx,
            chunks_rx,
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

/// Acquire the default render device's `IAudioClient` for loopback capture and
/// its mix format. The mix format is whatever the device runs at; we advertise
/// f32 stereo 48k and rely on the shared-mode mix being float (the common case),
/// converting later if a real device differs.
pub(super) unsafe fn init_loopback_client() -> Result<(IAudioClient, WAVEFORMATEX), String> {
    init_device_client(None)
}

/// Acquire an `IAudioClient` for capture, plus the device's mix format.
///
/// `mic` selects what to record:
/// - `None` — the default RENDER endpoint in **loopback**, i.e. what is playing.
/// - `Some(name)` — the CAPTURE endpoint whose friendly name contains `name`
///   (`Some("")` = the default input), recorded directly.
///
/// The two differ only in the endpoint direction and whether
/// `AUDCLNT_STREAMFLAGS_LOOPBACK` is set, so they share one implementation.
/// Passing the loopback flag on a capture endpoint is not a no-op — it asks for
/// a render device's monitor and fails here — which is why it is conditional.
pub(super) unsafe fn init_device_client(
    mic: Option<&str>,
) -> Result<(IAudioClient, WAVEFORMATEX), String> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .map_err(|e| format!("enumerator: {e}"))?;

    let device = match mic {
        None => enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("default endpoint: {e}"))?,
        Some("") => enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .map_err(|e| format!("default input endpoint: {e}"))?,
        Some(needle) => find_capture_device(&enumerator, needle)?,
    };

    let client: IAudioClient = device
        .Activate::<IAudioClient>(CLSCTX_ALL, None)
        .map_err(|e| format!("activate: {e}"))?;

    let mix_ptr = client
        .GetMixFormat()
        .map_err(|e| format!("mix format: {e}"))?;
    let mix = *mix_ptr;

    let flags = if mic.is_some() {
        Default::default()
    } else {
        AUDCLNT_STREAMFLAGS_LOOPBACK
    };

    // Shared mode, initialized with the device's own mix format.
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            // 200ms buffer, in 100ns units.
            200 * 10_000,
            0,
            mix_ptr,
            None,
        )
        .map_err(|e| format!("audio client init: {e}"))?;

    Ok((client, mix))
}

/// Initialize a **process-loopback** client for one pid.
///
/// Unlike device loopback there is no endpoint to ask for a mix format, so we
/// state the format we want (the wire format: 48 kHz stereo f32) and the audio
/// engine converts. `AUDCLNT_STREAMFLAGS_LOOPBACK` is *not* passed — the
/// activation params already make this a loopback capture.
pub(super) unsafe fn init_process_loopback_client(
    pid: u32,
) -> Result<(IAudioClient, WAVEFORMATEX), String> {
    /// `WAVE_FORMAT_IEEE_FLOAT` — 32-bit float PCM. Inlined rather than pulling
    /// in the whole `Win32_Media_Multimedia` feature for one constant.
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

    let client = activate_process_loopback(pid)?;

    let fmt = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        wBitsPerSample: 32,
        nBlockAlign: CHANNELS * 4,
        nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * 4,
        cbSize: 0,
    };

    // Flags and duration follow Microsoft's ApplicationLoopback sample:
    // LOOPBACK is still required, and **AUTOCONVERTPCM** is what lets us ask for
    // our own format instead of the engine's (without it, Initialize rejects
    // anything but the native one — that was AUDCLNT_E_UNSUPPORTED_FORMAT,
    // 0x88890021). Buffer duration must be 0 here, unlike the device path.
    // We omit EVENTCALLBACK deliberately: that would require SetEventHandle and
    // an event-driven loop, while this backend polls like its device sibling.
    use windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            0,
            0,
            &fmt,
            None,
        )
        .map_err(|e| format!("process-loopback init: {e}"))?;

    Ok((client, fmt))
}

pub(super) fn run_capture_loop(
    id: AudioStreamId,
    pid: Option<u32>,
    // Which microphone to record, if this is a mic tap rather than loopback.
    // `Some("")` means the default input; `None` means "not a mic tap".
    mic: Option<String>,
    chunks_tx: tokio::sync::mpsc::UnboundedSender<AudioChunk>,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
) {
    let _com = match ComGuard::new() {
        Ok(g) => g,
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
            return;
        }
    };

    // SAFETY: all WASAPI calls below run on this COM-initialized thread.
    let result: Result<(), String> = unsafe {
        let init = match (pid, &mic) {
            (Some(p), _) => init_process_loopback_client(p),
            (None, Some(name)) => init_device_client(Some(name.as_str())),
            (None, None) => init_loopback_client(),
        };
        let (client, mix) = match init {
            Ok(v) => v,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        let capture: IAudioCaptureClient = match client.GetService() {
            Ok(c) => c,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("capture service: {e}")));
                return;
            }
        };

        if let Err(e) = client.Start() {
            let _ = ready_tx.send(Err(format!("client start: {e}")));
            return;
        }

        let frame_bytes = mix.nBlockAlign as usize;
        let _ = ready_tx.send(Ok(()));

        let mut seq = 0u32;
        let mut ts = 0u64;
        let mut capture_phase = 0u64;
        // Set when the device fails; ends the outer loop too.
        let mut return_err: Option<String> = None;

        loop {
            if return_err.is_some() {
                break Err(return_err.take().unwrap_or_default());
            }
            if stop_rx.try_recv().is_ok() {
                let _ = client.Stop();
                break Ok(());
            }

            // Drain all currently available packets.
            //
            // A device error here is terminal, not transient: the endpoint is
            // gone (AUDCLNT_E_DEVICE_INVALIDATED = 0x88890004 when a device is
            // unplugged or reconfigured). Retrying just repeats the same failure
            // — it logged 9699 identical lines in one session — so stop the
            // capture instead.
            loop {
                let avail = match capture.GetNextPacketSize() {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = client.Stop();
                        tracing::warn!(error = %e, "wasapi capture ended: device gone");
                        return_err = Some(format!("next packet size: {e}"));
                        break;
                    }
                };
                if avail == 0 {
                    break;
                }

                let mut p_data: *mut u8 = std::ptr::null_mut();
                let mut num_frames = 0u32;
                let mut flags = 0u32;
                if let Err(e) =
                    capture.GetBuffer(&mut p_data, &mut num_frames, &mut flags, None, None)
                {
                    let _ = client.Stop();
                    tracing::warn!(error = %e, "wasapi capture ended: cannot get buffer");
                    return_err = Some(format!("get buffer: {e}"));
                    break;
                }

                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                if num_frames > 0 && (silent || !p_data.is_null()) {
                    let data = if silent {
                        let (frames, _) = crate::sample_clock::layout(
                            num_frames as usize,
                            mix.nSamplesPerSec,
                            SAMPLE_RATE,
                            &mut capture_phase,
                        );
                        vec![0; frames * CHANNELS as usize * 4]
                    } else {
                        let byte_len = num_frames as usize * frame_bytes;
                        // SAFETY: a non-silent packet holds num_frames complete frames
                        // until ReleaseBuffer. Silent packets must not read p_data.
                        let src = std::slice::from_raw_parts(p_data, byte_len);
                        to_wire_format_stream(
                            src,
                            mix.nChannels,
                            mix.nSamplesPerSec,
                            &mut capture_phase,
                        )
                    };
                    let wire_frames = data.len() / (CHANNELS as usize * 4);
                    let _ = chunks_tx.send(AudioChunk {
                        id,
                        seq,
                        timestamp: ts,
                        data,
                    });
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(wire_frames as u64);
                }

                let _ = capture.ReleaseBuffer(num_frames);
            }

            std::thread::sleep(Duration::from_millis(5));
        }
    };

    if let Err(e) = result {
        tracing::error!("wasapi capture loop error: {e}");
    }
}

// ---------------------------------------------------------------------------
// Playback (render / sink) side
// ---------------------------------------------------------------------------
