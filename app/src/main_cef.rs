#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

// Chromium starts this binary again for each of its helper processes; the
// attribute sends those straight into CEF before the app starts.
#[tauri_runtime_cef::cef_entry_point]
fn main() {
    cursio_app_lib::run();
}
