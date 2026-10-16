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
}

/// One of the things [`Setup`] lists, for the user to grant.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Grant {
    Notifications,
    Microphone,
    Battery,
    Autostart,
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

pub mod commands {
    use super::*;

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
