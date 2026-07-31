//! The window a mirrored iPhone screen appears in on Windows.
//!
//! `d3d11videosink` opens a window of its own when nothing gives it one, and
//! that window is titled "Direct3D11 renderer" — the sink exposes no `title`
//! property, so the name cannot be changed by configuring it. Handing the sink
//! a window we created is the only way to put Cursio's name on the thing the
//! user actually sees, in the taskbar and in the volume mixer once mirrored
//! audio arrives.
//!
//! ## Threading
//!
//! A Win32 window belongs to the thread that created it and its message loop
//! cannot be driven from anywhere else, so the window lives on a dedicated
//! thread — the same arrangement as `cursio-window`'s surrogate. Only the
//! numeric handle crosses back, which is what `GstVideoOverlay` wants anyway.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    PostQuitMessage, RegisterClassW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, MSG,
    SW_SHOW, WINDOW_EX_STYLE, WM_DESTROY, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use crate::AirPlayError;

/// What the user sees in the title bar, the taskbar, and Alt-Tab.
pub(super) const MIRROR_WINDOW_TITLE: &str = "Cursio (iPhone)";

/// A window owned by Cursio, for GStreamer to render into.
pub(super) struct MirrorWindow {
    hwnd: usize,
    closed: Arc<AtomicBool>,
}

impl MirrorWindow {
    /// Create the window and wait until it exists.
    ///
    /// `width`/`height` are the size of the picture, not of the window: the
    /// frame and title bar are added on top, so a 1920x1080 stream really does
    /// arrive as 1920x1080 pixels instead of being scaled down into what is
    /// left after the borders.
    ///
    /// Blocking until the handle is real matters: the sink must be given a
    /// window before the pipeline starts, or it opens its own and the name is
    /// wrong again.
    pub(super) fn open(width: u32, height: u32) -> Result<Self, AirPlayError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<usize, String>>();
        let closed = Arc::new(AtomicBool::new(false));
        let thread_closed = closed.clone();

        std::thread::Builder::new()
            .name("cursio-airplay-window".into())
            .spawn(move || run(width, height, ready_tx, thread_closed))
            .map_err(|error| {
                AirPlayError::Video(format!(
                    "could not start the AirPlay window thread: {error}"
                ))
            })?;

        match ready_rx.recv() {
            Ok(Ok(hwnd)) => Ok(Self { hwnd, closed }),
            Ok(Err(error)) => Err(AirPlayError::Video(error)),
            Err(_) => Err(AirPlayError::Video(
                "the AirPlay window thread ended before it reported a window".into(),
            )),
        }
    }

    /// The handle to hand `GstVideoOverlay`.
    pub(super) fn handle(&self) -> usize {
        self.hwnd
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

fn run(
    width: u32,
    height: u32,
    ready_tx: std::sync::mpsc::Sender<Result<usize, String>>,
    closed: Arc<AtomicBool>,
) {
    unsafe {
        let instance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(handle) => handle.into(),
            Err(error) => {
                let _ = ready_tx.send(Err(format!("module handle: {error}")));
                return;
            }
        };

        let class_name = w!("CursioAirPlayScreen");
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        // Re-registering returns 0 with ERROR_CLASS_ALREADY_EXISTS, which is
        // fine: a second mirroring session reuses the first registration.
        RegisterClassW(&class);

        let title: Vec<u16> = MIRROR_WINDOW_TITLE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let mut frame = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        // Failure only means the frame is not accounted for, which costs a
        // slightly small picture — not a reason to open no window at all.
        let _ = AdjustWindowRect(&mut frame, WS_OVERLAPPEDWINDOW, false);

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            i32::MIN, // CW_USEDEFAULT
            i32::MIN,
            frame.right - frame.left,
            frame.bottom - frame.top,
            None,
            None,
            Some(instance),
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("create window: {error}")));
                return;
            }
        };

        let _ = ShowWindow(hwnd, SW_SHOW);
        if ready_tx.send(Ok(hwnd.0 as usize)).is_err() {
            return;
        }

        // Nothing here paints: the sink renders into this window, so the loop
        // exists only to keep it alive and responsive.
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        closed.store(true, Ordering::Relaxed);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
