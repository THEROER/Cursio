//! What a Windows host offers a peer's remote-screen view.
//!
//! Every monitor, then every window a user would call a window: visible,
//! titled, unowned, not a tool window and not cloaked by DWM. The last test
//! matters more than it looks — suspended UWP apps and windows on other
//! virtual desktops pass every other check and capture as a blank rectangle.
//!
//! Ids are recomputed rather than remembered. A window's id is its handle (a
//! handle stays fixed for the window's life and fits in 31 bits in practice);
//! a monitor's is its enumeration index with the top bit set, since an
//! `HMONITOR` is reissued whenever the display layout changes. `resolve`
//! enumerates again and matches, so an id the viewer holds for a window that
//! has since closed fails by name instead of capturing a recycled handle.

use cursio_protocol::{ScreenSource, SourceId, SourceKind};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, GW_OWNER, WS_EX_TOOLWINDOW,
};

use super::Target;
use crate::WindowError;

/// Marks a [`SourceId`] as a monitor index rather than a window handle.
const MONITOR_BIT: SourceId = 0x8000_0000;

/// The window title every remote-screen view carries, so a machine viewing
/// this one is never offered its own view back — a hall of mirrors.
const VIEW_TITLE_PREFIX: &str = "Cursio — ";

fn window_id(hwnd: isize) -> SourceId {
    (hwnd as usize as u32) & !MONITOR_BIT
}

fn monitor_id(index: usize) -> SourceId {
    MONITOR_BIT | index as SourceId
}

fn monitor_index(id: SourceId) -> Option<usize> {
    (id & MONITOR_BIT != 0).then_some((id & !MONITOR_BIT) as usize)
}

/// Whether a window with this title and these traits belongs in the list.
fn offerable(title: &str, owned: bool, tool: bool, cloaked: bool) -> bool {
    !title.is_empty() && !owned && !tool && !cloaked && !title.starts_with(VIEW_TITLE_PREFIX)
}

struct Monitor {
    handle: isize,
    primary: bool,
    width: u32,
    height: u32,
}

unsafe extern "system" fn monitor_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let found = &mut *(lparam.0 as *mut Vec<Monitor>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        let r = info.rcMonitor;
        found.push(Monitor {
            handle: monitor.0 as isize,
            primary: info.dwFlags & 1 != 0,
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
        });
    }
    TRUE
}

fn monitors() -> Vec<Monitor> {
    let mut found: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only runs during this call and writes to `found`,
    // which outlives it.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(monitor_proc),
            LPARAM(&mut found as *mut Vec<Monitor> as isize),
        );
    }
    found
}

struct Window {
    handle: isize,
    title: String,
    width: u32,
    height: u32,
}

unsafe extern "system" fn window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = &mut *(lparam.0 as *mut Vec<Window>);
    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }
    // This process's own windows are never offered — a view or a transfer's
    // surrogate shown back to the peer it came from is a hall of mirrors — and
    // asking one its title waits on its thread, which is busy decoding the very
    // stream the peer is watching.
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
    if pid == std::process::id() {
        return TRUE;
    }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return TRUE;
    }
    let mut buf = vec![0u16; len as usize + 1];
    let got = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
    let title = String::from_utf16_lossy(&buf[..got]);

    let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.0.is_null());
    let tool = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0;
    let mut cloaked: u32 = 0;
    let cloaked = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
        && cloaked != 0;
    if !offerable(&title, owned, tool, cloaked) {
        return TRUE;
    }
    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    found.push(Window {
        handle: hwnd.0 as isize,
        title,
        width: (rect.right - rect.left).max(0) as u32,
        height: (rect.bottom - rect.top).max(0) as u32,
    });
    TRUE
}

fn windows() -> Vec<Window> {
    let mut found: Vec<Window> = Vec::new();
    // SAFETY: as for `monitors`.
    unsafe {
        let _ = EnumWindows(
            Some(window_proc),
            LPARAM(&mut found as *mut Vec<Window> as isize),
        );
    }
    found
}

/// Monitors first, primary first, then windows front to back.
pub(super) fn list() -> Vec<ScreenSource> {
    let mut out = Vec::new();
    let mut screens = monitors();
    // Stable, so the non-primary monitors keep the system's order.
    screens.sort_by_key(|m| !m.primary);
    for (index, m) in screens.iter().enumerate() {
        let primary = if m.primary { ", primary" } else { "" };
        out.push(ScreenSource {
            id: monitor_id(index),
            kind: SourceKind::Monitor,
            title: format!("Screen {} ({}x{}{primary})", index + 1, m.width, m.height),
            width: m.width,
            height: m.height,
            picked_on_host: false,
        });
    }
    for w in windows() {
        out.push(ScreenSource {
            id: window_id(w.handle),
            kind: SourceKind::Window,
            title: w.title,
            width: w.width,
            height: w.height,
            picked_on_host: false,
        });
    }
    out
}

/// What to capture for `source`, found afresh.
pub(super) fn resolve(source: &ScreenSource) -> Result<Target, WindowError> {
    if let Some(index) = monitor_index(source.id) {
        let mut screens = monitors();
        screens.sort_by_key(|m| !m.primary);
        let m = screens.get(index).ok_or_else(|| {
            WindowError::WindowGone(format!("{} is no longer connected", source.title))
        })?;
        return Ok(Target::Monitor {
            monitor: m.handle,
            width: m.width,
            height: m.height,
        });
    }
    windows()
        .into_iter()
        .find(|w| window_id(w.handle) == source.id)
        .map(|w| Target::Window {
            hwnd: w.handle,
            // A view shows the window where it is; hiding it would take it
            // away from the person sitting at this machine.
            hide: false,
        })
        .ok_or_else(|| WindowError::WindowGone(format!("{} has closed", source.title)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_and_window_ids_never_collide() {
        assert_eq!(monitor_index(monitor_id(2)), Some(2));
        assert_eq!(monitor_index(window_id(0x0001_0a2c)), None);
        assert_eq!(monitor_index(window_id(-1)), None);
    }

    #[test]
    fn a_remote_screen_view_is_never_offered_back() {
        assert!(!offerable("Cursio — Nocturne", false, false, false));
        assert!(offerable("Discord", false, false, false));
        assert!(!offerable("Discord", false, false, true));
        assert!(!offerable("", false, false, false));
    }
}
