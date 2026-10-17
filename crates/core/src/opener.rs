//! Opening a link or a file the way the person sitting here would.
//!
//! On Windows the node runs elevated (input injection needs it, see
//! `elevate.rs`), and a program it starts inherits that: a browser opened by
//! `ShellExecute` from here would run as administrator, with none of the
//! user's sessions and every page it loads holding admin rights. `explorer.exe`
//! is asked instead — it hands the target to the user's own, unelevated shell,
//! which opens it as a double-click would.

/// Open `target` (an `http(s)` URL or a local path) in its default handler.
/// Returns once the handler is started; it is not waited for.
pub fn open(target: &str) -> Result<(), String> {
    let mut command = handler(target)?;
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start the program that opens it: {e}"))?;
    // Reaped on a thread: the opener exits quickly on its own, and a child
    // never waited for stays a zombie for the life of the node.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(target_os = "windows")]
fn handler(target: &str) -> Result<std::process::Command, String> {
    let mut command = std::process::Command::new("explorer.exe");
    command.arg(target);
    Ok(command)
}

#[cfg(target_os = "macos")]
fn handler(target: &str) -> Result<std::process::Command, String> {
    let mut command = std::process::Command::new("open");
    command.arg(target);
    Ok(command)
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
fn handler(target: &str) -> Result<std::process::Command, String> {
    let mut command = std::process::Command::new("xdg-open");
    command.arg(target);
    Ok(command)
}

#[cfg(not(any(unix, target_os = "windows")))]
fn handler(_target: &str) -> Result<std::process::Command, String> {
    Err("opening links is not supported on this platform".into())
}

#[cfg(target_os = "android")]
fn handler(_target: &str) -> Result<std::process::Command, String> {
    Err("a phone opens shares through its own notification, not here".into())
}
