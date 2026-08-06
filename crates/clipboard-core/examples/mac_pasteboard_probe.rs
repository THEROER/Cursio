//! Read and write this Mac's real pasteboard.
//!
//! `NSPasteboard` cannot be unit-tested: it is process-wide OS state, and what
//! matters is whether another application sees what was written. So this reads
//! what is on the clipboard now, writes something back, and reads it again.
//!
//! ```sh
//! cargo run -p clipboard-core --example mac_pasteboard_probe
//! ```

#[cfg(target_os = "macos")]
fn main() {
    use clipboard_core::format::{ClipboardContent, ClipboardEntry, ClipboardFormat};
    use clipboard_core::Selection;

    let mut clipboard = clipboard_core::open().expect("open the pasteboard");

    match clipboard.read(Selection::Clipboard) {
        Ok(content) => {
            println!("on the clipboard now:");
            for entry in &content.entries {
                println!("  {} ({} bytes)", entry.format.mime(), entry.size);
            }
        }
        Err(error) => println!("nothing readable: {error}"),
    }

    let mut written = ClipboardContent::default();
    written.insert(ClipboardEntry::loaded(
        ClipboardFormat::Text,
        b"cursio pasteboard probe".to_vec(),
    ));
    clipboard
        .write(Selection::Clipboard, &written)
        .expect("write");
    println!("wrote plain text; reading it back");

    let back = clipboard
        .load(Selection::Clipboard, &ClipboardFormat::Text)
        .expect("load");
    println!("read back: {:?}", String::from_utf8_lossy(&back));
    assert_eq!(back, b"cursio pasteboard probe");
    println!("done");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this probe drives NSPasteboard and only runs on macOS");
}
