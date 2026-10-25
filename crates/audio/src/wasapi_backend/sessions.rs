//! Applications as Windows sees them: the audio sessions playing now,
//! their own volume, and the process loopback that records one alone.

use crate::wasapi_backend::ComGuard;
use crate::AudioError;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceCompletionHandler,
    IAudioClient, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

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
    let mut out = Vec::new();
    for (pid, ctl2) in app_sessions()? {
        // Prefer the session's *display name* when it has one: that's what
        // the Windows mixer shows, and it's how Cursio labels its own
        // per-stream sessions ("Cursio(Peer): Firefox"). Fall back to the
        // owning process name for ordinary apps, which set no display name.
        // SAFETY: COM is initialized on this thread for the guard's life; the
        // string is freed with the allocator COM used for it.
        let display = unsafe {
            ctl2.GetDisplayName().ok().and_then(|p| {
                let s = p.to_string().ok();
                windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
                s
            })
        }
        .filter(|s| !s.is_empty());
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

/// Every application's audio session on the default render device, with its
/// process id. System sessions (pid 0) and Cursio's own are left out — ours
/// by pid, not by name: our streams are deliberately named "Cursio(Peer): …",
/// so a name filter would hide exactly the rows wanted. The caller holds COM.
pub(super) fn app_sessions() -> Result<Vec<(u32, IAudioSessionControl2)>, AudioError> {
    // SAFETY: the caller has COM initialized on this thread.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
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
            if pid == 0 || pid == std::process::id() {
                continue;
            }
            out.push((pid, ctl2));
        }
        Ok(out)
    }
}

/// The sessions of every process named `process` (no `.exe`, any case).
pub(super) fn sessions_of(process: &str) -> Result<Vec<ISimpleAudioVolume>, AudioError> {
    Ok(app_sessions()?
        .into_iter()
        .filter(|(pid, _)| process_name(*pid).is_some_and(|n| n.eq_ignore_ascii_case(process)))
        .filter_map(|(_, ctl2)| ctl2.cast::<ISimpleAudioVolume>().ok())
        .collect())
}

/// An application's own volume in the Windows mixer (0.0–1.0), by process
/// name; `None` while it has no audio session.
pub fn app_volume(process: &str) -> Option<f32> {
    let _com = ComGuard::new().ok()?;
    let session = sessions_of(process).ok()?.into_iter().next()?;
    // SAFETY: COM is initialized on this thread for the guard's life.
    unsafe { session.GetMasterVolume() }.ok()
}

/// Set an application's own volume in the Windows mixer, on every session it
/// has — Spotify plays through more than one process.
pub fn set_app_volume(process: &str, volume: f32) -> Result<(), AudioError> {
    let _com = ComGuard::new()?;
    let sessions = sessions_of(process)?;
    if sessions.is_empty() {
        return Err(AudioError::Backend(format!(
            "{process} is not playing anything"
        )));
    }
    for session in sessions {
        // SAFETY: COM is initialized on this thread for the guard's life.
        unsafe { session.SetMasterVolume(volume.clamp(0.0, 1.0), std::ptr::null()) }
            .map_err(|e| AudioError::Backend(format!("set {process}'s volume: {e}")))?;
    }
    Ok(())
}

/// Resolve a process id to its executable name (without path/extension).
pub(super) fn process_name(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
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
pub(super) struct ActivationHandler {
    pub(super) done: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

/// Activate an `IAudioClient` that captures **only** the given process's audio.
///
/// This is the Windows answer to per-app sharing: unlike normal loopback (which
/// grabs the whole endpoint mix), process loopback restricts capture to one
/// process tree. Requires Windows 10 build 20348+ / Win11.
pub(super) unsafe fn activate_process_loopback(pid: u32) -> Result<IAudioClient, String> {
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
    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivationHandler { done: done.clone() }.into();

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
