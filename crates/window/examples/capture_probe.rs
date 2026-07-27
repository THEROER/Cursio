//! Manual probe for the Linux window-capture backend.
//!
//! Brings up the ScreenCast portal (you pick a window in the dialog the first
//! time) and prints what the capture produces: the negotiated geometry, then a
//! line per frame with its dirty rect, codec, and encoded size. That last number
//! is the one that matters — it shows the dirty-region + compression levers
//! doing their job (a still window should go quiet entirely; typing should cost
//! a few hundred bytes, not 8 MiB).
//!
//! Run with:
//!
//! ```sh
//! OPUS_TARGET=ubuntu-24.04_x86_64 \
//!   cargo run -p cursio-window --example capture_probe
//! ```
//!
//! Requires a Wayland session with `xdg-desktop-portal` running. Ctrl-C to stop.

use cursio_window::{CaptureSession, PortalWindowCapture, WindowCapture};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            // `zbus::proxy=error` silences a burst of harmless warnings the
            // portal handshake always produces ("Failed to populate properties
            // cache … Object does not exist at path"): ashpd builds each
            // Request/Session proxy before the portal has created the object,
            // so caching fails and zbus says so. Nothing is broken, but it
            // buries the lines that matter. Still overridable via RUST_LOG.
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cursio_window=debug,zbus::proxy=error".into()),
        )
        .init();

    println!("=======================================================");
    println!(" A screen-share dialog is about to open.");
    println!();
    println!(" It may appear BEHIND your other windows — a daemon has no");
    println!(" parent window to anchor it to, so the compositor won't");
    println!(" focus it. If you don't see it, check Alt+Tab.");
    println!();
    println!(" Pick a window to share (one with changing content, e.g. a");
    println!(" terminal, shows the dirty-region logic best).");
    println!("=======================================================");
    println!();

    // `begin` blocks until the user answers the portal dialog, which may never
    // happen (an unfocused dialog is easy to miss). Run it off-thread so we can
    // give up with an explanation instead of hanging in silence forever.
    const DIALOG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut capture = PortalWindowCapture;
        let _ = tx.send(capture.begin(1));
    });

    let mut session = match rx.recv_timeout(DIALOG_TIMEOUT) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            eprintln!("capture failed to start: {e}");
            eprintln!("(a declined portal dialog looks like this too)");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!();
            eprintln!(
                "No answer from the screen-share dialog after {}s — giving up.",
                DIALOG_TIMEOUT.as_secs()
            );
            eprintln!("The dialog was almost certainly open but unfocused, behind another");
            eprintln!("window. Re-run and look for it with Alt+Tab.");
            std::process::exit(1);
        }
    };

    let meta = session.meta().clone();
    println!(
        "capturing window id={} {}x{} — {:?}",
        meta.id, meta.width, meta.height, meta.title
    );
    println!("frames (dirty rect / codec / encoded bytes):");

    let full_frame_bytes = (meta.width as u64) * (meta.height as u64) * 4;
    let mut frames = 0u64;
    let mut total = 0u64;

    while let Some(frame) = session.frames().recv().await {
        frames += 1;
        total += frame.data.len() as u64;
        // Ratio against what a naive full raw frame would have cost — the whole
        // point of the dirty + codec levers.
        let ratio = full_frame_bytes as f64 / frame.data.len().max(1) as f64;
        println!(
            "  seq={:<6} {:>4}x{:<4} at ({:>4},{:<4})  {:?}  {:>8} B  ({:.0}x cheaper than a full raw frame)",
            frame.seq,
            frame.dirty.width,
            frame.dirty.height,
            frame.dirty.x,
            frame.dirty.y,
            frame.codec,
            frame.data.len(),
            ratio,
        );
        if frames % 30 == 0 {
            println!(
                "  -- {frames} frames, {:.1} MiB total, avg {:.1} KiB/frame",
                total as f64 / (1024.0 * 1024.0),
                total as f64 / frames as f64 / 1024.0
            );
        }
    }

    println!("capture ended after {frames} frames");
}
