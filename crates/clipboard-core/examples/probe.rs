//! Live probe against the real session clipboard.
//!
//! Unit tests cover the pure logic; this exercises the parts only a running
//! compositor can prove — lazy listing, per-format loading, and that a written
//! clipboard is actually served back.
//!
//! ```text
//! cargo run -p clipboard-core --example probe          # read what's on the clipboard
//! cargo run -p clipboard-core --example probe -- write # write, then read back
//! cargo run -p clipboard-core --example probe -- watch # print changes as they happen
//! ```

use clipboard_core::{ClipboardContent, ClipboardEntry, ClipboardFormat, Selection};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("write") => write_and_read_back(),
        Some("watch") => watch(),
        _ => read(),
    }
}

/// List what's on the clipboard, then load each representation.
fn read() -> Result<(), Box<dyn std::error::Error>> {
    let mut clipboard = clipboard_core::open()?;

    let listed = clipboard.read(Selection::Clipboard)?;
    println!("{} representation(s) offered:", listed.entries.len());
    for entry in &listed.entries {
        println!("  {}", entry.format.mime());
    }

    println!("\nloading each:");
    for entry in &listed.entries {
        match clipboard.load(Selection::Clipboard, &entry.format) {
            Ok(bytes) => {
                let preview = if entry.format.is_text() {
                    let text = String::from_utf8_lossy(&bytes);
                    let trimmed: String = text.chars().take(60).collect();
                    format!(
                        "{:?}{}",
                        trimmed,
                        if text.chars().count() > 60 { "…" } else { "" }
                    )
                } else {
                    // Show the leading bytes so an image is identifiable by its
                    // magic number (PNG starts 89 50 4E 47).
                    format!("{:02x?}…", &bytes[..bytes.len().min(8)])
                };
                println!(
                    "  {:<28} {:>9} bytes  {}",
                    entry.format.mime(),
                    bytes.len(),
                    preview
                );
            }
            Err(e) => println!("  {:<28} FAILED: {e}", entry.format.mime()),
        }
    }
    Ok(())
}

/// Write multi-format content and read it straight back.
///
/// The real assertion here is that content survives at all: on Wayland the
/// writer must stay alive to serve pastes, so a backend that forgot to fork
/// would show an empty clipboard on read-back.
fn write_and_read_back() -> Result<(), Box<dyn std::error::Error>> {
    let mut clipboard = clipboard_core::open()?;

    let content = ClipboardContent {
        entries: vec![
            ClipboardEntry::loaded(ClipboardFormat::Text, b"plain from probe".to_vec()),
            ClipboardEntry::loaded(ClipboardFormat::Html, b"<b>rich</b> from probe".to_vec()),
        ],
    };
    clipboard.write(Selection::Clipboard, &content)?;
    println!("wrote text + html");

    // Give the forked serving process a moment to take ownership.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let back = clipboard.read(Selection::Clipboard)?;
    println!("read back {} representation(s):", back.entries.len());
    for entry in &back.entries {
        let bytes = clipboard.load(Selection::Clipboard, &entry.format)?;
        println!(
            "  {:<28} {:?}",
            entry.format.mime(),
            String::from_utf8_lossy(&bytes)
        );
    }

    // The text must survive. HTML is not asserted: the X11 backend can only
    // own one representation at a time and offers HTML with text as its
    // fallback, so text is what comes back (see linux.rs).
    assert!(back.has(&ClipboardFormat::Text), "text representation lost");
    println!("\nOK: content survived the round trip");
    Ok(())
}

/// Print a line per clipboard change until interrupted.
#[tokio::main]
async fn watch() -> Result<(), Box<dyn std::error::Error>> {
    let mut clipboard = clipboard_core::open()?;
    let mut handle = clipboard_core::watch(Selection::Clipboard)?;
    println!("watching the clipboard; copy something (Ctrl+C to stop)");

    while let Some(change) = handle.changes.recv().await {
        match clipboard.read(change.selection) {
            Ok(content) => {
                let formats: Vec<_> = content.formats().map(|f| f.mime()).collect();
                println!("changed: {}", formats.join(", "));
            }
            Err(e) => println!("changed, but read failed: {e}"),
        }
    }
    Ok(())
}
