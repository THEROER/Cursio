//! Making a remote-screen view a window nobody sees but every capture app
//! lists — and giving its selector somewhere to live.
//!
//! A view exists to be picked in Discord's or OBS's window list, never to be
//! looked at here. `tools/wgc-probe --selftest` measured on the Windows peer
//! (2026-09-27) which states keep a window in both lists and recorded:
//!
//! | state | Chromium/WebRTC list | OBS list | WGC frames in 2 s | picture |
//! |---|---|---|---|---|
//! | on screen | yes | yes | 43 | yes |
//! | off the virtual desktop | yes | yes | **1** | stale |
//! | layered, alpha 0, on screen | yes | yes | 43 | yes |
//! | **layered, alpha 0, off the desktop, `WS_EX_NOACTIVATE`** | yes | yes | 44 | yes |
//! | `WS_EX_TOOLWINDOW` | **no** | **no** | 43 | yes |
//!
//! Off the desktop alone stops DWM refreshing the window; layered with alpha 0
//! keeps it composed, with the picture intact in the capture (alpha applies to
//! how the window is blended onto the screen, not to its surface). Both lists
//! filter tool windows and cloaked ones, so neither of those hides it.
//! `WS_EX_NOACTIVATE` keeps it off the taskbar and out of Alt+Tab while
//! leaving it in both lists (they filter owned windows, not unactivatable
//! ones), and off the desktop it can never sit under the pointer.
//!
//! The view's selector used to be a right-click menu on the window; nobody can
//! right-click a window that is nowhere, so the same menu hangs off a
//! notification-area icon, one per peer.

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, GetWindowLongW, LoadIconW, RegisterWindowMessageW,
    SetLayeredWindowAttributes, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, IDI_APPLICATION,
    LWA_ALPHA, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WM_APP, WS_EX_LAYERED, WS_EX_NOACTIVATE,
};

/// The message the notification-area icon reports its clicks with.
pub(super) const TRAY_MESSAGE: u32 = WM_APP + 1;

/// Make `hwnd` unseen: layered and fully transparent, never activated, and
/// placed past the bottom-right corner of every monitor.
pub(super) unsafe fn hide(hwnd: HWND) {
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let _ = SetWindowLongW(
        hwnd,
        GWL_EXSTYLE,
        (ex | WS_EX_LAYERED.0 | WS_EX_NOACTIVATE.0) as i32,
    );
    if let Err(e) = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA) {
        tracing::warn!(error = %e, "could not make the remote-screen view transparent");
    }
    move_off_desktop(hwnd);
}

/// Put `hwnd` just past the virtual desktop. Again whenever the monitor
/// layout changes, or a new monitor could land on top of it.
pub(super) unsafe fn move_off_desktop(hwnd: HWND) {
    let right = GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let bottom = GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN);
    let _ = SetWindowPos(
        hwnd,
        None,
        right + 100,
        bottom + 100,
        0,
        0,
        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// The message Explorer broadcasts when it restarts, after which every
/// notification-area icon has to be added again.
pub(super) unsafe fn taskbar_created() -> u32 {
    RegisterWindowMessageW(windows::core::w!("TaskbarCreated"))
}

fn icon_data(hwnd: HWND, tip: &str) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: TRAY_MESSAGE,
        hIcon: unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default(),
        ..Default::default()
    };
    for (slot, unit) in data.szTip.iter_mut().take(127).zip(tip.encode_utf16()) {
        *slot = unit;
    }
    data
}

/// Show the view's icon, titled like the view itself.
pub(super) unsafe fn add_icon(hwnd: HWND, tip: &str) {
    if !Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd, tip)).as_bool() {
        tracing::warn!(
            "could not add the remote-screen icon to the notification area; \
             the view still works, but its source can only change from the peer's default"
        );
    }
}

pub(super) unsafe fn remove_icon(hwnd: HWND) {
    let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd, ""));
}
