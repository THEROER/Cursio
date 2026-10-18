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
//!
//! Discord's own picker is stricter than Chromium's list, and was settled live
//! against Discord itself rather than a probe (2026-09-27):
//! - it drops `WS_EX_NOACTIVATE` and `WS_EX_TRANSPARENT` windows from
//!   "Applications" entirely;
//! - it pauses a shared window ("Stream Paused — switch to your application")
//!   that is off the desktop, covered by other windows, or minimized.
//!
//! So the view sits *on* the primary monitor, topmost so nothing ever covers
//! it, and alpha 0 so nobody sees it. Alpha 0 is also what lets every click and
//! scroll fall through to whatever is underneath: a layered window is hit-tested
//! by its transparency, so `WS_EX_TRANSPARENT` would add nothing but Discord's
//! filter — confirmed live with the view covering the whole screen. Show Desktop (Win+D)
//! minimizes even topmost windows; the view restores itself at once. It is kept
//! off the taskbar through `ITaskbarList::DeleteTab`, since the styles that
//! would do that (`WS_EX_TOOLWINDOW`, `WS_EX_NOACTIVATE`, an owner) are exactly
//! the ones the pickers filter.
//!
//! The view's selector used to be a right-click menu on the window; nobody can
//! right-click a window that is nowhere, so the same menu hangs off a
//! notification-area icon, one per peer. Those icons stand aside while the
//! Cursio app is attached to the node: its tray carries the same choice for
//! every peer, and an icon per peer next to it was the same thing twice.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    ITaskbarList, Shell_NotifyIconW, TaskbarList, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
    NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, GetWindowLongW, LoadIconW, PostMessageW, RegisterWindowMessageW,
    SetLayeredWindowAttributes, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, HICON, HWND_TOPMOST,
    IDI_APPLICATION, LR_DEFAULTCOLOR, LWA_ALPHA, SWP_NOACTIVATE, SWP_NOSIZE, WM_APP, WS_EX_LAYERED,
};

/// The message the notification-area icon reports its clicks with.
pub(super) const TRAY_MESSAGE: u32 = WM_APP + 1;

/// Posted to every view when [`set_icons_shown`] changes, so each adds or
/// removes its icon on its own thread, the one that owns the window.
pub(super) const ICONS_CHANGED: u32 = WM_APP + 2;

/// Whether views keep an icon in the notification area: not while the Cursio
/// app's tray carries their selector.
static ICONS_SHOWN: AtomicBool = AtomicBool::new(true);

/// Every open view, as raw handles: `HWND` is not `Send`.
fn views() -> &'static Mutex<Vec<isize>> {
    static VIEWS: OnceLock<Mutex<Vec<isize>>> = OnceLock::new();
    VIEWS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Show or hide every view's icon, now and for views opened later.
pub fn set_icons_shown(shown: bool) {
    if ICONS_SHOWN.swap(shown, Ordering::SeqCst) == shown {
        return;
    }
    for &hwnd in views().lock().unwrap_or_else(|p| p.into_inner()).iter() {
        // SAFETY: posting to a window that has since gone only fails.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut _)),
                ICONS_CHANGED,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

pub(super) fn icons_shown() -> bool {
    ICONS_SHOWN.load(Ordering::SeqCst)
}

pub(super) fn track(hwnd: HWND) {
    views()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(hwnd.0 as isize);
}

pub(super) fn untrack(hwnd: HWND) {
    views()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|&h| h != hwnd.0 as isize);
}

/// The Cursio logo, built once from the PNG the app's icons are made from. The
/// stock application icon stood in before, and next to the app's own tray it
/// read as some other program's.
fn logo() -> HICON {
    static LOGO: OnceLock<isize> = OnceLock::new();
    let raw = *LOGO.get_or_init(|| {
        const PNG: &[u8] = include_bytes!("../../../../app/icons/32x32.png");
        // SAFETY: the bytes are a complete PNG, which icon resources may be.
        unsafe { CreateIconFromResourceEx(PNG, true, 0x0003_0000, 32, 32, LR_DEFAULTCOLOR) }
            .or_else(|_| unsafe { LoadIconW(None, IDI_APPLICATION) })
            .map(|icon| icon.0 as isize)
            .unwrap_or_default()
    });
    HICON(raw as *mut _)
}

/// Make `hwnd` unseen: layered and fully transparent (which also makes it
/// click-through), placed over the primary monitor above everything else.
pub(super) unsafe fn hide(hwnd: HWND) {
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, (ex | WS_EX_LAYERED.0) as i32);
    if let Err(e) = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA) {
        tracing::warn!(error = %e, "could not make the remote-screen view transparent");
    }
    place(hwnd);
}

/// Pin `hwnd` topmost at the primary monitor's origin. Again whenever the
/// monitor layout changes, since the primary monitor may have moved.
pub(super) unsafe fn place(hwnd: HWND) {
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

/// Take `hwnd`'s button off the taskbar, which it gains on every show.
pub(super) unsafe fn remove_from_taskbar(hwnd: HWND) {
    // The thread may already be in the multithreaded apartment for the video
    // decoder; either answer leaves COM usable here.
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let taskbar: windows::core::Result<ITaskbarList> =
        CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER);
    let removed = taskbar.and_then(|t| {
        t.HrInit()?;
        t.DeleteTab(hwnd)
    });
    if let Err(e) = removed {
        tracing::warn!(error = %e, "could not take the remote-screen view off the taskbar");
    }
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
        hIcon: logo(),
        ..Default::default()
    };
    for (slot, unit) in data.szTip.iter_mut().take(127).zip(tip.encode_utf16()) {
        *slot = unit;
    }
    data
}

/// Show the view's icon, titled like the view itself, unless the app's tray
/// stands in for it.
pub(super) unsafe fn add_icon(hwnd: HWND, tip: &str) {
    if !icons_shown() {
        return;
    }
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
