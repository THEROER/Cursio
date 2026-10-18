//! The call window: a phone's ringing or ongoing call, small and on top.
//!
//! A second window rather than a page of the main one, as Phone Link does: a
//! call has to be seen whatever the main window is doing — hidden in the tray,
//! on the messages page, behind a game — and has to go away by itself when the
//! call does. The node asks for it (`--page call`, see the core's
//! `calls_display`); the page inside reads the calls from the node's status
//! and hides the window once none is left.
//!
//! It loads the same app as the main window and is told apart by its label:
//! the generated site has one `index.html`, and a route URL would ask the
//! asset server for a file that does not exist.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// The window's label, which the page reads to know it is the call window.
pub const LABEL: &str = "call";

const WIDTH: f64 = 360.0;
const HEIGHT: f64 = 210.0;
/// Clear of the screen's edge and of a bottom panel.
const MARGIN: f64 = 24.0;

/// Show the call window, building it the first time.
pub fn open(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    let mut builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("Cursio — Call")
        .inner_size(WIDTH, HEIGHT)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(false)
        .focused(true);
    // Bottom right, where a desktop's own notifications appear on Windows and
    // where Phone Link puts its call; centred when the monitor is unknown.
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let size = monitor.size().to_logical::<f64>(monitor.scale_factor());
        let origin = monitor.position().to_logical::<f64>(monitor.scale_factor());
        builder = builder.position(
            origin.x + size.width - WIDTH - MARGIN,
            origin.y + size.height - HEIGHT - MARGIN * 3.0,
        );
    } else {
        builder = builder.center();
    }
    if let Err(e) = builder.build() {
        tracing::warn!(error = %e, "could not open the call window");
    }
}
