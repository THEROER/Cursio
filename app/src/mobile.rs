//! The phone's half of the app that only Android can do: the foreground
//! service that keeps the node alive, and the permissions it needs.
//!
//! The Kotlin side is the `cursio-mesh` Android plugin
//! (`crates/android/plugin`, wired into the Gradle build by `build.rs`), the
//! same arrangement the LeavePulse launcher uses for its runtime. Everything
//! here is a thin call through to it: the node itself is Rust and lives in
//! `cursio_android`, started by the service rather than by this window, because
//! the service outlives the window — and comes up at boot with no window at all.

use serde::{Deserialize, Serialize};
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{AppHandle, Manager, Runtime, Wry};

/// The plugin's Kotlin class, as `register_android_plugin` names it.
const PLUGIN_PACKAGE: &str = "dev.cursio.mesh";
const PLUGIN_CLASS: &str = "MeshPlugin";

struct MeshPlugin<R: Runtime>(PluginHandle<R>);

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("cursio-mesh")
        .setup(|app, api| {
            let handle = api.register_android_plugin(PLUGIN_PACKAGE, PLUGIN_CLASS)?;
            // Started from the visible Activity, which is the one moment
            // Android lets a service claim the microphone and media playback.
            if let Err(e) = handle.run_mobile_plugin::<serde_json::Value>("startService", ()) {
                tracing::warn!(error = %e, "could not start the Cursio service");
            }
            app.manage(MeshPlugin(handle));
            Ok(())
        })
        .build()
}

/// What the phone still needs from the user for the node to keep running and
/// to carry audio. The first three are `true` once granted; the last is a
/// hint, not a grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Setup {
    pub notifications: bool,
    pub microphone: bool,
    /// Exempt from battery optimisation.
    pub battery: bool,
    /// `true` while the user should still be sent to a maker's own setting
    /// that keeps background apps alive (HyperOS "Autostart"). No API can
    /// read that setting, so this turns false once the screen has been opened.
    pub autostart_setting: bool,
    /// This phone's notifications may be read and shown on a desktop.
    pub notification_access: bool,
    /// This phone's calls may be shown, answered and placed from a computer.
    /// Defaulted for a plugin built before the row existed.
    #[serde(default)]
    pub calls: bool,
    /// This phone's SMS may be read and sent from a desktop. Defaulted for a
    /// plugin built before the row existed.
    #[serde(default)]
    pub messages: bool,
    /// The phone may reach its computers over Bluetooth when the network
    /// does not. Defaulted for a plugin built before the row existed.
    #[serde(default)]
    pub nearby: bool,
    /// A computer may mount this phone's storage. Defaulted for a plugin
    /// built before the row existed.
    #[serde(default)]
    pub storage: bool,
    /// Cursio is the companion app of a computer paired over Bluetooth, so
    /// the system keeps it running and wakes it near that computer.
    /// Defaulted for a plugin built before the row existed.
    #[serde(default)]
    pub companion: bool,
    /// This phone's photos and videos may be browsed from its peers.
    /// Defaulted for a plugin built before the row existed.
    #[serde(default)]
    pub gallery: bool,
    /// A computer showing this screen may tap and type on it (the
    /// accessibility service is on). Defaulted for a plugin built before the
    /// row existed.
    #[serde(default)]
    pub touch: bool,
    /// The screen is being shown on computers now.
    #[serde(default)]
    pub showing_screen: bool,
}

/// One of the things [`Setup`] lists, for the user to grant.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Grant {
    Notifications,
    Microphone,
    Battery,
    Autostart,
    NotificationAccess,
    Calls,
    /// Not a grant: Android's Bluetooth screen, where the computer is paired
    /// and picked as the speaker.
    Bluetooth,
    Messages,
    Nearby,
    Storage,
    Companion,
    Gallery,
    /// Not a grant either: the accessibility list, where the user turns on
    /// touch from computers.
    Touch,
}

#[derive(Serialize)]
struct GrantArgs {
    what: Grant,
}

fn plugin(app: &AppHandle) -> Result<PluginHandle<Wry>, String> {
    app.try_state::<MeshPlugin<Wry>>()
        .map(|p| p.0.clone())
        .ok_or_else(|| "the Cursio Android plugin is not loaded".to_string())
}

pub fn start_service(app: &AppHandle) -> Result<(), String> {
    plugin(app)?
        .run_mobile_plugin::<serde_json::Value>("startService", ())
        .map(drop)
        .map_err(|e| e.to_string())
}

/// A peer device the phone's playback can be sent to.
#[derive(Debug, Clone, Serialize)]
pub struct PlaybackTarget {
    pub key: u64,
    pub name: String,
    pub routed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhoneAudioState {
    /// Linear, 0 to 1.
    pub volume: f32,
    pub awaited: Option<String>,
    /// The device chosen before Cursio restarted, while nothing is routed.
    pub remembered: Option<String>,
    /// That device's key, when it is connected and can be resumed now.
    pub resume: Option<u64>,
}

pub mod commands {
    use super::*;

