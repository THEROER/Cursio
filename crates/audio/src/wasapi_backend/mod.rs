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

use windows::Win32::Media::Audio::{
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler_Impl,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::wasapi_backend::sessions::ActivationHandler_Impl;
use crate::AudioError;

mod capture;
mod devices;
mod format;
mod monitor;
mod render;
mod sessions;

pub use capture::WasapiSource;
pub use devices::{list_input_devices, list_output_devices};
pub(crate) use format::wire_format;
pub(crate) use monitor::spawn_device_monitor;
pub use render::WasapiSink;
pub use sessions::{app_volume, list_app_streams, set_app_volume};

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
