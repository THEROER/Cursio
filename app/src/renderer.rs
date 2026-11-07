//! Which engine draws the window: the platform webview, or Chromium.
//!
//! They are two binaries, `cursio-app` on the platform webview and
//! `cursio-app-cef` on Chromium (CEF). A binary linked against CEF needs
//! `libcef` beside it to start at all, and CEF brings GTK 4 while WebKitGTK
//! links GTK 3, which refuse to share a process. So the plain binary decides
//! before any window exists and replaces itself with the Chromium one when
//! that is chosen and can start.
//!
//! The platform webview is the default: it ships with the system and costs
//! nothing. Chromium is the way out on Linux machines where WebKitGTK cannot
//! use the GPU, the proprietary NVIDIA driver being the known case: there it
//! draws every frame on the CPU and the window lags. The same split, and the
//! reasons for it, come from the LeavePulse launcher, which met them first.
//!
//! `CURSIO_RENDERER=webkit|chromium|auto` chooses for one run; without it the
//! choice is automatic.

use std::path::{Path, PathBuf};

pub const ENV: &str = "CURSIO_RENDERER";

#[cfg(feature = "wry")]
const CEF_BINARY: &str = "cursio-app-cef";
#[cfg(feature = "cef")]
const WEBKIT_BINARY: &str = "cursio-app";

#[cfg(feature = "wry")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Auto,
    Webkit,
    Chromium,
}

#[cfg(feature = "wry")]
fn parse(value: &str) -> Option<Choice> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(Choice::Auto),
        "webkit" | "webview" | "native" => Some(Choice::Webkit),
        "chromium" | "cef" => Some(Choice::Chromium),
        _ => None,
    }
}

#[cfg(feature = "wry")]
fn requested() -> Choice {
    std::env::var(ENV)
        .ok()
        .and_then(|v| parse(&v))
        .unwrap_or(Choice::Auto)
}

fn sibling(name: &str) -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let exe = dir.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    });
    exe.is_file().then_some(exe)
}

#[cfg(feature = "wry")]
/// The Chromium binary beside this one, with the library it links
/// (`$ORIGIN` rpath, so both sit side by side).
fn chromium_exe() -> Option<PathBuf> {
    let exe = sibling(CEF_BINARY)?;
    let lib = if cfg!(windows) {
        "libcef.dll"
    } else {
        "libcef.so"
    };
    (cfg!(target_os = "macos") || exe.with_file_name(lib).is_file()).then_some(exe)
}

#[cfg(feature = "cef")]
/// The platform-webview binary beside this one, to fall back to from Chromium.
pub fn webkit_exe() -> Option<PathBuf> {
    sibling(WEBKIT_BINARY)
}

#[cfg(feature = "wry")]
/// The proprietary NVIDIA driver, on which WebKitGTK sets its hardware
/// acceleration policy to "never". The driver's version file exists only while
/// that driver is loaded (nouveau and NVK do not create it).
fn webkit_lacks_gpu() -> bool {
    cfg!(target_os = "linux") && Path::new("/proc/driver/nvidia/version").exists()
}

/// Whether Chromium will find a sandbox here.
///
/// Without one it does not fail politely: it aborts with "No usable sandbox!"
/// before any window, and since this process has already been replaced by
/// then, nothing is left to fall back to the platform webview. So the
/// question is asked before handing over. A sandbox is the setuid-root
/// `chrome-sandbox` beside the binary, a helper named by
/// `CHROME_DEVEL_SANDBOX`, or unprivileged user namespaces — which Ubuntu
/// 24.04 and later hold back behind AppArmor, so there the helper is the only
/// way. A debug build runs Chromium without the sandbox (see `cef_runtime`).
#[cfg(all(target_os = "linux", feature = "wry"))]
fn chromium_sandbox_usable(exe: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    if cfg!(debug_assertions) || std::env::var_os("CHROME_DEVEL_SANDBOX").is_some() {
        return true;
    }
    let helper = exe.with_file_name("chrome-sandbox");
    if std::fs::metadata(&helper).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o4000 != 0) {
        return true;
    }
    let sysctl = |name: &str| {
        std::fs::read_to_string(format!("/proc/sys/kernel/{name}"))
            .ok()
            .map(|v| v.trim().to_owned())
    };
    sysctl("apparmor_restrict_unprivileged_userns").as_deref() != Some("1")
        && sysctl("unprivileged_userns_clone").as_deref() != Some("0")
}

#[cfg(all(not(target_os = "linux"), feature = "wry"))]
fn chromium_sandbox_usable(_exe: &Path) -> bool {
    true
}

#[cfg(feature = "wry")]
/// The Chromium binary to run instead of this one, if it is chosen and can
/// start.
fn chromium_for_this_run() -> Option<PathBuf> {
    let choice = requested();
    if choice == Choice::Webkit || (choice == Choice::Auto && !webkit_lacks_gpu()) {
        return None;
    }
    let Some(exe) = chromium_exe() else {
        if choice == Choice::Chromium {
            eprintln!("Chromium was asked for but is not installed beside cursio-app");
        }
        return None;
    };
    if !chromium_sandbox_usable(&exe) {
        eprintln!(
            "Chromium would find no sandbox here, so the platform webview draws the window: \
             make {} owned by root with mode 4755, or point CHROME_DEVEL_SANDBOX at such a helper",
            exe.with_file_name("chrome-sandbox").display()
        );
        return None;
    }
    Some(exe)
}

#[cfg(feature = "wry")]
/// Replace this process with the Chromium binary when that is the engine for
/// this run. Returns when the platform webview stays.
pub fn hand_over_to_chromium() {
    if let Some(exe) = chromium_for_this_run() {
        let error = hand_over(&exe);
        eprintln!("could not start {}: {error}", exe.display());
    }
}

/// Replace this process with `exe`, keeping the arguments. Returns only if the
/// new process could not be started.
pub fn hand_over(exe: &Path) -> std::io::Error {
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.exec()
    }
    #[cfg(not(unix))]
    {
        match command.spawn() {
            Ok(_) => std::process::exit(0),
            Err(error) => error,
        }
    }
}

#[cfg(all(test, feature = "wry"))]
mod tests {
    use super::*;

    #[test]
    fn a_renderer_is_named_by_its_engine_or_its_project() {
        assert_eq!(parse(" Chromium "), Some(Choice::Chromium));
        assert_eq!(parse("cef"), Some(Choice::Chromium));
        assert_eq!(parse("webkit"), Some(Choice::Webkit));
        assert_eq!(parse("auto"), Some(Choice::Auto));
        assert_eq!(parse("gecko"), None);
    }
}
