#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
