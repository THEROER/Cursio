//! The Win32 window itself: create it, paint the framebuffer, harvest input.
//!
//! Runs entirely on its own thread — a window belongs to the thread that
//! created it, and its message loop cannot be driven from anywhere else.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cursio_protocol::ScreenSource;
use cursio_protocol::{MouseButton, Rect, WindowInput, WindowMeta};
use tokio::sync::mpsc;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, DrawTextW, EndPaint, FillRect, GetStockObject, InvalidateRect, SetBkMode,
    SetBrushOrgEx, SetStretchBltMode, SetTextColor, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, BLACK_BRUSH, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, HALFTONE, HBRUSH,
    PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};

use crate::SourcePick;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetWindowLongPtrW, PeekMessageW, PostQuitMessage, RegisterClassW, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
    MA_NOACTIVATE, MSG, PM_REMOVE, SIZE_MINIMIZED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    SW_SHOW, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WMSZ_BOTTOM, WMSZ_TOP, WMSZ_TOPLEFT,
    WMSZ_TOPRIGHT, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SIZE, WM_SIZING, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use crate::framebuffer::{contained_to_host, refit, Framebuffer};
use crate::video_decoder_win::VideoDecoder;

use super::{unseen, SurrogateCommand};

/// State the window procedure needs, reached through `GWLP_USERDATA`.
///
/// A window procedure is a bare `extern "system"` function with no place to put
/// context, so the pointer is stashed on the window itself. Everything here is
/// touched only from this one thread.
struct WindowState {
    id: u32,
    fb: RefCell<Framebuffer>,
    input_tx: mpsc::UnboundedSender<WindowInput>,
    presented_tx: mpsc::UnboundedSender<u32>,
    pending_paint_seq: RefCell<Option<u32>>,
    closed: Arc<AtomicBool>,
    /// Set when this window is a remote-screen view rather than a transfer's
    /// surrogate.
    view: Option<ViewState>,
}

/// What a remote-screen view needs beyond a surrogate.
struct ViewState {
    picks: mpsc::UnboundedSender<SourcePick>,
    sources: RefCell<Vec<ScreenSource>>,
    /// Text drawn while nothing streams; cleared by the first frame.
    hint: RefCell<Option<String>>,
    /// The icon's tooltip: the view's own title.
    tip: String,
    /// Explorer's restart broadcast, after which the icon must be re-added.
    taskbar_created: u32,
}

/// What an idle view shows — seen only in a capture app's preview, which is
/// exactly where someone wonders why the picture is empty.
/// It names both places the choice lives: the view's own icon, and the app
/// tray's "Show in Discord/OBS", which hides that icon while it is attached.
const VIEW_HINT: &str = "Choose what to show: Cursio icon → Show in Discord/OBS";

/// The menu command for "show nothing"; sources use their index plus one.
const STOP_COMMAND: usize = 0xFFFF;

/// One line of the view's context menu.
#[derive(Debug, PartialEq, Eq)]
enum MenuEntry {
    Item { command: usize, label: String },
    Disabled(String),
    Separator,
}

/// The view's menu: screens, then windows, then a way to show nothing.
fn menu_entries(sources: &[ScreenSource]) -> Vec<MenuEntry> {
    use cursio_protocol::SourceKind;
    let mut out = Vec::new();
    for kind in [SourceKind::Monitor, SourceKind::Window] {
        let before = out.len();
        for (index, source) in sources.iter().enumerate().filter(|(_, s)| s.kind == kind) {
            out.push(MenuEntry::Item {
                command: index + 1,
                label: source.title.clone(),
            });
        }
        if out.len() > before {
            out.push(MenuEntry::Separator);
        }
    }
    if sources.is_empty() {
        out.push(MenuEntry::Disabled("Waiting for the peer's list…".into()));
        out.push(MenuEntry::Separator);
    }
    out.push(MenuEntry::Item {
        command: STOP_COMMAND,
        label: "Show nothing".into(),
    });
    out
}

