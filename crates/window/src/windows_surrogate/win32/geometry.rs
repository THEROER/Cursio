//! Sizing the surrogate: its client area to the picture, within the monitor's
//! work area, keeping the picture's shape while the user drags an edge.

use crate::framebuffer::{contained_to_host, refit};
use crate::windows_surrogate::win32::WindowState;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, WINDOW_EX_STYLE,
    WMSZ_BOTTOM, WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT, WS_OVERLAPPEDWINDOW,
};

/// Map a click's `lParam` to host pixel coordinates.
///
/// GDI gives client-area coordinates, which match the host's pixels only while
/// the surrogate is shown unscaled. The picture is letterboxed into the client
/// area, so the click is mapped through the same fit — the correction the GTK
/// surrogate makes.
pub(super) fn to_host(hwnd: HWND, state: &WindowState, lparam: LPARAM) -> (u32, u32) {
    let x = (lparam.0 & 0xFFFF) as i16 as f64;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f64;
    let fb = state.fb.borrow();
    let (cw, ch) = client_size(hwnd).unwrap_or((fb.width as i32, fb.height as i32));
    contained_to_host((cw as f64, ch as f64), (fb.width, fb.height), x, y)
}

pub(super) fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
    Some((rect.right - rect.left, rect.bottom - rect.top))
}

/// The work area — the monitor less the taskbar — of the monitor holding
/// `hwnd`.
pub(super) unsafe fn work_area(hwnd: HWND) -> Option<(i32, i32)> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
        return None;
    }
    let work = info.rcWork;
    Some((work.right - work.left, work.bottom - work.top))
}

/// Size the window so its *client area* is exactly `width` × `height` device
/// pixels.
///
/// `CreateWindowExW` takes the outer size, which includes the frame and the
/// title bar. Passing the picture's size there leaves a client area some 16 by
/// 39 pixels smaller, so every frame arrived downscaled by an arbitrary,
/// non-integer ratio: text lost strokes and edges crawled, on a link that had
/// just carried the pixels perfectly.
pub(super) unsafe fn fit_client(hwnd: HWND, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    let Some((fw, fh)) = frame(hwnd) else {
        return;
    };
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        width as i32 + fw,
        height as i32 + fh,
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// How much wider and taller the window is than its client area. Measured at
/// the window's own DPI rather than the system's, because the two differ on
/// any mixed-scale desktop.
pub(super) unsafe fn frame(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    let dpi = GetDpiForWindow(hwnd);
    if dpi == 0
        || AdjustWindowRectExForDpi(
            &mut rect,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE(0),
            dpi,
        )
        .is_err()
    {
        return None;
    }
    Some((rect.right - rect.left, rect.bottom - rect.top))
}

/// Size the client area for a picture of `picture` that was `old` until now.
///
/// A view keeps the source's own size: nobody sees it, and at that size the
/// capture app records it 1:1. A surrogate is fitted to its monitor's work
/// area keeping the picture's shape, and on a change of shape — a phone
/// turning — keeps the scale it was shown at, as scrcpy does: a phone's
/// 1920-high screen left at its own size is cut to the monitor by Windows,
/// which then stretched the picture across the cut window.
pub(super) unsafe fn size_for(hwnd: HWND, is_view: bool, old: (u32, u32), picture: (u32, u32)) {
    if is_view {
        fit_client(hwnd, picture.0, picture.1);
        return;
    }
    let room = work_area(hwnd).and_then(|(w, h)| {
        let (fw, fh) = frame(hwnd)?;
        Some((w - fw, h - fh))
    });
    let shown = client_size(hwnd).unwrap_or((0, 0));
    let (w, h) = refit(shown, old, picture, room);
    fit_client(hwnd, w as u32, h as u32);
}

/// Keep a surrogate's client area the picture's shape while the user drags
/// its border, so the picture fills it instead of gaining bars.
///
/// `rect` is the proposed outer rectangle; the side being dragged decides
/// which dimension follows the other, and which edge moves to make it so.
pub(super) unsafe fn keep_shape(hwnd: HWND, edge: u32, rect: &mut RECT, picture: (u32, u32)) {
    let (Some((fw, fh)), (pw, ph)) = (frame(hwnd), picture) else {
        return;
    };
    if pw == 0 || ph == 0 {
        return;
    }
    let (pw, ph) = (pw as i64, ph as i64);
    let cw = (rect.right - rect.left - fw).max(1) as i64;
    let ch = (rect.bottom - rect.top - fh).max(1) as i64;
    if edge == WMSZ_TOP || edge == WMSZ_BOTTOM {
        rect.right = rect.left + (ch * pw / ph) as i32 + fw;
    } else {
        let h = (cw * ph / pw) as i32 + fh;
        if edge == WMSZ_TOPLEFT || edge == WMSZ_TOPRIGHT {
            rect.top = rect.bottom - h;
        } else {
            rect.bottom = rect.top + h;
        }
    }
}
