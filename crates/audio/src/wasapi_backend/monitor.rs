//! Endpoints' volume and mute, polled and set: the Windows half of
//! device-state replication.

use crate::wasapi_backend::devices::device_friendly_name;
use crate::wasapi_backend::ComGuard;
use crate::worker::{OnDrop, Worker};
use crate::{AudioError, DeviceChange, DeviceCommand};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eRender, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

/// Start watching this machine's render endpoints' volume/mute, reporting
/// changes on `changes` and applying what arrives on `commands` — the Windows
/// half of device-state replication (see docs/audio-design.md, "Device state
/// is REPLICATED"). Fails only if the monitor thread can't be spawned;
/// per-device COM errors are logged and skipped, not fatal.
///
/// Unlike the PipeWire backend (event-driven on the graph), this **polls** each
/// endpoint's [`IAudioEndpointVolume`] on a short interval and emits a change
/// when the scalar volume or mute flag moves. Polling avoids the COM
/// apartment/lifetime pitfalls of `RegisterControlChangeNotify` callbacks, and a
/// volume knob changes far slower than the poll, so nothing perceptible is lost.
pub(crate) fn spawn_device_monitor(
    changes: tokio::sync::mpsc::UnboundedSender<DeviceChange>,
    commands: std_mpsc::Receiver<DeviceCommand>,
) -> Result<Worker, AudioError> {
    Worker::spawn(
        "cursio-wasapi-device-monitor".to_string(),
        "device-monitor thread",
        OnDrop::Join,
        move |stop_rx| run_device_monitor_loop(changes, commands, stop_rx),
    )
}

/// How often the monitor polls each endpoint's volume/mute. A knob turn changes
/// far slower than this, so ~5/s reflects promptly without spinning.
pub(super) const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Open the `IAudioEndpointVolume` for a render endpoint by friendly name.
///
/// Returns `None` (not an error) when the device isn't found or can't be
/// activated — the monitor logs and skips rather than failing the whole poll.
pub(super) unsafe fn endpoint_volume_by_name(
    enumerator: &IMMDeviceEnumerator,
    name: &str,
) -> Option<IAudioEndpointVolume> {
    let collection = enumerator
        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        .ok()?;
    let count = collection.GetCount().ok()?;
    let name_low = name.to_lowercase();
    for i in 0..count {
        let Ok(dev) = collection.Item(i) else {
            continue;
        };
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
pub(super) fn run_device_monitor_loop(
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
                            if let Err(e) = vol.SetMasterVolumeLevelScalar(
                                volume.clamp(0.0, 1.0),
                                std::ptr::null(),
                            ) {
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
                        let Ok(dev) = collection.Item(i) else {
                            continue;
                        };
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