/// Show the view's context menu at the cursor and report the choice.
///
/// `TrackPopupMenu` opens a window of its own (class `#32768`), and window
/// capture records only the captured `HWND`, so the menu stays out of what a
/// screen-sharing application sends.
unsafe fn show_source_menu(hwnd: HWND, view: &ViewState) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, SetForegroundWindow,
        TrackPopupMenu, MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    };

    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let sources = view.sources.borrow().clone();
    for entry in menu_entries(&sources) {
        let appended = match entry {
            MenuEntry::Item { command, label } => {
                let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
                AppendMenuW(menu, MF_STRING, command, PCWSTR(wide.as_ptr()))
            }
            MenuEntry::Disabled(label) => {
                let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
                AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(wide.as_ptr()))
            }
            MenuEntry::Separator => AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()),
        };
        if let Err(e) = appended {
            tracing::debug!(error = %e, "could not build the source menu");
        }
    }
    let mut at = POINT::default();
    let _ = GetCursorPos(&mut at);
    // Without this the menu does not close when the user clicks elsewhere — a
    // documented `TrackPopupMenu` quirk.
    let _ = SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        at.x,
        at.y,
        None,
        hwnd,
        None,
    )
    .0 as usize;
    let _ = DestroyMenu(menu);

    let pick = match chosen {
        0 => return,
        STOP_COMMAND => SourcePick::Stop,
        n => match sources.get(n - 1) {
            Some(source) => SourcePick::Source(source.id),
            None => return,
        },
    };
    let _ = view.picks.send(pick);
}

/// Map a click's `lParam` to host pixel coordinates.
///
/// GDI gives client-area coordinates, which match the host's pixels only while
/// the surrogate is shown unscaled. The picture is letterboxed into the client
/// area, so the click is mapped through the same fit — the correction the GTK
/// surrogate makes.
fn to_host(hwnd: HWND, state: &WindowState, lparam: LPARAM) -> (u32, u32) {
    let x = (lparam.0 & 0xFFFF) as i16 as f64;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f64;
    let fb = state.fb.borrow();
    let (cw, ch) = client_size(hwnd).unwrap_or((fb.width as i32, fb.height as i32));
    contained_to_host((cw as f64, ch as f64), (fb.width, fb.height), x, y)
}

fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
    Some((rect.right - rect.left, rect.bottom - rect.top))
}

