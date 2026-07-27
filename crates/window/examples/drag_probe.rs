//! Print window-drag events as the Shell extension reports them.
//!
//! Bring-up tool for the drag trigger: it exercises the real D-Bus path
//! ([`DragWatcher`]) without needing a session or a peer, so "is the extension
//! talking to us?" can be answered on its own.
//!
//! ```text
//! cargo run -p cursio-window --example drag_probe
//! ```
//!
//! Then drag a window by its titlebar. Nothing printed means the extension
//! isn't running — which is also the expected result immediately after
//! installing it, since GNOME only scans for new extensions when the session
//! starts (on Wayland that means logging out and back in).

use cursio_window::drag::DragEvent;
use cursio_window::linux_drag::DragWatcher;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cursio_window=debug".into()),
        )
        .init();

    let mut watcher = DragWatcher::spawn();
    println!("listening for window drags — drag a window by its titlebar (Ctrl-C to stop)");

    while let Some(event) = watcher.next().await {
        match event {
            DragEvent::Started { title, at } => {
                println!("drag started: {title:?} at ({}, {})", at.x, at.y)
            }
            DragEvent::Moved { at } => println!("  moved to ({}, {})", at.x, at.y),
            DragEvent::Ended { at } => println!("drag ended at ({}, {})", at.x, at.y),
        }
    }
}
