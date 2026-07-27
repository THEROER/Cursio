//! Safe live smoke test of the Wayland capture backend (`spawn_wayland_capture`).
//!
//! Brings capture up, enables it (grab), prints neutral events for a few
//! seconds, then releases — proving the portal→libei→neutral-channel path works
//! end to end. Designed NOT to lock you out: capture auto-releases quickly.
//!
//! Run:
//!   CURSIO_AUTO_RELEASE_SECS=8 cargo run -p cursio-input --example wayland_capture_smoke
//! Approve the portal dialog, then push the mouse into a screen edge. After the
//! window (default 8s) capture releases and input returns to the desktop even if
//! you did nothing. Kill via SSH if anything wedges.

use std::time::Duration;

use cursio_input::spawn_capture;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info,cursio_input=debug")
        .init();

    // Keep the safety window short unless the caller overrode it.
    if std::env::var_os("CURSIO_AUTO_RELEASE_SECS").is_none() {
        // SAFETY: single-threaded here, before any threads spawn.
        unsafe { std::env::set_var("CURSIO_AUTO_RELEASE_SECS", "8") };
    }

    println!("bringing up capture (approve the portal dialog if shown)...");
    // Arm the right edge so pushing into it activates capture (the whole point
    // of this smoke test). Real sessions get the edge from --peer-edge.
    let mut handle = spawn_capture(Some(cursio_protocol::Edge::Right))?;
    println!("capture up: {:?}", handle.devices);

    // Enable capture (this is what a dormant machine does). On Wayland this
    // arms the portal; input is captured once the cursor hits an edge barrier.
    println!("\n>>> enabling capture. Push the mouse into a SCREEN EDGE. <<<");
    println!(">>> auto-releases in a few seconds; input returns to desktop. <<<\n");
    handle.grab.set_grabbed(true);

    // Print events until our own hard deadline, then force release regardless.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::select! {
            ev = handle.events.recv() => match ev {
                Some(e) => println!("  captured: {e:?}"),
                None => { println!("capture channel closed"); break; }
            },
            _ = tokio::time::sleep_until(deadline) => {
                println!("\ndeadline reached; releasing.");
                break;
            }
        }
    }

    // Belt-and-braces: release the grab so the portal disables capture.
    handle.grab.set_grabbed(false);
    // Give the release a moment to round-trip before we exit (drop also releases).
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("done. input should be back on the desktop.");
    Ok(())
}
