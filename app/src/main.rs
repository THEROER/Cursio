#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

/// Driving a peer's cursor from a touch surface — the phone-as-touchpad half.
mod touchpad;

fn main() {
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

    tauri::Builder::default()
        .manage(touchpad::Touchpad::default())
        .invoke_handler(tauri::generate_handler![
            touchpad::commands::touchpad_status,
            touchpad::commands::touchpad_connect,
            touchpad::commands::touchpad_motion,
            touchpad::commands::touchpad_button,
            touchpad::commands::touchpad_scroll,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
