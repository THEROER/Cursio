//! Windows self-elevation: ask for admin rights once at startup, and carry on
//! without them if the user says no.
//!
//! **Why Cursio wants admin at all.** Windows' UIPI (User Interface Privilege
//! Isolation) refuses injected input from a process whose integrity level is
//! lower than the target window's. `SendInput` doesn't fail — it returns success
//! and the OS quietly drops the event. So a medium-integrity Cursio can drive
//! Firefox but *not* an elevated console or Task Manager: the peer's mouse and
//! keyboard look dead over exactly those windows, with nothing in the log to
//! explain it. Running elevated removes that blind spot.
//!
//! **Why not a `requireAdministrator` manifest.** A manifest makes elevation
//! mandatory: decline the UAC prompt and Windows refuses to start the process at
//! all. Cursio is perfectly useful unelevated — clipboard, audio, and input to
//! ordinary windows all work — so refusing to run would be worse than the
//! limitation. Asking at runtime lets "no" mean "carry on with less".
//!
//! **Shape of the flow.** If already elevated, do nothing. Otherwise re-launch
//! ourselves via `ShellExecuteExW` with the `runas` verb (this is what raises the
//! UAC dialog), and:
//! - accepted → the elevated copy takes over and this one exits, so there is
//!   never a second node fighting over the same port and portal;
//! - declined (`ERROR_CANCELLED`) → log the consequence plainly and continue.
//!
//! On non-Windows this is a no-op, so callers need no `#[cfg]`.

/// Whether this process has a console window someone is watching.
///
/// A node started from a terminal has one, and a copy of itself it starts
/// should be seen there too. One started by the app or at login was started
/// without a console, and a copy it starts must stay without one: Windows gives
/// every console program that has none a fresh, visible window.
#[cfg(target_os = "windows")]
pub fn has_console() -> bool {
    // SAFETY: no arguments; returns the calling process's console window or null.
    !unsafe { windows::Win32::System::Console::GetConsoleWindow() }.is_invalid()
}

#[cfg(not(target_os = "windows"))]
pub fn has_console() -> bool {
    true
}

/// What happened when we asked for elevation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Running with admin rights: input reaches every window.
    Elevated,
    /// Running without them, by choice or because elevation isn't available.
    /// Everything works except input aimed at higher-integrity windows.
    Unelevated,
    /// An elevated copy was started; this process should exit immediately.
    Relaunched,
}

#[cfg(not(target_os = "windows"))]
pub fn ensure_elevated() -> Elevation {
    // Linux has no UIPI equivalent: uinput injection is governed by device
    // permissions (the `input` group), which are checked where capture starts.
    Elevation::Unelevated
}

#[cfg(target_os = "windows")]
pub fn ensure_elevated() -> Elevation {
    if is_elevated() {
        tracing::info!("running elevated; input can reach all windows");
        return Elevation::Elevated;
    }

    match relaunch_as_admin() {
        Ok(true) => {
            tracing::info!("elevated instance started; this one is stepping aside");
            Elevation::Relaunched
        }
        Ok(false) => {
            // The user declined. Not an error — just a smaller Cursio.
            tracing::warn!(
                "continuing without admin rights: input will NOT reach windows running as \
                 administrator (an elevated console, Task Manager). Everything else works."
            );
            Elevation::Unelevated
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not request elevation; continuing without admin rights"
            );
            Elevation::Unelevated
        }
    }
}

#[cfg(target_os = "windows")]
fn is_elevated() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: standard token query. The handle is closed on every path, and
    // GetTokenInformation writes into a correctly-sized local.
    unsafe {
        let mut token = Default::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }

        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();

        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Re-launch this executable with the `runas` verb, raising the UAC prompt.
///
/// Returns `Ok(true)` if an elevated copy started, `Ok(false)` if the user
/// declined. Arguments are forwarded so the new process runs the same command.
#[cfg(target_os = "windows")]
fn relaunch_as_admin() -> Result<bool, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_CANCELLED;
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;

    // Rebuild the command line, quoting each argument so paths with spaces
    // survive the round-trip through the shell.
    let args = std::env::args()
        .skip(1)
        .map(|a| format!("\"{}\"", a.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ");

    let verb = to_wide("runas");
    let file = to_wide(&exe.to_string_lossy());
    let params = to_wide(&args);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        // An elevated process cannot share an unelevated console, so the copy
        // always gets a new one; it is shown only if this one had one to show.
        nShow: if has_console() { SW_SHOWNORMAL.0 } else { SW_HIDE.0 },
        ..Default::default()
    };

    // SAFETY: `info` is fully initialised and the wide strings outlive the call.
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok(true),
        Err(e) if e.code().0 as u32 & 0xFFFF == ERROR_CANCELLED.0 => Ok(false),
        Err(e) => Err(format!("ShellExecuteExW: {e}")),
    }
}

#[cfg(target_os = "windows")]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
