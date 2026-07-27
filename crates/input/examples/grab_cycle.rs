//! Smoke test for local-input grab (Linux `EVIOCGRAB`).
//!
//! Starts capture, grabs every captured device for a short window, then
//! releases — proving `GrabControl::set_grabbed` freezes and unfreezes the real
//! mouse/keyboard without leaving them stuck. Prints captured events so you can
//! see input stop reaching the desktop while grabbed (events still arrive here).
//!
//! Run (needs `input`-group access to /dev/input):
//!   sg input -c 'cargo run -p cursio-input --example grab_cycle'
//!
//! Safety: the grab lasts only GRAB_SECS and is always released; even if you
//! kill the process mid-grab, the kernel drops EVIOCGRAB when the fd closes.

use std::time::Duration;

use cursio_input::spawn_capture;

/// How long to hold the grab before releasing.
const GRAB_SECS: u64 = 3;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info,cursio_input=debug")
        .init();

    let mut handle = spawn_capture(None)?;
    println!("capturing: {:?}", handle.devices);

    // Drain events in the background so we can observe them arriving.
    let mut events = std::mem::replace(&mut handle.events, {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        rx
    });
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            println!("  event: {ev:?}");
        }
    });

    println!("\n>>> input is LIVE for {GRAB_SECS}s — move the mouse, it should work normally");
    tokio::time::sleep(Duration::from_secs(GRAB_SECS)).await;

    println!("\n>>> GRABBING for {GRAB_SECS}s — mouse/keyboard should FREEZE locally");
    println!("    (events still print here, but the desktop cursor won't move)");
    handle.grab.set_grabbed(true);
    tokio::time::sleep(Duration::from_secs(GRAB_SECS)).await;

    println!("\n>>> RELEASING — input should work locally again");
    handle.grab.set_grabbed(false);
    tokio::time::sleep(Duration::from_secs(GRAB_SECS)).await;

    println!("\ndone.");
    Ok(())
}
