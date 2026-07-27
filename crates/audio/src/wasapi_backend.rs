//! Windows audio backend via WASAPI.
//!
//! The Windows counterpart to the PipeWire backend: [`WasapiSource`] captures
//! the default render device in **loopback** mode (what's playing) and
//! [`WasapiSink`] renders inbound audio to the default output. Symmetric shape
//! to `pipewire_backend`: each stream owns a dedicated OS thread (WASAPI + COM
//! are apartment/thread-bound), bridged to the async world by channels.
//!
//! First cut: **whole-device, RawPcm f32**, matching the wire format the Linux
//! side already produces (48 kHz stereo f32). Per-app capture (WASAPI *process*
//! loopback, Win10 2004+) and Opus are later milestones; the trait shape already
//! accommodates them.

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use cursio_protocol::{
    AudioChunk, AudioCodec, AudioFormat, AudioStreamId, AudioStreamMeta, SampleKind, StreamRole,
};
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioRenderClient,
    IAudioSessionControl, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, StructuredStorage::PropVariantToStringAlloc,
    CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

use crate::AudioError;

/// `PKEY_Device_FriendlyName` — the human name shown in Sound settings (e.g.
/// "VB-Audio Virtual Cable"). Used to find a device by name. Defined here
/// because the constant isn't re-exported by the `windows` crate in a stable
/// path; the GUID/pid are from `functiondiscoverykeys_devpkey.h`.
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

/// Find a render (output) device whose friendly name contains `needle`
/// (case-insensitive). Returns the default endpoint when `needle` is `None`.
///
/// Lets Cursio render the peer's audio into a *specific* virtual device (e.g. a
/// VB-Audio cable) so it appears/behaves as that endpoint on Windows, since
/// Windows can't create endpoints from user space (see docs/audio-design.md).
unsafe fn find_render_device(
    enumerator: &IMMDeviceEnumerator,
    needle: Option<&str>,
) -> Result<IMMDevice, String> {
    let Some(needle) = needle else {
        return enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("default endpoint: {e}"));
    };
    let needle_low = needle.to_lowercase();

    let collection = enumerator
        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        .map_err(|e| format!("enum endpoints: {e}"))?;
    let count = collection.GetCount().map_err(|e| format!("count: {e}"))?;
    for i in 0..count {
        let dev = match collection.Item(i) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let Ok(name) = device_friendly_name(&dev) {
            if name.to_lowercase().contains(&needle_low) {
                tracing::info!(%name, "matched render device");
                return Ok(dev);
            }
        }
    }
    Err(format!("no active render device matching '{needle}'"))
}

/// List active render (output) endpoints for mirroring to the peer.
///
/// Skips Cursio's own mirror devices so we never mirror a mirror (which would
/// loop audio back and forth). See [`crate::list_output_devices`].
pub fn list_output_devices() -> Result<Vec<crate::OutputDevice>, AudioError> {
    list_endpoints(eRender)
}

/// List this machine's capture endpoints (microphones, line-ins).
///
/// The mirror image of [`list_output_devices`]: on Windows an input device is
/// the same enumeration with `eCapture` instead of `eRender`, so both share one
/// implementation.
pub fn list_input_devices() -> Result<Vec<crate::OutputDevice>, AudioError> {
    list_endpoints(eCapture)
}

/// Enumerate active endpoints in one direction, skipping our own mirrors.
fn list_endpoints(
    direction: windows::Win32::Media::Audio::EDataFlow,
) -> Result<Vec<crate::OutputDevice>, AudioError> {
    let _com = ComGuard::new()?;
    // SAFETY: COM is initialized on this thread for the duration of the guard.
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| AudioError::Backend(format!("enumerator: {e}")))?;
        let collection = enumerator
            .EnumAudioEndpoints(direction, DEVICE_STATE_ACTIVE)
            .map_err(|e| AudioError::Backend(format!("enum endpoints: {e}")))?;
        let count = collection
            .GetCount()
            .map_err(|e| AudioError::Backend(format!("count: {e}")))?;

        let mut out = Vec::new();
        for i in 0..count {
            let Ok(dev) = collection.Item(i) else { continue };
            let Ok(name) = device_friendly_name(&dev) else {
                continue;
            };
            if name.to_lowercase().contains(crate::MIRROR_MARKER) {
                continue; // one of our own mirrors
            }
            out.push(crate::OutputDevice {
                // On Windows the friendly name is what `open_device` matches on.
                id: name.clone(),
                description: name,
            });
        }
        Ok(out)
    }
}

