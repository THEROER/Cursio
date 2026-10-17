//! The Cursio app: a window in front of a node.
//!
//! On a desktop it does not run the mesh (see `cursio_core::control` for why):
//! it is a client of the `cursio` node's control socket, showing what the node
//! is doing and changing its config, with a tray icon that is the app while
//! the window is closed. Quitting the app leaves the node running, because the
//! mesh is not the app's to stop.
//!
//! On a phone there is no separate node to be a client of: Android runs apps,
//! not daemons. The node runs in this process, kept alive by a foreground
//! service (`mobile.rs`), and the window talks to it through the same
//! control commands a desktop sends over its socket — so one UI draws both.

/// Starting the node and the tray at login.
#[cfg(desktop)]
mod autostart;
/// Installed versions and rolling back to the previous one.
#[cfg(desktop)]
mod install;
/// The node's and this app's log, for the window's log drawer.
#[cfg(desktop)]
mod logs;
/// The phone's foreground service and the permissions it needs.
#[cfg(mobile)]
mod mobile;
/// The line to the running node.
mod node;
/// Driving a peer's cursor from a phone's touch surface.
#[cfg(mobile)]
mod touchpad;
#[cfg(desktop)]
mod tray;

/// Which face of the app this build shows, so the UI picks its first page.
#[tauri::command]
fn app_platform() -> &'static str {
    if cfg!(mobile) {
        "mobile"
    } else {
        "desktop"
    }
}

/// Steer WebKitGTK away from the renderer paths that crash it, before any
/// thread exists and before Tauri creates the webview.
///
/// Taken from the LeavePulse launcher, which met the same crashes first: on
/// Wayland the DMABUF renderer takes the web process down, so it is off there
/// and X11 keeps the GPU path. Anything the user exported is honoured, so
/// `WEBKIT_DISABLE_DMABUF_RENDERER=0` brings it back.
#[cfg(target_os = "linux")]
fn tune_webkitgtk_renderer() {
    fn set_if_unset(key: &str, value: &str) {
        if std::env::var_os(key).is_none() {
            // SAFETY: called first thing in `run`, before any thread is
            // started, so nothing can be reading the environment concurrently.
            unsafe { std::env::set_var(key, value) };
        }
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t.eq_ignore_ascii_case("wayland"));

    set_if_unset("WEBKIT_FORCE_COMPOSITING_MODE", "1");
    if wayland {
        set_if_unset("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    tune_webkitgtk_renderer();

    #[cfg(desktop)]
    let (_log, hidden) = {
        // Started as the install's stable copy: run the current version instead.
        if let Some(target) = cursio_core::update::layout::handover("cursio-app") {
            match cursio_core::update::layout::hand_over(&target) {
                Ok(()) => return,
                Err(e) => eprintln!(
                    "could not start {}: {e}; running this copy",
                    target.display()
                ),
            }
        }

        // Console (info) + rotating file (debug) logging, shared with the CLI.
        // `_log` must outlive the UI — dropping it flushes the log file.
        let log = cursio_core::logging::init("cursio-app");

        // Ensure this node has a stable identity before the UI comes up; the
        // same identity the `cursio` CLI uses, so the app and CLI act as one
        // node.
        match cursio_core::NodeIdentity::load_or_create() {
            Ok(identity) => {
                tracing::info!(
                    name = %identity.display_name,
                    peer_id = %identity.peer_id,
                    "cursio node"
                );
            }
            Err(e) => tracing::warn!(error = %e, "could not load node identity"),
        }

        // Started at login by autostart: come up as the tray alone.
        (log, std::env::args().any(|a| a == "--hidden"))
    };

    let builder = tauri::Builder::default().manage(node::NodeLink::default());

    #[cfg(desktop)]
    let builder = builder
        .setup(move |app| {
            node::follow(app.handle().clone());
            // `reg.exe` / `systemctl`: off the thread that draws the window.
            tauri::async_runtime::spawn_blocking(autostart::refresh);
            if let Err(e) = tray::install(app.handle()) {
                // No tray (GNOME without the AppIndicator extension) must not
                // leave an app nobody can reach: show the window instead.
                tracing::warn!(
                    error = %e,
                    "no tray icon; on GNOME install the AppIndicator extension"
                );
                tray::open(app.handle(), "/");
            } else if !hidden {
                tray::open(app.handle(), "/");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_platform,
            node::commands::node_status,
            node::commands::node_config,
            node::commands::node_set_module,
            node::commands::node_set_position,
            node::commands::node_set_name,
            node::commands::node_set_mic_gate,
            node::commands::node_set_stream_volume,
            node::commands::node_pair_show,
            node::commands::node_pair_enter,
            node::commands::node_pair_cancel,
            node::commands::node_pairing,
            node::commands::node_restart,
            node::commands::node_update_apply,
            node::commands::node_forget,
            node::commands::node_start,
            autostart::commands::autostart_get,
            autostart::commands::autostart_set,
            install::commands::install_status,
            install::commands::install_rollback,
            logs::commands::log_tail,
        ]);

    #[cfg(mobile)]
    let builder = builder
        .manage(touchpad::Touchpad::default())
        .plugin(mobile::init())
        .setup(|app| {
            node::follow(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_platform,
            touchpad::commands::touchpad_status,
            touchpad::commands::touchpad_targets,
            touchpad::commands::touchpad_connect,
            touchpad::commands::touchpad_motion,
            touchpad::commands::touchpad_button,
            touchpad::commands::touchpad_scroll,
            touchpad::commands::touchpad_gesture,
            node::commands::node_status,
            node::commands::node_config,
            node::commands::node_set_module,
            node::commands::node_set_name,
            node::commands::node_set_mic_gate,
            node::commands::node_set_stream_volume,
            node::commands::node_pair_show,
            node::commands::node_pair_enter,
            node::commands::node_pair_cancel,
            node::commands::node_pairing,
            node::commands::node_forget,
            node::commands::node_start,
            mobile::commands::mobile_setup,
            mobile::commands::mobile_request,
            mobile::commands::mobile_restart,
            mobile::commands::phone_audio_targets,
            mobile::commands::phone_audio_route,
            mobile::commands::phone_audio_state,
            mobile::commands::phone_audio_volume,
        ]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
