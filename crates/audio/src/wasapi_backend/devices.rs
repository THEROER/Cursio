//! This machine's audio endpoints, found by their friendly names: what
//! is offered to peers, and what a stream asks to open.

use crate::wasapi_backend::ComGuard;
use crate::AudioError;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, StructuredStorage::PropVariantToStringAlloc, CLSCTX_ALL, STGM_READ,
};

/// `PKEY_Device_FriendlyName` — the human name shown in Sound settings (e.g.
/// "VB-Audio Virtual Cable"). Used to find a device by name. Defined here
/// because the constant isn't re-exported by the `windows` crate in a stable
/// path; the GUID/pid are from `functiondiscoverykeys_devpkey.h`.
pub(super) const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

/// Find a render (output) device whose friendly name contains `needle`
/// (case-insensitive). Returns the default endpoint when `needle` is `None`.
///
/// Lets Cursio render the peer's audio into a *specific* virtual device (e.g. a
/// VB-Audio cable) so it appears/behaves as that endpoint on Windows, since
/// Windows can't create endpoints from user space (see docs/audio-design.md).
pub(super) unsafe fn find_render_device(
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
pub(super) fn list_endpoints(
    direction: windows::Win32::Media::Audio::EDataFlow,
) -> Result<Vec<crate::OutputDevice>, AudioError> {
    let _com = ComGuard::new()?;
    // SAFETY: COM is initialized on this thread for the duration of the guard.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| AudioError::Backend(format!("enumerator: {e}")))?;
        let collection = enumerator
            .EnumAudioEndpoints(direction, DEVICE_STATE_ACTIVE)
            .map_err(|e| AudioError::Backend(format!("enum endpoints: {e}")))?;
        let count = collection
            .GetCount()
            .map_err(|e| AudioError::Backend(format!("count: {e}")))?;

        let mut out = Vec::new();
        for i in 0..count {
            let Ok(dev) = collection.Item(i) else {
                continue;
            };
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

/// Read a device's friendly name via its property store.
pub(super) unsafe fn device_friendly_name(dev: &IMMDevice) -> Result<String, String> {
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

/// Find an active capture endpoint whose friendly name contains `needle`.
///
/// The capture-side twin of [`find_render_device`], used so a specific
/// microphone can be shared rather than only the default input.
pub(super) unsafe fn find_capture_device(
    enumerator: &IMMDeviceEnumerator,
    needle: &str,
) -> Result<IMMDevice, String> {
    let needle_low = needle.to_lowercase();
    let collection = enumerator
        .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
        .map_err(|e| format!("enum capture endpoints: {e}"))?;
    let count = collection.GetCount().map_err(|e| format!("count: {e}"))?;
    for i in 0..count {
        let Ok(dev) = collection.Item(i) else {
            continue;
        };
        if let Ok(name) = device_friendly_name(&dev) {
            if name.to_lowercase().contains(&needle_low) {
                tracing::info!(%name, "matched capture device");
                return Ok(dev);
            }
        }
    }
    Err(format!("no active capture device matching '{needle}'"))
}
