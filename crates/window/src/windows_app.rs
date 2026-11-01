//! One application's window on this Windows machine, read and driven for a
//! program on another machine — an AI agent checking what a button did.
//!
//! The window is found by the process ids its application registered with
//! the node, never by a handle a peer names. A still is `PrintWindow` with
//! `PW_RENDERFULLCONTENT` rather than a Windows Graphics Capture session:
//! one picture is wanted, not a stream, and the full-content flag makes DWM
//! render what a WebView2 or DirectComposition window draws, which plain
//! `PrintWindow` returns black. A minimised window prints nothing; that is
//! said, not returned as a black picture.
//!
//! Input is posted to the window under the point (the deepest child, which is
//! where a WebView2 page takes its clicks) through the same `PostMessage`
//! replay a transferred window uses ([`crate::windows_input`]), so the real
//! cursor is never moved and the person at the machine is not fought. The
//! same limit holds: an application reading raw or DirectInput ignores it.

use cursio_protocol::{MouseButton, WindowInput};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, ScreenToClient,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPointEx, EnumWindows, GetGUIThreadInfo, GetWindow, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    PostMessageW, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, GUITHREADINFO, GW_OWNER, WM_CHAR,
};

/// `PrintWindow`'s "render what DWM composes" flag; not named in the
/// bindings.
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

/// One top-level window of an application.
#[derive(Debug, Clone)]
pub struct AppWindow {
    pub hwnd: isize,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

fn hwnd(raw: isize) -> HWND {
    HWND(raw as _)
}

fn title_of(window: HWND) -> String {
    // SAFETY: a buffer sized from the length the window reports.
    unsafe {
        let len = GetWindowTextLengthW(window);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let got = GetWindowTextW(window, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

fn rect_of(window: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    // SAFETY: a valid out pointer.
    unsafe { GetWindowRect(window, &mut rect) }.ok()?;
    Some(rect)
}

/// The visible, unowned top-level windows of `pids`, the largest first.
pub fn windows_of(pids: &[u32]) -> Vec<AppWindow> {
    struct Found<'a> {
        pids: &'a [u32],
        out: Vec<AppWindow>,
    }
    unsafe extern "system" fn each(window: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `Found` passed below, alive for the call.
        let found = unsafe { &mut *(data.0 as *mut Found) };
        let mut pid = 0u32;
        // SAFETY: plain queries of a window the system just named.
        unsafe {
            GetWindowThreadProcessId(window, Some(&mut pid));
            let owned = GetWindow(window, GW_OWNER).is_ok_and(|o| !o.is_invalid());
            if !found.pids.contains(&pid) || !IsWindowVisible(window).as_bool() || owned {
                return true.into();
            }
        }
        if let Some(rect) = rect_of(window) {
            let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
            if w > 0 && h > 0 {
                found.out.push(AppWindow {
                    hwnd: window.0 as isize,
                    title: title_of(window),
                    width: w as u32,
                    height: h as u32,
                });
            }
        }
        true.into()
    }
    let mut found = Found {
        pids,
        out: Vec::new(),
    };
    // SAFETY: the callback only reads `found` through the pointer given here.
    let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut found as *mut Found as isize)) };
    found
        .out
        .sort_by_key(|w| std::cmp::Reverse(u64::from(w.width) * u64::from(w.height)));
    found.out
}

/// A still of the window: width, height and RGBA pixels.
pub fn still(raw: isize) -> Result<(u32, u32, Vec<u8>), String> {
    let window = hwnd(raw);
    // SAFETY: a query of the window.
    if unsafe { IsIconic(window) }.as_bool() {
        return Err("the window is minimised, and a minimised window draws nothing".into());
    }
    let rect = rect_of(window).ok_or("the window is gone")?;
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if w <= 0 || h <= 0 {
        return Err("the window has no size".into());
    }
    // SAFETY: every GDI object created here is selected out and released
    // before returning; `bits` is read only while the section lives.
    unsafe {
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative: rows top-down, as an image is written.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let section = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0);
        let result = match section {
            Ok(section) => {
                let old = SelectObject(memory, HGDIOBJ(section.0));
                let printed =
                    PrintWindow(window, memory, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();
                let pixels = if printed && !bits.is_null() {
                    let len = (w * h * 4) as usize;
                    let mut rgba = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
                    for px in rgba.chunks_exact_mut(4) {
                        px.swap(0, 2);
                        px[3] = 255;
                    }
                    Ok(rgba)
                } else {
                    Err("the window would not print itself".to_owned())
                };
                SelectObject(memory, old);
                let _ = DeleteObject(HGDIOBJ(section.0));
                pixels
            }
            Err(e) => Err(format!("no bitmap for the picture: {e}")),
        };
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        result.map(|rgba| (w as u32, h as u32, rgba))
    }
}

/// The deepest visible child of `window` under the screen point, and the
/// point in its own client coordinates.
fn target_at(window: HWND, screen: POINT) -> (HWND, POINT) {
    let mut target = window;
    // SAFETY: plain window queries; the loop ends at a window with no child
    // under the point, which every chain does.
    unsafe {
        loop {
            let mut local = screen;
            let _ = ScreenToClient(target, &mut local);
            let child =
                ChildWindowFromPointEx(target, local, CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT);
            if child.is_invalid() || child == target {
                return (target, local);
            }
            target = child;
        }
    }
}

/// A click at a fraction of the window (0 to 1 each way).
pub fn click(raw: isize, fx: f32, fy: f32, right: bool) -> Result<(), String> {
    let window = hwnd(raw);
    let rect = rect_of(window).ok_or("the window is gone")?;
    let screen = POINT {
        x: rect.left + ((rect.right - rect.left - 1) as f32 * fx).round() as i32,
        y: rect.top + ((rect.bottom - rect.top - 1) as f32 * fy).round() as i32,
    };
    let (target, local) = target_at(window, screen);
    let button = if right {
        MouseButton::Right
    } else {
        MouseButton::Left
    };
    let (x, y) = (local.x.max(0) as u32, local.y.max(0) as u32);
    for pressed in [true, false] {
        crate::windows_input::replay(
            target,
            WindowInput::PointerButton {
                id: 0,
                x,
                y,
                button,
                pressed,
            },
        )?;
    }
    Ok(())
}

/// Where typed input goes: the window's focused control, or the window.
fn focus_of(window: HWND) -> HWND {
    // SAFETY: a sized struct and a thread id the system gave.
    unsafe {
        let thread = GetWindowThreadProcessId(window, None);
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(thread, &mut info).is_ok() && !info.hwndFocus.is_invalid() {
            return info.hwndFocus;
        }
    }
    window
}

/// Text, any script, as characters to the window's focused control.
pub fn type_text(raw: isize, text: &str) -> Result<(), String> {
    let target = focus_of(hwnd(raw));
    for unit in text.encode_utf16() {
        // SAFETY: posting to a window handle the system gave.
        unsafe { PostMessageW(Some(target), WM_CHAR, WPARAM(unit as usize), LPARAM(1)) }
            .map_err(|e| format!("WM_CHAR: {e}"))?;
    }
    Ok(())
}

/// A key, by its evdev code, pressed and released at the focused control.
pub fn key(raw: isize, evdev: u16) -> Result<(), String> {
    let target = focus_of(hwnd(raw));
    for pressed in [true, false] {
        crate::windows_input::replay(
            target,
            WindowInput::Key {
                id: 0,
                key: evdev,
                pressed,
            },
        )?;
    }
    Ok(())
}
