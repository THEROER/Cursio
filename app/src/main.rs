#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

//! The Cursio app: a window and a tray icon in front of the `cursio` node.
//!
//! It does not run the mesh (see `cursio_core::control` for why): on a desktop
//! it is a client of the node's control socket, showing what the node is doing
//! and changing its config; on a phone it is the touchpad. Closing the window
//! hides it — the tray is the app — and quitting the app leaves the node
//! running, because the mesh is not the app's to stop.

/// Starting the node and the tray at login.
#[cfg(desktop)]
mod autostart;
/// Installed versions and rolling back to the previous one.
#[cfg(desktop)]
mod install;
/// The node's and this app's log, for the window's log drawer.
#[cfg(desktop)]
mod logs;
/// The line to the running node.
mod node;
/// Driving a peer's cursor from a touch surface — the phone-as-touchpad half.
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
            // SAFETY: called first thing in `main`, before any thread is
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

fn main() {
    #[cfg(target_os = "linux")]
    tune_webkitgtk_renderer();

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
    let _log = cursio_core::logging::init("cursio-app");

    // Ensure this node has a stable identity before the UI comes up; the same
    // identity the `cursio` CLI uses, so the app and CLI act as one node.
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
    let hidden = std::env::args().any(|a| a == "--hidden");

    let builder = tauri::Builder::default()
        .manage(touchpad::Touchpad::default())
        .manage(node::NodeLink::default());

    #[cfg(desktop)]
    let builder = builder
        .setup(move |app| {
            node::follow(app.handle().clone());
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
            touchpad::commands::touchpad_status,
            touchpad::commands::touchpad_connect,
            touchpad::commands::touchpad_motion,
            touchpad::commands::touchpad_button,
            touchpad::commands::touchpad_scroll,
            node::commands::node_status,
            node::commands::node_config,
            node::commands::node_set_module,
            node::commands::node_set_position,
            node::commands::node_pair_show,
            node::commands::node_pair_enter,
            node::commands::node_pair_cancel,
            node::commands::node_pairing,
            node::commands::node_restart,
            node::commands::node_update_apply,
            node::commands::node_start,
            autostart::commands::autostart_get,
            autostart::commands::autostart_set,
            install::commands::install_status,
            install::commands::install_rollback,
            logs::commands::log_tail,
        ]);

    #[cfg(mobile)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        app_platform,
        touchpad::commands::touchpad_status,
        touchpad::commands::touchpad_connect,
        touchpad::commands::touchpad_motion,
        touchpad::commands::touchpad_button,
        touchpad::commands::touchpad_scroll,
    ]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