    #[tauri::command]
    pub fn phone_audio_targets() -> Vec<PlaybackTarget> {
        cursio_android::playback_targets()
            .into_iter()
            .map(|t| PlaybackTarget {
                key: t.key,
                name: t.name,
                routed: t.routed,
            })
            .collect()
    }

    /// Send what this phone plays to one peer device, or stop with `None`.
    ///
    /// Capture comes first: Android asks the user to allow it (MediaProjection)
    /// every time, and a route set before that answer would open a stream with
    /// nothing in it. Stopping goes the other way round, so the peer hears the
    /// stream end rather than fall silent.
    #[tauri::command]
    pub async fn phone_audio_route(app: AppHandle, target: Option<u64>) -> Result<(), String> {
        let plugin = plugin(&app)?;
        tauri::async_runtime::spawn_blocking(move || match target {
            Some(key) => {
                plugin
                    .run_mobile_plugin::<serde_json::Value>("startPlaybackCapture", ())
                    .map_err(|e| e.to_string())?;
                cursio_android::route_playback(Some(key)).map_err(|e| e.to_string())
            }
            None => {
                cursio_android::route_playback(None).map_err(|e| e.to_string())?;
                plugin
                    .run_mobile_plugin::<serde_json::Value>("stopPlaybackCapture", ())
                    .map(drop)
                    .map_err(|e| e.to_string())
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// How loud the phone's sound reaches the computer, the device it is
    /// waiting for while that device's machine is away, and the one chosen
    /// before a restart that only the user's consent can bring back.
    #[tauri::command]
    pub fn phone_audio_state() -> PhoneAudioState {
        PhoneAudioState {
            volume: cursio_android::playback_volume(),
            awaited: cursio_android::awaited_playback(),
            remembered: cursio_android::remembered_playback(),
            resume: cursio_android::resumable_playback().map(|t| t.key),
        }
    }

    #[tauri::command]
    pub fn phone_audio_volume(volume: f32) -> Result<(), String> {
        cursio_android::set_playback_volume(volume).map_err(|e| e.to_string())
    }

    /// Show this screen on every computer linked now (`true`), after the
    /// system's consent, or stop showing it.
    #[tauri::command]
    pub async fn phone_screen_share(app: AppHandle, show: bool) -> Result<(), String> {
        let plugin = plugin(&app)?;
        let command = if show {
            "startScreenShare"
        } else {
            "stopScreenShare"
        };
        tauri::async_runtime::spawn_blocking(move || {
            plugin.run_mobile_plugin::<serde_json::Value>(command, ())
        })
        .await
        .map_err(|e| e.to_string())?
        .map(drop)
        .map_err(|e| e.to_string())
    }

    /// The window's answer to a computer asking to see this screen.
    #[tauri::command]
    pub async fn phone_screen_answer(
        app: AppHandle,
        computer: String,
        allow: bool,
        always: bool,
    ) -> Result<(), String> {
        let plugin = plugin(&app)?;
        let args = serde_json::json!({ "computer": computer, "allow": allow, "always": always });
        tauri::async_runtime::spawn_blocking(move || {
            plugin.run_mobile_plugin::<serde_json::Value>("answerScreen", args)
        })
        .await
        .map_err(|e| e.to_string())?
        .map(drop)
        .map_err(|e| e.to_string())
    }

    /// Bring the node down and up again, so its links greet peers with what
    /// changed — a new name, most often. The service does it, since the node
    /// is the service's.
    #[tauri::command]
    pub async fn mobile_restart(app: AppHandle) -> Result<(), String> {
        let plugin = plugin(&app)?;
        tauri::async_runtime::spawn_blocking(move || {
            plugin.run_mobile_plugin::<serde_json::Value>("restartService", ())
        })
        .await
        .map_err(|e| e.to_string())?
        .map(drop)
        .map_err(|e| e.to_string())
    }

    /// Colour the system bars' icons for the page's theme; see the plugin's
    /// `setSystemBars` for why the system's own day/night is the wrong guide.
    #[tauri::command]
    pub fn mobile_system_bars(app: AppHandle, dark: bool) -> Result<(), String> {
        #[derive(Serialize)]
        struct Args {
            dark: bool,
        }
        plugin(&app)?
            .run_mobile_plugin::<serde_json::Value>("setSystemBars", Args { dark })
            .map(drop)
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn mobile_setup(app: AppHandle) -> Result<Setup, String> {
        let plugin = plugin(&app)?;
        tauri::async_runtime::spawn_blocking(move || plugin.run_mobile_plugin("setup", ()))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    /// Ask for one grant. Resolves once the user is back from the system
    /// dialog or settings screen, with what is granted now.
    ///
    /// `run_mobile_plugin` waits for that, which can be minutes in a settings
    /// screen, so it waits on a blocking thread rather than an async worker.
    #[tauri::command]
    pub async fn mobile_request(app: AppHandle, what: Grant) -> Result<Setup, String> {
        let plugin = plugin(&app)?;
        tauri::async_runtime::spawn_blocking(move || {
            plugin.run_mobile_plugin("request", GrantArgs { what })
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }
}
