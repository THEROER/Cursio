//! Choosing which window to transfer.
//!
//! A stand-in for the real trigger. Once dragging a window past the screen edge
//! starts a transfer (milestone 5), the window is whatever the user is dragging
//! and none of this is needed. Until then the caller names one, and an empty
//! name means "the window in front", which is the closest thing to intent
//! available without a gesture.

use windows::core::BOOL;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, TRUE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
};

use crate::WindowError;

/// Enumeration state: the title we're looking for, and the best match so far.
struct Search {
    needle: String,
    found: Option<(isize, String)>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam.0 as *mut Search);

    // Invisible windows are skipped: every process has hidden helper windows,
    // and transferring one would show the peer a blank rectangle.
    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }
    let len = GetWindowTextLengthW(hwnd);
    if len == 0 {
        return TRUE; // untitled — a tool or message-only window
    }

    let mut buf = vec![0u16; len as usize + 1];
    let got = GetWindowTextW(hwnd, &mut buf);
    if got == 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&buf[..got as usize]);
    if title.to_lowercase().contains(&search.needle) {
        search.found = Some((hwnd.0 as isize, title));
        return FALSE; // stop enumerating
    }
    TRUE
}

/// Find a capturable window whose title contains `hint`.
///
/// An empty `hint` returns the foreground window.
pub fn find(hint: &str) -> Result<(HWND, String), WindowError> {
    unsafe {
        if hint.is_empty() {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return Err(WindowError::WindowGone("no foreground window".into()));
            }
            let len = GetWindowTextLengthW(hwnd).max(0) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
            let title = String::from_utf16_lossy(&buf[..got]);
            return Ok((hwnd, title));
        }

        let mut search = Search {
            needle: hint.to_lowercase(),
            found: None,
        };
        // `EnumWindows` reports an error when the callback stops it early, which
        // is exactly how a successful match exits — so the result is ignored and
        // `found` is the real answer.
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut Search as isize));

        match search.found {
            Some((bits, title)) => Ok((HWND(bits as *mut core::ffi::c_void), title)),
            None => Err(WindowError::WindowGone(format!(
                "no visible window with {hint:?} in its title"
            ))),
        }
    }
}
