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
    #[cfg(target_os = "windows")]
    if crate::share_sink::openable_url(target) {
        return open_link(target.trim());
    }
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

/// A link on Windows goes to `url.dll`'s protocol handler, not to
/// `explorer.exe`: Explorer reads commas in its command line as its own
/// switches (`/select,`, `/root,`), so a link holding one — a map's
/// `@55.75,37.61,15z`, a list in a query — was cut there and Explorer opened
/// a folder instead. `rundll32` hands the rest of its line over whole. From an
/// elevated node it is started with the desktop's token, for the reason the
/// module docs give for Explorer.
#[cfg(target_os = "windows")]
fn open_link(url: &str) -> Result<(), String> {
    let system = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("System32");
    let rundll = system.join("rundll32.exe");
    let args = ["url.dll,FileProtocolHandler".to_owned(), url.to_owned()];
    if crate::elevate::is_elevated() {
        return crate::apps::launch::as_the_desktop_user(&rundll, &system, &args).map(drop);
    }
    let mut child = std::process::Command::new(&rundll)
        .args(&args)
        .spawn()
        .map_err(|e| format!("could not start the program that opens it: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
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