/// List applications currently playing audio, via the default render device's
/// audio sessions.
///
/// Windows models each playing app as an *audio session* on an endpoint;
/// `IAudioSessionManager2` enumerates them and each session exposes the owning
/// process id, which we resolve to a process name. The pid is the stream's
/// identity (Windows per-app *capture* later uses process loopback, keyed by
/// exactly this pid).
///
/// Cursio's own sessions are skipped so we never capture our own playback.
pub fn list_app_streams() -> Result<Vec<crate::AppStream>, AudioError> {
    let _com = ComGuard::new()?;
    // SAFETY: COM is initialized on this thread for the duration of the guard.
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| AudioError::Backend(format!("enumerator: {e}")))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| AudioError::Backend(format!("default endpoint: {e}")))?;
        let manager: IAudioSessionManager2 = device
            .Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)
            .map_err(|e| AudioError::Backend(format!("session manager: {e}")))?;
        let sessions = manager
            .GetSessionEnumerator()
            .map_err(|e| AudioError::Backend(format!("session enumerator: {e}")))?;
        let count = sessions
            .GetCount()
            .map_err(|e| AudioError::Backend(format!("session count: {e}")))?;

        let mut out = Vec::new();
        for i in 0..count {
            let Ok(ctl) = sessions.GetSession(i) else {
                continue;
            };
            // SessionControl2 exposes the owning process id.
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let pid = ctl2.GetProcessId().unwrap_or(0);
            if pid == 0 {
                continue; // system/shared session, not an app
            }
            // Prefer the session's *display name* when it has one: that's what
            // the Windows mixer shows, and it's how Cursio labels its own
            // per-stream sessions ("Cursio(Peer): Firefox"). Fall back to the
            // owning process name for ordinary apps, which set no display name.
            let display = ctl2
                .GetDisplayName()
                .ok()
                .and_then(|p| {
                    let s = p.to_string().ok();
                    windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
                    s
                })
                .filter(|s| !s.is_empty());
            // Skip our OWN sessions by process id, not by name: our streams are
            // deliberately named "Cursio(Peer): …", so a name-based filter would
            // hide exactly the rows we want to see (and still wouldn't catch a
            // differently-named one). The pid is unambiguous.
            if pid == std::process::id() {
                continue;
            }
            let name = display
                .or_else(|| process_name(pid))
                .unwrap_or_else(|| format!("pid {pid}"));
            out.push(crate::AppStream {
                id: pid.to_string(),
                app_name: name,
            });
        }
        Ok(out)
    }
}

/// Resolve a process id to its executable name (without path/extension).
fn process_name(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: handle is closed below; buffer length is passed and updated.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if !ok {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Some(
            full.rsplit('\\')
                .next()
                .unwrap_or(&full)
                .trim_end_matches(".exe")
                .to_string(),
        )
    }
}

// ---------------------------------------------------------------------------
// Per-application capture (WASAPI process loopback)
// ---------------------------------------------------------------------------

/// Completion handler for [`ActivateAudioInterfaceAsync`].
///
/// Process-loopback activation is **asynchronous**: the call returns an
/// operation and the result arrives on this callback. We just signal an event so
/// the caller can wait — everything else stays synchronous, matching how the
/// rest of this backend is written.
#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _op: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let (lock, cv) = &*self.done;
        if let Ok(mut flag) = lock.lock() {
            *flag = true;
            cv.notify_all();
        }
        Ok(())
    }
}

/// Activate an `IAudioClient` that captures **only** the given process's audio.
///
/// This is the Windows answer to per-app sharing: unlike normal loopback (which
/// grabs the whole endpoint mix), process loopback restricts capture to one
/// process tree. Requires Windows 10 build 20348+ / Win11.
unsafe fn activate_process_loopback(pid: u32) -> Result<IAudioClient, String> {
    use windows::Win32::Media::Audio::{
        AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
        AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };

    // The params travel inside a PROPVARIANT as a blob (VT_BLOB).
    //
    // Wrapped in ManuallyDrop on purpose: `PROPVARIANT`'s Drop calls
    // `PropVariantClear`, which would try to *free* our blob pointer — but it
    // points at a stack local, so freeing it corrupts the heap (we hit exit
    // 0xC0000374 twice before spotting this). The blob is borrowed for the
    // duration of the call only; nothing here owns heap memory to release.
    let mut prop = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    {
        let inner = &mut *prop.Anonymous.Anonymous;
        inner.vt = windows::Win32::System::Variant::VT_BLOB;
        inner.wReserved1 = 0;
        inner.wReserved2 = 0;
        inner.wReserved3 = 0;
        inner.Anonymous.blob = windows::Win32::System::Com::BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &mut params as *mut _ as *mut u8,
        };
    }

    let done = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler { done: done.clone() }.into();

    let op = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some(&*prop),
        &handler,
    )
    .map_err(|e| format!("ActivateAudioInterfaceAsync: {e}"))?;

    // Wait for the handler (bounded, so a stuck activation can't hang us).
    {
        let (lock, cv) = &*done;
        let mut flag = lock.lock().map_err(|_| "activation mutex poisoned")?;
        let deadline = Duration::from_secs(3);
        while !*flag {
            let (guard, timeout) = cv
                .wait_timeout(flag, deadline)
                .map_err(|_| "activation wait failed")?;
            flag = guard;
            if timeout.timed_out() {
                return Err("process-loopback activation timed out".into());
            }
        }
    }

    let mut result = windows::core::HRESULT(0);
    let mut client: Option<windows::core::IUnknown> = None;
    op.GetActivateResult(&mut result, &mut client)
        .map_err(|e| format!("GetActivateResult: {e}"))?;
    result.ok().map_err(|e| format!("activation failed: {e}"))?;
    client
        .ok_or_else(|| "activation returned no interface".to_string())?
        .cast::<IAudioClient>()
        .map_err(|e| format!("cast to IAudioClient: {e}"))
}

