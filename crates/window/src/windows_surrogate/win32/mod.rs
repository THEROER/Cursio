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

use crate::SourcePick;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetWindowLongPtrW, PeekMessageW, PostQuitMessage, RegisterClassW, SetWindowLongPtrW,
    ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, MA_NOACTIVATE, MSG,
    PM_REMOVE, SIZE_MINIMIZED, SW_SHOW, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SIZE, WM_SIZING, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use crate::framebuffer::Framebuffer;
use crate::video_decoder_win::VideoDecoder;

use super::{unseen, SurrogateCommand, Win32Command};
use crate::windows_surrogate::win32::geometry::keep_shape;
use crate::windows_surrogate::win32::geometry::size_for;
use crate::windows_surrogate::win32::geometry::to_host;
use crate::windows_surrogate::win32::keys::evdev_from_vk;
use crate::windows_surrogate::win32::menu::show_source_menu;
use crate::windows_surrogate::win32::menu::VIEW_HINT;

mod geometry;
mod keys;
mod menu;

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
                    Ok(SurrogateCommand::Platform(Win32Command::Restart { title })) => {
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
                    Ok(SurrogateCommand::Platform(Win32Command::Sources(list))) => {
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