/// The work area — the monitor less the taskbar — of the monitor holding
/// `hwnd`.
unsafe fn work_area(hwnd: HWND) -> Option<(i32, i32)> {
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

/// Windows virtual-key code → the evdev code the protocol speaks.
///
/// The inverse of the host-side table in
/// [`windows_input`](crate::windows_input): the wire format is evdev
/// everywhere, so Windows is the side that translates — going out here, coming
/// in there.
fn evdev_from_vk(vk: u16) -> Option<u16> {
    Some(match vk {
        0x1B => 1,                       // ESC
        0x31..=0x39 => (vk - 0x31) + 2,  // '1'..'9'
        0x30 => 11,                      // '0'
        0xBD => 12,                      // minus
        0xBB => 13,                      // equal
        0x08 => 14,                      // backspace
        0x09 => 15,                      // tab
        0x51 => 16,                      // q
        0x57 => 17,                      // w
        0x45 => 18,                      // e
        0x52 => 19,                      // r
        0x54 => 20,                      // t
        0x59 => 21,                      // y
        0x55 => 22,                      // u
        0x49 => 23,                      // i
        0x4F => 24,                      // o
        0x50 => 25,                      // p
        0xDB => 26,                      // [
        0xDD => 27,                      // ]
        0x0D => 28,                      // enter
        0x11 => 29,                      // ctrl
        0x41 => 30,                      // a
        0x53 => 31,                      // s
        0x44 => 32,                      // d
        0x46 => 33,                      // f
        0x47 => 34,                      // g
        0x48 => 35,                      // h
        0x4A => 36,                      // j
        0x4B => 37,                      // k
        0x4C => 38,                      // l
        0xBA => 39,                      // ;
        0xDE => 40,                      // '
        0xC0 => 41,                      // `
        0x10 => 42,                      // shift
        0xDC => 43,                      // backslash
        0x5A => 44,                      // z
        0x58 => 45,                      // x
        0x43 => 46,                      // c
        0x56 => 47,                      // v
        0x42 => 48,                      // b
        0x4E => 49,                      // n
        0x4D => 50,                      // m
        0xBC => 51,                      // ,
        0xBE => 52,                      // .
        0xBF => 53,                      // /
        0x12 => 56,                      // alt
        0x20 => 57,                      // space
        0x14 => 58,                      // caps lock
        0x70..=0x79 => (vk - 0x70) + 59, // F1..F10
        0x7A => 87,                      // F11
        0x7B => 88,                      // F12
        0x24 => 102,                     // home
        0x26 => 103,                     // up
        0x21 => 104,                     // page up
        0x25 => 105,                     // left
        0x27 => 106,                     // right
        0x23 => 107,                     // end
        0x28 => 108,                     // down
        0x22 => 109,                     // page down
        0x2D => 110,                     // insert
        0x2E => 111,                     // delete
        0x5B => 125,                     // left Windows key
        0x5C => 126,                     // right Windows key
        _ => return None,
    })
}

/// Size the window so its *client area* is exactly `width` × `height` device
/// pixels.
///
/// `CreateWindowExW` takes the outer size, which includes the frame and the
/// title bar. Passing the picture's size there leaves a client area some 16 by
/// 39 pixels smaller, so every frame arrived downscaled by an arbitrary,
/// non-integer ratio: text lost strokes and edges crawled, on a link that had
/// just carried the pixels perfectly.
unsafe fn fit_client(hwnd: HWND, width: u32, height: u32) {
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
unsafe fn frame(hwnd: HWND) -> Option<(i32, i32)> {
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
unsafe fn size_for(hwnd: HWND, is_view: bool, old: (u32, u32), picture: (u32, u32)) {
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
unsafe fn keep_shape(hwnd: HWND, edge: u32, rect: &mut RECT, picture: (u32, u32)) {
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

/// The window procedure: painting and input.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
    if ptr.is_null() {
        // Messages arrive before the state is attached (WM_CREATE and friends).
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &*ptr;

    // A view is view-only and unseen: nothing can point at it, and its
    // selector is the notification-area icon's menu.
    if let Some(view) = state.view.as_ref() {
        match msg {
            unseen::TRAY_MESSAGE => {
                // The low word of lParam is the mouse message on the icon.
                let click = (lparam.0 & 0xFFFF) as u32;
                if click == WM_RBUTTONUP || click == WM_LBUTTONUP {
                    show_source_menu(hwnd, view);
                }
                return LRESULT(0);
            }
            WM_DISPLAYCHANGE => {
                unseen::place(hwnd);
                return LRESULT(0);
            }
            // Show Desktop minimizes even topmost windows, and a capture app
            // pauses a minimized source.
            WM_SIZE if wparam.0 as u32 == SIZE_MINIMIZED => {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                unseen::place(hwnd);
                return LRESULT(0);
            }
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            unseen::ICONS_CHANGED => {
                if unseen::icons_shown() {
                    unseen::add_icon(hwnd, &view.tip);
                } else {
                    unseen::remove_icon(hwnd);
                }
                return LRESULT(0);
            }
            m if m == view.taskbar_created => {
                unseen::add_icon(hwnd, &view.tip);
                unseen::remove_from_taskbar(hwnd);
                return LRESULT(0);
            }
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
            | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP | WM_MOUSEWHEEL
            | WM_KEYDOWN | WM_KEYUP => {
                return LRESULT(0);
            }
            _ => {}
        }
    }

    match msg {
        WM_SIZING if state.view.is_none() => {
            let fb = state.fb.borrow();
            keep_shape(
                hwnd,
                wparam.0 as u32,
                &mut *(lparam.0 as *mut RECT),
                (fb.width, fb.height),
            );
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let fb = state.fb.borrow();
            let mut painted = false;
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let (cw, ch) = (
                (client.right - client.left).max(0) as u32,
                (client.bottom - client.top).max(0) as u32,
            );
            // Letterboxed, never stretched: a view keeps one window while its
            // source changes shape, and a surrogate's client area can lose the
            // picture's shape to a maximize or to Windows cutting it to the
            // monitor. Nothing else covers the bars, so they are painted here.
            let (dx, dy, dw, dh) = {
                let (x, y, w, h) = crate::framebuffer::letterbox(fb.width, fb.height, cw, ch);
                // Only the bars, never the whole client first: DWM composes a
                // redirected window whenever it likes, and a capture taken
                // between a full black fill and the blit over it records a
                // black frame — measured on the peer as every other capture
                // of the view coming back black, which is flicker in a share.
                let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
                let (x, y, w, h) = (x as i32, y as i32, w as i32, h as i32);
                for bar in [
                    RECT {
                        left: 0,
                        top: 0,
                        right: client.right,
                        bottom: y,
                    },
                    RECT {
                        left: 0,
                        top: y + h,
                        right: client.right,
                        bottom: client.bottom,
                    },
                    RECT {
                        left: 0,
                        top: y,
                        right: x,
                        bottom: y + h,
                    },
                    RECT {
                        left: x + w,
                        top: y,
                        right: client.right,
                        bottom: y + h,
                    },
                ] {
                    if bar.right > bar.left && bar.bottom > bar.top {
                        FillRect(hdc, &bar, black);
                    }
                }
                (x as u32, y as u32, w as u32, h as u32)
            };
            if fb.width > 0 && fb.height > 0 {
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: fb.width as i32,
                        // Negative height = top-down rows. Our framebuffer's
                        // first row is the top one; without this the image
                        // would paint upside down.
                        biHeight: -(fb.height as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                // The client area is kept at the host's resolution where the
                // monitor has room, so this is normally a straight copy. It
                // stops being one for a picture taller than the monitor, or the
                // moment the user resizes the surrogate, and GDI's default stretch mode
                // (`BLACKONWHITE`) then *drops* whole rows and columns rather
                // than resampling — which is what turned a scaled window into
                // broken text and jagged edges. `HALFTONE` averages instead,
                // and requires the brush origin to be reset after it or later
                // fills tile from the wrong offset.
                SetStretchBltMode(hdc, HALFTONE);
                let _ = SetBrushOrgEx(hdc, 0, 0, None);
                painted = StretchDIBits(
                    hdc,
                    dx as i32,
                    dy as i32,
                    dw as i32,
                    dh as i32,
                    0,
                    0,
                    fb.width as i32,
                    fb.height as i32,
                    Some(fb.pixels.as_ptr() as *const _),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                ) != 0;
            }
            if let Some(text) = state.view.as_ref().and_then(|v| v.hint.borrow().clone()) {
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                let mut area = client;
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, COLORREF(0x00FF_FFFF));
                DrawTextW(
                    hdc,
                    &mut wide,
                    &mut area,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
            }
            let ended = EndPaint(hwnd, &ps).as_bool();
            if painted && ended {
                if let Some(seq) = state.pending_paint_seq.borrow_mut().take() {
                    let _ = state.presented_tx.send(seq);
                }
            }
            LRESULT(0)
        }

        WM_MOUSEMOVE => {
            let (x, y) = to_host(hwnd, state, lparam);
            let _ = state
                .input_tx
                .send(WindowInput::PointerMove { id: state.id, x, y });
            LRESULT(0)
        }

        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let (x, y) = to_host(hwnd, state, lparam);
            let pressed = matches!(
                msg,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            );
            let button = match msg {
                WM_LBUTTONDOWN | WM_LBUTTONUP => MouseButton::Left,
                WM_RBUTTONDOWN | WM_RBUTTONUP => MouseButton::Right,
                WM_MBUTTONDOWN | WM_MBUTTONUP => MouseButton::Middle,
                // X-buttons say which one in the high word of wParam.
                _ => {
                    if ((wparam.0 >> 16) & 0xFFFF) == 0x0001 {
                        MouseButton::Back
                    } else {
                        MouseButton::Forward
                    }
                }
            };
            let _ = state.input_tx.send(WindowInput::PointerButton {
                id: state.id,
                x,
                y,
                button,
                pressed,
            });
            LRESULT(0)
        }

        WM_MOUSEWHEEL => {
            // wParam's high word is a signed multiple of WHEEL_DELTA (120);
            // the protocol counts whole notches, with the same sign convention.
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            let notches = (delta as i32) / 120;
            if notches != 0 {
                let _ = state.input_tx.send(WindowInput::PointerScroll {
                    id: state.id,
                    dx: 0,
                    dy: notches,
                });
            }
            LRESULT(0)
        }

        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            if let Some(key) = evdev_from_vk(wparam.0 as u16) {
                let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
                let _ = state.input_tx.send(WindowInput::Key {
                    id: state.id,
                    key,
                    pressed,
                });
            }
            LRESULT(0)
        }

        WM_CLOSE => {
            state.closed.store(true, Ordering::Relaxed);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }

        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Create the window and pump its message loop until told to stop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    meta: WindowMeta,
    selector: Option<mpsc::UnboundedSender<SourcePick>>,
    mut commands: mpsc::UnboundedReceiver<SurrogateCommand>,
    input_tx: mpsc::UnboundedSender<WindowInput>,
    presented_tx: mpsc::UnboundedSender<u32>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
    closed: Arc<AtomicBool>,
    wants_keyframe: Arc<AtomicBool>,
) {
    unsafe {
        // Declared before any window exists, because Windows fixes a process's
        // awareness at that moment. Left unaware, this process is told the
        // desktop is 96 DPI and the compositor bitmap-stretches the whole
        // window up to the monitor's real scale — so on a 125% or 150% display
        // the host's pixels were resampled twice before reaching the screen.
        // Failure only means someone (a manifest, an earlier surrogate) already
        // set it, which is fine.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(e) => {
                let _ = ready_tx.send(Err(format!("module handle: {e}")));
                return;
            }
        };

        let class_name = w!("CursioSurrogate");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        // Re-registering the same class returns 0 with ERROR_CLASS_ALREADY_EXISTS,
        // which is fine: a second transfer reuses the first registration.
        RegisterClassW(&wc);

        let title: Vec<u16> = meta
            .title
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            // Let Windows place it; the design's edge-drag trigger will decide
            // position later.
            i32::MIN, // CW_USEDEFAULT
            i32::MIN,
            meta.width as i32,
            meta.height as i32,
            None,
            None,
            Some(instance),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("create window: {e}")));
                return;
            }
        };

        let state = Box::new(WindowState {
            id: meta.id,
            fb: RefCell::new(Framebuffer::new(meta.width, meta.height)),
            input_tx,
            presented_tx,
            pending_paint_seq: RefCell::new(None),
            closed: closed.clone(),
            view: selector.map(|picks| ViewState {
                picks,
                sources: RefCell::new(Vec::new()),
                hint: RefCell::new(Some(VIEW_HINT.to_string())),
                tip: meta.title.clone(),
                taskbar_created: unseen::taskbar_created(),
            }),
        });
        let is_view = state.view.is_some();
        let state_ptr = Box::into_raw(state);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        // Only now can the frame be measured at this window's DPI, so the size
        // passed to `CreateWindowExW` is corrected here rather than guessed
        // above.
        size_for(hwnd, is_view, (0, 0), (meta.width, meta.height));

        if is_view {
            unseen::hide(hwnd);
            unseen::track(hwnd);
            unseen::add_icon(hwnd, &meta.title);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            unseen::remove_from_taskbar(hwnd);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = ready_tx.send(Ok(()));

        let mut decoder: Option<VideoDecoder> = None;
        let mut msg = MSG::default();

        'outer: loop {
            // Drain the message queue without blocking, so commands are still
            // serviced on an idle window.
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == windows::Win32::UI::WindowsAndMessaging::WM_QUIT {
                    break 'outer;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let mut painted = false;
            let mut presented_seq: Option<u32> = None;
            // Bounded per tick: a burst of frames must not starve the painting
            // that makes them visible.
            for _ in 0..16 {
                match commands.try_recv() {
                    Ok(SurrogateCommand::Frame { seq, dirty, raw }) => {
                        let state = &*state_ptr;
                        if let Err(e) = state.fb.borrow_mut().blit(dirty, &raw) {
                            tracing::warn!(error = %e, "dropping malformed tile");
                            continue;
                        }
                        painted = true;
                        presented_seq =
                            Some(presented_seq.map_or(seq, |previous| previous.max(seq)));
                    }
                    Ok(SurrogateCommand::VideoPacket { seq, packet }) => {
                        if decoder.is_none() {
                            match VideoDecoder::open() {
                                Ok(d) => decoder = Some(d),
                                Err(e) => {
                                    tracing::error!(error = %e, "no H.264 decoder; video frames dropped");
                                    continue;
                                }
                            }
                        }
                        let Some(dec) = decoder.as_mut() else {
                            continue;
                        };
                        match dec.decode(&packet) {
                            Ok(pictures) => {
                                let state = &*state_ptr;
                                for pic in pictures {
                                    let mut fb = state.fb.borrow_mut();
                                    if fb.width != pic.width || fb.height != pic.height {
                                        let old = (fb.width, fb.height);
                                        fb.resize(pic.width, pic.height);
                                        // The encoder changed resolution
                                        // mid-stream — a phone turned, or a
                                        // quality change; follow it, or every
                                        // frame from here is squeezed into the
                                        // old shape.
                                        size_for(hwnd, is_view, old, (pic.width, pic.height));
                                    }
                                    let full = Rect {
                                        x: 0,
                                        y: 0,
                                        width: pic.width,
                                        height: pic.height,
                                    };
                                    if let Err(e) = fb.blit(full, &pic.pixels) {
                                        tracing::warn!(error = %e, "dropping decoded frame");
                                        continue;
                                    }
                                    painted = true;
                                    presented_seq = Some(
                                        presented_seq.map_or(seq, |previous| previous.max(seq)),
                                    );
                                }
                            }
                            Err(e) => {
                                // Its reference pictures are now suspect, and
                                // every later P-frame would build on them;
                                // start again from the host's next keyframe.
                                tracing::warn!(error = %e, "video decode failed; restarting the decoder");
                                decoder = None;
                            }
                        }
                        if !decoder.as_ref().is_some_and(VideoDecoder::synced) {
                            wants_keyframe.store(true, Ordering::Relaxed);
                        }
                    }
                    Ok(SurrogateCommand::Resize { width, height }) => {
                        let state = &*state_ptr;
                        let old = {
                            let mut fb = state.fb.borrow_mut();
                            let old = (fb.width, fb.height);
                            fb.resize(width, height);
                            old
                        };
                        // The host's window was resized, so this one follows it.
                        size_for(hwnd, is_view, old, (width, height));
                        painted = true;
                    }
                    Ok(SurrogateCommand::Grow { width, height }) => {
                        let state = &*state_ptr;
                        let old = {
                            let mut fb = state.fb.borrow_mut();
                            let old = (fb.width, fb.height);
                            fb.grow_to(width, height);
                            old
                        };
                        size_for(hwnd, is_view, old, (width, height));
                        painted = true;
                    }
                    Ok(SurrogateCommand::Restart { title }) => {
                        // A new source is a new H.264 stream, possibly at
                        // another size; the old decoder's reference frames
                        // mean nothing to it.
                        decoder = None;
                        let state = &*state_ptr;
                        let mut fb = state.fb.borrow_mut();
                        let (w, h) = (fb.width, fb.height);
                        fb.resize(w, h);
                        if let Some(view) = state.view.as_ref() {
                            *view.hint.borrow_mut() = Some(if title.is_empty() {
                                VIEW_HINT.to_string()
                            } else {
                                format!("Starting {title}…")
                            });
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    Ok(SurrogateCommand::Sources(list)) => {
                        let state = &*state_ptr;
                        if let Some(view) = state.view.as_ref() {
                            *view.sources.borrow_mut() = list;
                        }
                    }
                    Ok(SurrogateCommand::Close(reason)) => {
                        tracing::info!(?reason, id = meta.id, "surrogate closing");
                        break 'outer;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => break 'outer,
                }
            }

            if painted {
                // One repaint per tick, not per tile: several tiles of the same
                // visual update coalesce into a single present.
                if let Some(view) = (*state_ptr).view.as_ref() {
                    *view.hint.borrow_mut() = None;
                }
                if let Some(seq) = presented_seq {
                    let state = &*state_ptr;
                    let mut pending = state.pending_paint_seq.borrow_mut();
                    *pending = Some(pending.map_or(seq, |previous| previous.max(seq)));
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
            } else {
                // Nothing to do — yield rather than spin the CPU on an idle
                // window.
                std::thread::sleep(std::time::Duration::from_millis(4));
            }
        }

        closed.store(true, Ordering::Relaxed);
        if is_view {
            unseen::untrack(hwnd);
            unseen::remove_icon(hwnd);
        }
        let _ = DestroyWindow(hwnd);
        // Reclaim the state only after the window is gone, so no late message
        // can reach a freed pointer.
        drop(Box::from_raw(state_ptr));
        tracing::info!(id = meta.id, "surrogate window thread finished");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vk_to_evdev_round_trips_with_the_host_table() {
        // These must be the inverse of `windows_input::vk_from_evdev`, or a key
        // typed on the surrogate would arrive as a different key on the host.
        assert_eq!(evdev_from_vk(0x41), Some(30)); // 'A' -> KEY_A
        assert_eq!(evdev_from_vk(0x5A), Some(44)); // 'Z' -> KEY_Z
        assert_eq!(evdev_from_vk(0x31), Some(2)); // '1' -> KEY_1
        assert_eq!(evdev_from_vk(0x30), Some(11)); // '0' -> KEY_0
        assert_eq!(evdev_from_vk(0x70), Some(59)); // F1
        assert_eq!(evdev_from_vk(0x7B), Some(88)); // F12
    }

    #[test]
    fn the_view_menu_lists_screens_before_windows_and_ends_with_show_nothing() {
        use cursio_protocol::SourceKind;
        let source = |id, kind, title: &str| ScreenSource {
            id,
            kind,
            title: title.into(),
            width: 0,
            height: 0,
            picked_on_host: false,
        };
        let entries = menu_entries(&[
            source(7, SourceKind::Window, "Firefox"),
            source(0x8000_0000, SourceKind::Monitor, "Screen 1"),
        ]);
        assert_eq!(
            entries,
            vec![
                MenuEntry::Item {
                    command: 2,
                    label: "Screen 1".into()
                },
                MenuEntry::Separator,
                MenuEntry::Item {
                    command: 1,
                    label: "Firefox".into()
                },
                MenuEntry::Separator,
                MenuEntry::Item {
                    command: STOP_COMMAND,
                    label: "Show nothing".into()
                },
            ]
        );
    }

    #[test]
    fn unmapped_keys_are_dropped() {
        assert_eq!(evdev_from_vk(0xFF), None);
    }
}
