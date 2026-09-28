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

fn main() {
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
            node::commands::node_start,
            autostart::commands::autostart_get,
            autostart::commands::autostart_set,
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