/// Read a device's friendly name via its property store.
unsafe fn device_friendly_name(dev: &IMMDevice) -> Result<String, String> {
    let store = dev
        .OpenPropertyStore(STGM_READ)
        .map_err(|e| format!("open property store: {e}"))?;
    let prop = store
        .GetValue(&PKEY_DEVICE_FRIENDLY_NAME)
        .map_err(|e| format!("get name: {e}"))?;
    let pwstr = PropVariantToStringAlloc(&prop).map_err(|e| format!("name to string: {e}"))?;
    let s = pwstr.to_string().map_err(|e| format!("utf16: {e}"))?;
    // Free the allocation PropVariantToStringAlloc handed us.
    windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0 as *const _));
    Ok(s)
}

/// The wire format this backend targets: 48 kHz stereo f32, same as Linux.
///
/// Devices really do differ from this — a VB-CABLE endpoint enumerates as
/// "CABLE In 16ch", and plenty of hardware runs at 44.1 kHz. Capture is
/// initialized with the device's own mix format (that is what WASAPI shared
/// mode gives us), so [`to_wire_format`] converts each buffer to the format we
/// advertise. Sending device bytes under a 48k-stereo label is what made a
/// 16-channel endpoint arrive as noise.
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;

/// Convert one buffer from the wire format (48 kHz stereo f32) into the
/// device's mix format, for rendering.
///
/// The mirror image of [`to_wire_format`]: extra channels are filled by
/// repeating the stereo pair (so a multi-channel endpoint isn't left with
/// garbage in the other channels), and the rate is nearest-neighbour resampled.
fn from_wire_format(src: &[u8], dst_channels: u16, dst_rate: u32) -> Vec<u8> {
    if dst_channels == CHANNELS && dst_rate == SAMPLE_RATE {
        return src.to_vec();
    }
    if dst_channels == 0 {
        return Vec::new();
    }

    let samples: Vec<f32> = src
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let src_frames = samples.len() / CHANNELS as usize;
    if src_frames == 0 {
        return Vec::new();
    }

    let dst_frames = if dst_rate == SAMPLE_RATE {
        src_frames
    } else {
        ((src_frames as u64 * dst_rate as u64) / SAMPLE_RATE as u64) as usize
    };

    let mut out = Vec::with_capacity(dst_frames * dst_channels as usize * 4);
    for i in 0..dst_frames {
        let src_i = if dst_rate == SAMPLE_RATE {
            i
        } else {
            (i as u64 * SAMPLE_RATE as u64 / dst_rate.max(1) as u64) as usize
        };
        let base = src_i.min(src_frames - 1) * CHANNELS as usize;
        let (left, right) = (samples[base], samples[base + 1]);

        if dst_channels == 1 {
            // A mono endpoint must hear BOTH channels. Taking only the left
            // silently drops everything panned right — anything hard-panned
            // vanishes outright. Average rather than sum so the result cannot
            // clip when both channels are loud.
            let v = (left + right) * 0.5;
            out.extend_from_slice(&v.to_le_bytes());
            continue;
        }
        for ch in 0..dst_channels {
            // Alternate L/R across however many channels the device has.
            let v = if ch % 2 == 0 { left } else { right };
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// Convert one captured buffer from the device's mix format into the wire
/// format (48 kHz stereo f32 little-endian).
///
/// Handles the two ways a device commonly differs:
/// - **Channel count** — extra channels are dropped and mono is duplicated, so
///   the frame size matches what the peer expects. Without this, a 16-channel
///   frame read as 2 channels slides the stream out of alignment every frame,
///   which is heard as continuous noise.
/// - **Sample rate** — nearest-neighbour resampling to 48 kHz. Cheap and
///   artefact-prone compared to a proper filter, but correct in pitch and
///   duration; a real resampler belongs with the Opus milestone.
///
/// Sample *kind* is not converted: WASAPI shared mode mixes to float, so the
/// mix format is f32 in practice. A non-float device is left alone rather than
/// silently mangled — better a wrong-sounding stream than a wrong-looking one.
fn to_wire_format(src: &[u8], src_channels: u16, src_rate: u32) -> Vec<u8> {
    // Already what we want: hand the bytes straight through.
    if src_channels == CHANNELS && src_rate == SAMPLE_RATE {
        return src.to_vec();
    }
    if src_channels == 0 {
        return Vec::new();
    }

    let samples: Vec<f32> = src
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let src_frames = samples.len() / src_channels as usize;
    if src_frames == 0 {
        return Vec::new();
    }

    // Resample by frame index so the output lasts the right amount of time.
    let dst_frames = if src_rate == SAMPLE_RATE {
        src_frames
    } else {
        ((src_frames as u64 * SAMPLE_RATE as u64) / src_rate.max(1) as u64) as usize
    };

    let mut out = Vec::with_capacity(dst_frames * CHANNELS as usize * 4);
    for i in 0..dst_frames {
        let src_i = if src_rate == SAMPLE_RATE {
            i
        } else {
            (i as u64 * src_rate as u64 / SAMPLE_RATE as u64) as usize
        };
        let base = src_i.min(src_frames - 1) * src_channels as usize;

        // Take the first two channels; duplicate when the source is mono.
        let left = samples[base];
        let right = if src_channels >= 2 { samples[base + 1] } else { left };
        out.extend_from_slice(&left.to_le_bytes());
        out.extend_from_slice(&right.to_le_bytes());
    }
    out
}

pub(crate) fn wire_format() -> AudioFormat {
    AudioFormat {
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS as u8,
        sample_kind: SampleKind::F32,
    }
}

/// RAII guard: `CoInitializeEx` on this thread, `CoUninitialize` on drop. WASAPI
/// calls must run on a COM-initialized thread.
struct ComGuard;

impl ComGuard {
    fn new() -> Result<Self, AudioError> {
        // SAFETY: called once per backend thread; matched by CoUninitialize in
        // Drop. MULTITHREADED so we don't need a message pump.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| AudioError::Backend(format!("CoInitializeEx: {e}")))?;
        }
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances the CoInitializeEx in `new` on the same thread.
        unsafe { CoUninitialize() };
    }
}

/// Handle to a running WASAPI loopback capture (the **source** side).
pub struct WasapiSource {
    id: AudioStreamId,
    adverts_rx: tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta>,
    chunks_rx: tokio::sync::mpsc::UnboundedReceiver<AudioChunk>,
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
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
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();

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

        let thread = std::thread::Builder::new()
            .name(format!("cursio-wasapi-capture-{id}"))
            .spawn(move || run_capture_loop(id, pid, mic, chunks_tx, stop_rx, ready_tx))
            .map_err(|e| AudioError::Backend(format!("spawn capture thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(WasapiSource {
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

impl Drop for WasapiSource {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Acquire the default render device's `IAudioClient` for loopback capture and
/// its mix format. The mix format is whatever the device runs at; we advertise
/// f32 stereo 48k and rely on the shared-mode mix being float (the common case),
/// converting later if a real device differs.
unsafe fn init_loopback_client() -> Result<(IAudioClient, WAVEFORMATEX), String> {
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
unsafe fn init_device_client(mic: Option<&str>) -> Result<(IAudioClient, WAVEFORMATEX), String> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("enumerator: {e}"))?;

    let device = match mic {
        None => enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("default endpoint: {e}"))?,
        Some(needle) if needle.is_empty() => enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .map_err(|e| format!("default input endpoint: {e}"))?,
        Some(needle) => find_capture_device(&enumerator, needle)?,
    };

    let client: IAudioClient = device
        .Activate::<IAudioClient>(CLSCTX_ALL, None)
        .map_err(|e| format!("activate: {e}"))?;

    let mix_ptr = client.GetMixFormat().map_err(|e| format!("mix format: {e}"))?;
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

/// Find an active capture endpoint whose friendly name contains `needle`.
///
/// The capture-side twin of [`find_render_device`], used so a specific
/// microphone can be shared rather than only the default input.
unsafe fn find_capture_device(
    enumerator: &IMMDeviceEnumerator,
    needle: &str,
) -> Result<IMMDevice, String> {
    let needle_low = needle.to_lowercase();
    let collection = enumerator
        .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
        .map_err(|e| format!("enum capture endpoints: {e}"))?;
    let count = collection.GetCount().map_err(|e| format!("count: {e}"))?;
    for i in 0..count {
        let Ok(dev) = collection.Item(i) else { continue };
        if let Ok(name) = device_friendly_name(&dev) {
            if name.to_lowercase().contains(&needle_low) {
                tracing::info!(%name, "matched capture device");
                return Ok(dev);
            }
        }
    }
    Err(format!("no active capture device matching '{needle}'"))
}

/// Initialize a **process-loopback** client for one pid.
///
/// Unlike device loopback there is no endpoint to ask for a mix format, so we
/// state the format we want (the wire format: 48 kHz stereo f32) and the audio
/// engine converts. `AUDCLNT_STREAMFLAGS_LOOPBACK` is *not* passed — the
/// activation params already make this a loopback capture.
unsafe fn init_process_loopback_client(pid: u32) -> Result<(IAudioClient, WAVEFORMATEX), String> {
    /// `WAVE_FORMAT_IEEE_FLOAT` — 32-bit float PCM. Inlined rather than pulling
    /// in the whole `Win32_Media_Multimedia` feature for one constant.
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

    let client = activate_process_loopback(pid)?;

    let mut fmt = WAVEFORMATEX {
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
            &mut fmt,
            None,
        )
        .map_err(|e| format!("process-loopback init: {e}"))?;

    Ok((client, fmt))
}

fn run_capture_loop(
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

                if num_frames > 0 && !p_data.is_null() {
                    let byte_len = num_frames as usize * frame_bytes;
                    // SAFETY: WASAPI guarantees p_data points to num_frames *
                    // frame_bytes valid bytes until ReleaseBuffer.
                    let src = std::slice::from_raw_parts(p_data, byte_len);
                    // Convert to the format we advertised. The device's mix
                    // format is whatever Windows chose (16 channels on a
                    // VB-CABLE endpoint, 44.1 kHz on plenty of hardware); the
                    // peer decodes strictly as 48k stereo f32.
                    let data = to_wire_format(src, mix.nChannels, mix.nSamplesPerSec);
                    let _ = chunks_tx.send(AudioChunk {
                        id,
                        seq,
                        timestamp: ts,
                        data,
                    });
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(num_frames as u64);
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

/// Helper: stop the client and produce an error to break the capture loop with.

// ---------------------------------------------------------------------------
// Playback (render / sink) side
// ---------------------------------------------------------------------------

/// Bytes to prefill the jitter buffer before render starts (~40ms at the common
/// mix format). Small on purpose — see the PipeWire sink for the rationale.
const RENDER_PREFILL_BYTES: usize = 4 * 480 * 8;

/// Hard ceiling on the render jitter buffer: ~200 ms at the common mix format.
///
/// Mirrors the PipeWire sink — an unbounded queue converts a rate mismatch into
/// unbounded latency, because nothing ever removes the backlog. Beyond this the
/// OLDEST audio is dropped: live audio wants freshness, not completeness.
const RENDER_MAX_BYTES: usize = 48_000 / 5 * 8;

/// A running WASAPI render node (the **sink** side).
///
/// Inbound [`AudioChunk`]s are pushed into a shared byte queue drained by the
/// render thread, which fills the device buffer's free space each pass and
/// writes silence on underrun. Symmetric to the PipeWire `PwSink`.
pub struct WasapiSink {
    id: AudioStreamId,
    feed: std_mpsc::Sender<Vec<u8>>,
    /// Logical gain (linear, 1.0 = unity) applied to inbound audio, published to
    /// the render thread via an atomic (f32 bits) so `set_gain` never blocks it.
    /// The per-stream gain of the "one system" model — above the OS mixer,
    /// distinct from the device's single replicated volume. Applied to wire-format
    /// f32 samples before the device-format conversion, so it is independent of
    /// the endpoint's mix format.
    gain: std::sync::Arc<std::sync::atomic::AtomicU32>,
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
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
        let (feed_tx, feed_rx) = std_mpsc::channel::<Vec<u8>>();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();

        let gain = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(1.0f32.to_bits()));
        let gain_thread = std::sync::Arc::clone(&gain);

        let thread = std::thread::Builder::new()
            .name(format!("cursio-wasapi-render-{id}"))
            .spawn(move || {
                run_render_loop(device_name, display_name, feed_rx, stop_rx, ready_tx, gain_thread)
            })
            .map_err(|e| AudioError::Backend(format!("spawn render thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(WasapiSink {
                id,
                feed: feed_tx,
                gain,
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(AudioError::Backend(e)),
            Err(_) => Err(AudioError::Backend(
                "render thread exited before signalling readiness".into(),
            )),
        }
    }

    /// Enqueue one inbound chunk for playback.
    pub fn play(&self, chunk: &AudioChunk) -> Result<(), AudioError> {
        self.feed
            .send(chunk.data.clone())
            .map_err(|_| AudioError::Backend("render thread gone".into()))
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

impl Drop for WasapiSink {
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
// Device-state monitor (volume/mute replication — docs/audio-design.md phase 5)
// ---------------------------------------------------------------------------

/// An observed change to a real device's volume/mute. The Windows twin of the
/// PipeWire `DeviceChange`; the `lib.rs` façade unifies the two.
#[derive(Debug, Clone)]
pub struct DeviceChange {
    /// The device's friendly name — its Windows-side handle, matched to the
    /// peer's Intent Sink (this is what `list_output_devices` advertises as `id`).
    pub node_name: String,
    /// Master volume, linear `0.0`..=`1.0` (WASAPI's scalar is already linear).
    pub volume: f32,
    /// Whether the device is muted.
    pub mute: bool,
}

/// A command to set a real device's volume/mute, sent to a [`WasapiDeviceMonitor`].
enum DeviceCommand {
    SetVolume { node_name: String, volume: f32 },
    SetMute { node_name: String, mute: bool },
}

/// Watches this machine's render endpoints' volume/mute and applies sets — the
/// Windows half of device-state replication (see docs/audio-design.md, "Device
/// state is REPLICATED"). Mirrors the PipeWire `PwDeviceMonitor` API.
///
/// Unlike the PipeWire backend (event-driven on the graph), this **polls** each
/// endpoint's [`IAudioEndpointVolume`] on a short interval and emits a change
/// when the scalar volume or mute flag moves. Polling avoids the COM
/// apartment/lifetime pitfalls of `RegisterControlChangeNotify` callbacks, and a
/// volume knob changes far slower than the poll, so nothing perceptible is lost.
pub struct WasapiDeviceMonitor {
    changes_rx: tokio::sync::mpsc::UnboundedReceiver<DeviceChange>,
    commands: std_mpsc::Sender<DeviceCommand>,
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WasapiDeviceMonitor {
    /// Start the monitor. Fails only if the monitor thread can't be spawned;
    /// per-device COM errors are logged and skipped, not fatal.
    pub fn start() -> Result<Self, AudioError> {
        let (changes_tx, changes_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<DeviceCommand>();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();

        let thread = std::thread::Builder::new()
            .name("cursio-wasapi-device-monitor".to_string())
            .spawn(move || run_device_monitor_loop(changes_tx, cmd_rx, stop_rx))
            .map_err(|e| AudioError::Backend(format!("spawn device-monitor thread: {e}")))?;

        Ok(WasapiDeviceMonitor {
            changes_rx,
            commands: cmd_tx,
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }

    pub fn changes(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<DeviceChange> {
        &mut self.changes_rx
    }

    pub fn set_volume(&self, node_name: String, volume: f32) {
        let _ = self
            .commands
            .send(DeviceCommand::SetVolume { node_name, volume });
    }

    pub fn set_mute(&self, node_name: String, mute: bool) {
        let _ = self.commands.send(DeviceCommand::SetMute { node_name, mute });
    }
}

impl Drop for WasapiDeviceMonitor {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// How often the monitor polls each endpoint's volume/mute. A knob turn changes
/// far slower than this, so ~5/s reflects promptly without spinning.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Open the `IAudioEndpointVolume` for a render endpoint by friendly name.
///
/// Returns `None` (not an error) when the device isn't found or can't be
/// activated — the monitor logs and skips rather than failing the whole poll.
unsafe fn endpoint_volume_by_name(
    enumerator: &IMMDeviceEnumerator,
    name: &str,
) -> Option<IAudioEndpointVolume> {
    let collection = enumerator
        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        .ok()?;
    let count = collection.GetCount().ok()?;
    let name_low = name.to_lowercase();
    for i in 0..count {
        let Ok(dev) = collection.Item(i) else { continue };
        let Ok(dev_name) = device_friendly_name(&dev) else {
            continue;
        };
        if dev_name.to_lowercase() == name_low {
            return dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok();
        }
    }
    None
}

/// The monitor loop: poll every render endpoint's volume/mute, emit changes, and
/// apply queued sets. Runs on its own COM-initialized thread.
fn run_device_monitor_loop(
    changes_tx: tokio::sync::mpsc::UnboundedSender<DeviceChange>,
    cmd_rx: std_mpsc::Receiver<DeviceCommand>,
    stop_rx: std_mpsc::Receiver<()>,
) {
    let _com = match ComGuard::new() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "device monitor: COM init failed; no device-state replication");
            return;
        }
    };

    // SAFETY: all WASAPI/COM calls run on this COM-initialized thread.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "device monitor: no enumerator");
                    return;
                }
            };

        // Last-seen (volume, mute) per friendly name, so we emit only real
        // transitions — the poll would otherwise re-send an unchanged value
        // every tick and (with the session's echo handling) could ping-pong.
        let mut last: std::collections::HashMap<String, (f32, bool)> =
            std::collections::HashMap::new();

        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }

            // Apply any pending set commands first, so a just-requested value is
            // reflected on this same tick rather than a poll later.
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    DeviceCommand::SetVolume { node_name, volume } => {
                        if let Some(vol) = endpoint_volume_by_name(&enumerator, &node_name) {
                            if let Err(e) =
                                vol.SetMasterVolumeLevelScalar(volume.clamp(0.0, 1.0), std::ptr::null())
                            {
                                tracing::debug!(device = %node_name, error = %e, "set volume failed");
                            }
                        }
                    }
                    DeviceCommand::SetMute { node_name, mute } => {
                        if let Some(vol) = endpoint_volume_by_name(&enumerator, &node_name) {
                            if let Err(e) = vol.SetMute(mute, std::ptr::null()) {
                                tracing::debug!(device = %node_name, error = %e, "set mute failed");
                            }
                        }
                    }
                }
            }

            // Poll every active render endpoint.
            if let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
                if let Ok(count) = collection.GetCount() {
                    for i in 0..count {
                        let Ok(dev) = collection.Item(i) else { continue };
                        let Ok(name) = device_friendly_name(&dev) else {
                            continue;
                        };
                        // Skip our own mirrors: they carry no real device volume.
                        if name.to_lowercase().contains(crate::MIRROR_MARKER) {
                            continue;
                        }
                        let Ok(vol) = dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) else {
                            continue;
                        };
                        let volume = vol.GetMasterVolumeLevelScalar().unwrap_or(1.0);
                        let mute = vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
                        let state = (volume, mute);
                        if last.get(&name) == Some(&state) {
                            continue; // unchanged since last poll
                        }
                        last.insert(name.clone(), state);
                        let _ = changes_tx.send(DeviceChange {
                            node_name: name,
                            volume,
                            mute,
                        });
                    }
                }
            }

            std::thread::sleep(DEVICE_POLL_INTERVAL);
        }
    }
}

/// Acquire a render device's `IAudioClient` for shared-mode playback, plus its
/// mix format and total buffer frame count. `device_name = None` uses the
/// default endpoint; `Some(name)` matches a device by friendly name.
unsafe fn init_render_client(
    device_name: Option<&str>,
) -> Result<(IAudioClient, WAVEFORMATEX, u32), String> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("enumerator: {e}"))?;
    let device = find_render_device(&enumerator, device_name)?;
    let client: IAudioClient = device
        .Activate::<IAudioClient>(CLSCTX_ALL, None)
        .map_err(|e| format!("activate: {e}"))?;

    let mix_ptr = client.GetMixFormat().map_err(|e| format!("mix format: {e}"))?;
    let mix = *mix_ptr;

    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            0,
            200 * 10_000, // 200ms buffer, 100ns units
            0,
            mix_ptr,
            None,
        )
        .map_err(|e| format!("audio client init: {e}"))?;

    let buffer_frames = client
        .GetBufferSize()
        .map_err(|e| format!("buffer size: {e}"))?;

    Ok((client, mix, buffer_frames))
}

fn run_render_loop(
    device_name: Option<String>,
    display_name: String,
    feed_rx: std_mpsc::Receiver<Vec<u8>>,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
    gain: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    use std::collections::VecDeque;

    let _com = match ComGuard::new() {
        Ok(g) => g,
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
            return;
        }
    };

    // SAFETY: all WASAPI calls run on this COM-initialized thread.
    unsafe {
        let (client, mix, buffer_frames) = match init_render_client(device_name.as_deref()) {
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

        // Label this stream in the Windows volume mixer and give it its OWN row.
        //
        // Each IAudioClient is its own audio session, but the mixer groups
        // sessions by their *grouping GUID* — and every session we create is
        // owned by the same process (cursio.exe), so Windows lumped them all
        // under one grouping and showed a single "RemoteApp" row instead of one
        // per peer app. SetDisplayName alone can't split them: the mixer takes
        // the label from the group, not the individual session. So we hand each
        // session a unique grouping GUID (derived from the stream id, stable
        // across reconnects) BEFORE naming it — that is what makes Firefox,
        // Spotify, etc. appear as separate, individually adjustable rows.
        // Each render stream is its own WASAPI session. Sndvol gives every
        // session its OWN volume slider *by default* — as long as its grouping
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
                if let Err(e) = session.SetDisplayName(
                    windows::core::PCWSTR(wide.as_ptr()),
                    std::ptr::null(),
                ) {
                    tracing::warn!(error = %e, "could not set mixer display name");
                }
            }
            Err(e) => tracing::warn!(error = %e, "no session control; mixer row stays unnamed"),
        }

        let frame_bytes = mix.nBlockAlign as usize;

        if let Err(e) = client.Start() {
            let _ = ready_tx.send(Err(format!("client start: {e}")));
            return;
        }
        let _ = ready_tx.send(Ok(()));

        let mut jitter: VecDeque<u8> = VecDeque::new();
        let mut started = false;

        loop {
            if stop_rx.try_recv().is_ok() {
                let _ = client.Stop();
                break;
            }

            // Pull any newly arrived chunks into the jitter buffer, converting
            // from the wire format to whatever this device actually wants.
            // Inbound chunks are always 48k stereo f32; the endpoint may be
            // 16-channel or 44.1 kHz, and feeding wire bytes straight into it
            // misaligns every frame (heard as noise).
            while let Ok(mut bytes) = feed_rx.try_recv() {
                // Apply the logical gain to the wire-format f32 samples before
                // converting to the device format, so gain is independent of the
                // endpoint's mix format. Read the atomic once per chunk (knob
                // speed, not audio speed); skip the multiply at unity.
                let g = f32::from_bits(gain.load(std::sync::atomic::Ordering::Relaxed));
                if (g - 1.0).abs() > f32::EPSILON {
                    for s in bytes.chunks_exact_mut(4) {
                        let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]) * g;
                        s.copy_from_slice(&v.to_le_bytes());
                    }
                }
                jitter.extend(from_wire_format(&bytes, mix.nChannels, mix.nSamplesPerSec));
            }
            // Bound the backlog by discarding the oldest audio — see
            // RENDER_MAX_BYTES. Trim whole frames so the stream stays aligned
            // and channels don't swap.
            if jitter.len() > RENDER_MAX_BYTES {
                let frame = (mix.nChannels as usize) * (mix.wBitsPerSample as usize / 8);
                let mut excess = jitter.len() - RENDER_MAX_BYTES;
                if frame > 0 {
                    excess -= excess % frame;
                }
                if excess > 0 {
                    jitter.drain(..excess);
                    tracing::debug!(
                        dropped_bytes = excess,
                        "render jitter buffer over its limit; dropped stale audio to keep latency bounded"
                    );
                }
            }
            if !started && jitter.len() >= RENDER_PREFILL_BYTES {
                started = true;
            }

            // Fill the device's currently-free space.
            let padding = client.GetCurrentPadding().unwrap_or(0);
            let free_frames = buffer_frames.saturating_sub(padding);
            if free_frames > 0 {
                if let Ok(p_data) = render.GetBuffer(free_frames) {
                    if !p_data.is_null() {
                        let want = free_frames as usize * frame_bytes;
                        let take = if started { want.min(jitter.len()) } else { 0 };
                        // SAFETY: WASAPI guarantees `p_data` addresses
                        // free_frames * frame_bytes writable bytes until
                        // ReleaseBuffer.
                        let dst = std::slice::from_raw_parts_mut(p_data, want);
                        for (i, b) in jitter.drain(..take).enumerate() {
                            dst[i] = b;
                        }
                        // Zero the remainder (underrun / not started).
                        for b in dst.iter_mut().skip(take) {
                            *b = 0;
                        }
                        let flags = if take == 0 { AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 } else { 0 };
                        let _ = render.ReleaseBuffer(free_frames, flags);
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
mod format_tests {
    use super::*;

    fn f32s(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|s| s.to_le_bytes()).collect()
    }
    fn parse(b: &[u8]) -> Vec<f32> {
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    /// The bug the user heard as noise: a 16-channel endpoint (VB-CABLE
    /// enumerates as "CABLE In 16ch") was sent as-is under a stereo label, so
    /// every frame slid out of alignment.
    #[test]
    fn multichannel_capture_is_reduced_to_stereo() {
        // Two frames of 16 channels; only the first two carry signal.
        let mut input = Vec::new();
        for f in 0..2 {
            for ch in 0..16 {
                input.push(if ch < 2 { (f * 2 + ch) as f32 * 0.1 } else { 0.9 });
            }
        }
        let out = to_wire_format(&f32s(&input), 16, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.0, 0.1, 0.2, 0.3]);
    }

    /// Mono must be duplicated, not left half-length — otherwise the peer reads
    /// one frame as half of the next.
    #[test]
    fn mono_capture_is_duplicated_to_stereo() {
        let out = to_wire_format(&f32s(&[0.5, -0.5]), 1, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.5, 0.5, -0.5, -0.5]);
    }

    /// A 44.1 kHz device must come out at 48 kHz, or it plays at the wrong
    /// pitch and drifts.
    #[test]
    fn rate_conversion_lands_on_the_wire_rate() {
        let frames = 441;
        let input: Vec<f32> = (0..frames * 2).map(|i| i as f32 * 0.001).collect();
        let out = to_wire_format(&f32s(&input), 2, 44_100);
        assert_eq!(parse(&out).len() / 2, 480, "441 frames @44.1k should be 480 @48k");
    }

    /// Matching format must be a pass-through — no needless copying or drift.
    #[test]
    fn matching_format_passes_through_untouched() {
        let bytes = f32s(&[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(to_wire_format(&bytes, 2, SAMPLE_RATE), bytes);
        assert_eq!(from_wire_format(&bytes, 2, SAMPLE_RATE), bytes);
    }

    /// Render into a multi-channel endpoint must fill every channel, not leave
    /// the extras as garbage.
    #[test]
    fn render_expands_stereo_to_multichannel() {
        let out = from_wire_format(&f32s(&[0.25, -0.25]), 4, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.25, -0.25, 0.25, -0.25]);
    }

    /// Rendering to a mono endpoint must carry both channels, not just the
    /// left — otherwise hard-panned audio disappears and a stereo mix is heard
    /// as "one ear only".
    #[test]
    fn render_downmixes_stereo_to_mono() {
        // Hard-panned right: a left-only downmix would render pure silence.
        let out = from_wire_format(&f32s(&[0.0, 1.0]), 1, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.5]);

        // Centre content keeps its level rather than doubling into clipping.
        let out = from_wire_format(&f32s(&[0.8, 0.8]), 1, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.8]);
    }

    /// Round-tripping through an odd device format must preserve frame count,
    /// so the stream neither stretches nor compresses over time.
    #[test]
    fn round_trip_preserves_duration() {
        let frames = 480;
        let input: Vec<f32> = (0..frames * 2).map(|i| (i as f32 * 0.01).sin()).collect();
        let to_device = from_wire_format(&f32s(&input), 6, 44_100);
        let back = to_wire_format(&to_device, 6, 44_100);
        let got = parse(&back).len() / 2;
        assert!(
            got.abs_diff(frames) <= 2,
            "round trip changed duration: {frames} -> {got}"
        );
    }
}
