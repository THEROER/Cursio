//! `wgc_probe --selftest`: can a window nobody sees still be picked and
//! recorded by screen-sharing software?
//!
//! A remote-screen source on Windows has to be a top-level window, because a
//! window is what Discord and OBS list; the question is which states of that
//! window keep it (a) in their pickers and (b) recordable, while the user never
//! sees it. This probe opens its own window painting a known colour, puts it
//! through each candidate state, and reports for every state:
//!
//! - whether Chromium/WebRTC's window list (Discord's web client, browser
//!   calls) and OBS's window list would include it — their filters, re-stated
//!   from `window_capture_utils.cc` and `window-helpers.c`;
//! - how many frames `Windows.Graphics.Capture` delivered, and the colour at
//!   the centre of the last one (black or transparent means "recorded nothing");
//! - the centre colour `PrintWindow(PW_RENDERFULLCONTENT)` reads, which is what
//!   GDI-era capturers fall back to.
//!
//! Its own window rather than someone else's because the answer depends on
//! the window's styles, which only the owner may change freely.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{w, Interface, BOOL};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{COLORREF, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, GetDC, GetPixel, InvalidateRect, ReleaseDC, SelectObject,
    PAINTSTRUCT,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::*;

const MAGENTA: COLORREF = COLORREF(0x00FF_00FF);

static TICK: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            TICK.fetch_add(1, Ordering::Relaxed);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let brush = CreateSolidBrush(MAGENTA);
            FillRect(hdc, &rc, brush);
            let _ = DeleteObject(brush.into());
            // A moving white bar near the top, so every tick is a new frame
            // while the centre pixel stays magenta.
            let x = (TICK.load(Ordering::Relaxed) * 8) as i32 % rc.right.max(1);
            let bar = RECT {
                left: x,
                top: 0,
                right: x + 20,
                bottom: 20,
            };
            let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
            FillRect(hdc, &bar, white);
            let _ = DeleteObject(white.into());
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Chromium/WebRTC's `GetWindowListHandler` plus `IsWindowVisibleOnCurrentDesktop`,
/// with the WGC capturer's `WS_EX_TOOLWINDOW` style filter.
unsafe fn chromium_lists(hwnd: HWND) -> bool {
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return false;
    }
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
    if !owner.is_invalid() && ex & WS_EX_APPWINDOW.0 == 0 {
        return false;
    }
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    if GetWindowTextLengthW(hwnd) == 0 {
        return false;
    }
    !cloaked(hwnd)
}

/// OBS' `check_window_valid` in `EXCLUDE_MINIMIZED` mode.
unsafe fn obs_lists(hwnd: HWND) -> bool {
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || cloaked(hwnd) {
        return false;
    }
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    ex & WS_EX_TOOLWINDOW.0 == 0 && style & WS_CHILD.0 == 0 && rc.bottom != 0 && rc.right != 0
}

unsafe fn cloaked(hwnd: HWND) -> bool {
    let mut value = 0u32;
    DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut value as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
        && value != 0
}

/// Centre pixel through `PrintWindow(PW_RENDERFULLCONTENT)`, as `0xRRGGBB`.
unsafe fn print_window_centre(hwnd: HWND) -> u32 {
    let mut rc = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rc);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, w, h);
    let old = SelectObject(mem, bmp.into());
    let _ = PrintWindow(hwnd, mem, PRINT_WINDOW_FLAGS(2));
    let px = GetPixel(mem, w / 2, h / 2).0;
    SelectObject(mem, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    // COLORREF is 0x00BBGGRR.
    ((px & 0xFF) << 16) | (px & 0xFF00) | ((px >> 16) & 0xFF)
}

/// Frames delivered in `dur`, and the centre pixel of the last one (`0xAARRGGBB`).
unsafe fn wgc_capture(hwnd: HWND, dur: Duration) -> Result<(u32, u32), String> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
    let device = device.ok_or("no device")?;
    let context = context.ok_or("no context")?;
    let dxgi: IDXGIDevice = device.cast().map_err(|e| e.to_string())?;
    let d3d = CreateDirect3D11DeviceFromDXGIDevice(&dxgi).map_err(|e| e.to_string())?;
    let d3d: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice =
        d3d.cast().map_err(|e| e.to_string())?;
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| e.to_string())?;
    let item: GraphicsCaptureItem = interop
        .CreateForWindow(hwnd)
        .map_err(|e| format!("CreateForWindow: {e}"))?;
    let size = item.Size().map_err(|e| e.to_string())?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &d3d,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|e| e.to_string())?;
    let session: GraphicsCaptureSession = pool
        .CreateCaptureSession(&item)
        .map_err(|e| e.to_string())?;
    let _ = session.SetIsBorderRequired(false);

    let frames = Arc::new(AtomicU32::new(0));
    let centre = Arc::new(Mutex::new(0u32));
    let (cb_frames, cb_centre) = (frames.clone(), centre.clone());
    pool.FrameArrived(&TypedEventHandler::new(
        move |pool: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
            let Some(pool) = pool.as_ref() else {
                return Ok(());
            };
            let Ok(frame) = pool.TryGetNextFrame() else {
                return Ok(());
            };
            cb_frames.fetch_add(1, Ordering::Relaxed);
            if let Some(px) = read_centre(&device, &context, &frame) {
                *cb_centre.lock().unwrap() = px;
            }
            Ok(())
        },
    ))
    .map_err(|e| e.to_string())?;
    session.StartCapture().map_err(|e| e.to_string())?;
    std::thread::sleep(dur);
    let _ = session.Close();
    let _ = pool.Close();
    let px = *centre.lock().unwrap();
    Ok((frames.load(Ordering::Relaxed), px))
}

fn read_centre(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
) -> Option<u32> {
    unsafe {
        let access = frame
            .Surface()
            .ok()?
            .cast::<IDirect3DDxgiInterfaceAccess>()
            .ok()?;
        let texture = access.GetInterface::<ID3D11Texture2D>().ok()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .ok()?;
        let staging = staging?;
        context.CopyResource(&staging, &texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .ok()?;
        let row =
            (mapped.pData as *const u8).add((desc.Height / 2) as usize * mapped.RowPitch as usize);
        let p = row.add((desc.Width / 2) as usize * 4) as *const u32;
        let px = p.read_unaligned();
        context.Unmap(&staging, 0);
        Some(px)
    }
}

/// One state to put the probe window in.
struct State {
    name: &'static str,
    apply: fn(HWND),
}

unsafe fn reset(hwnd: HWND) {
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let _ = SetWindowLongW(
        hwnd,
        GWL_EXSTYLE,
        (ex & !(WS_EX_LAYERED.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0)) as i32,
    );
    let _ = SetWindowPos(
        hwnd,
        None,
        100,
        100,
        0,
        0,
        SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
    );
}

unsafe fn off_screen(hwnd: HWND) {
    let right = GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let bottom = GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN);
    let _ = SetWindowPos(
        hwnd,
        None,
        right + 200,
        bottom + 200,
        0,
        0,
        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

unsafe fn layered(hwnd: HWND, alpha: u8) {
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, (ex | WS_EX_LAYERED.0) as i32);
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
}

fn states() -> Vec<State> {
    vec![
        State {
            name: "on screen (control)",
            apply: |_| {},
        },
        State {
            name: "off the virtual desktop",
            apply: |h| unsafe { off_screen(h) },
        },
        State {
            name: "off-desktop + NOACTIVATE",
            apply: |h| unsafe {
                let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
                let _ = SetWindowLongW(h, GWL_EXSTYLE, (ex | WS_EX_NOACTIVATE.0) as i32);
                off_screen(h);
            },
        },
        State {
            name: "on screen, layered alpha 0",
            apply: |h| unsafe { layered(h, 0) },
        },
        State {
            name: "on screen, layered alpha 1",
            apply: |h| unsafe { layered(h, 1) },
        },
        State {
            name: "off-desktop, layered alpha 0",
            apply: |h| unsafe {
                layered(h, 0);
                off_screen(h);
            },
        },
        State {
            name: "off-desktop, alpha 0, NOACT",
            apply: |h| unsafe {
                let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
                let _ = SetWindowLongW(h, GWL_EXSTYLE, (ex | WS_EX_NOACTIVATE.0) as i32);
                layered(h, 0);
                off_screen(h);
            },
        },
        State {
            name: "tool window (control)",
            apply: |h| unsafe {
                let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
                let _ = SetWindowLongW(h, GWL_EXSTYLE, (ex | WS_EX_TOOLWINDOW.0) as i32);
            },
        },
    ]
}

/// The window handle, as an integer so it may cross to the test thread.
static TARGET: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// `--inspect <title>`: the same questions about someone else's window,
/// left exactly as it is — for checking Cursio's own remote-screen view.
pub fn inspect(hwnd: HWND, title: &str) -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        println!(
            "{title:?} at ({}, {}) {}x{}",
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top
        );
        println!(
            "layered={} noactivate={} toolwindow={} cloaked={}",
            ex & WS_EX_LAYERED.0 != 0,
            ex & WS_EX_NOACTIVATE.0 != 0,
            ex & WS_EX_TOOLWINDOW.0 != 0,
            cloaked(hwnd)
        );
        println!("chromium lists it: {}", chromium_lists(hwnd));
        println!("obs lists it: {}", obs_lists(hwnd));
        let (frames, px) = wgc_capture(hwnd, Duration::from_secs(2))?;
        println!("wgc: {frames} frames in 2 s, centre pixel {px:#010x}");
        println!("printwindow centre: {:#08x}", print_window_centre(hwnd));
    }
    Ok(())
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("CursioProbeTarget"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("CursioProbeTarget"),
            w!("Cursio probe target"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            640,
            360,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        SetTimer(Some(hwnd), 1, 33, None);
        TARGET.store(hwnd.0 as usize as u32, Ordering::Relaxed);

        std::thread::spawn(|| {
            let hwnd = HWND(TARGET.load(Ordering::Relaxed) as usize as *mut _);
            std::thread::sleep(Duration::from_millis(800));
            println!(
                "{:<30} {:>8} {:>5} {:>7} {:>10} {:>9}",
                "STATE", "chromium", "obs", "frames", "wgc centre", "printwin"
            );
            for state in states() {
                reset(hwnd);
                (state.apply)(hwnd);
                std::thread::sleep(Duration::from_millis(500));
                let (chromium, obs) = (chromium_lists(hwnd), obs_lists(hwnd));
                let wgc = wgc_capture(hwnd, Duration::from_secs(2));
                let print = print_window_centre(hwnd);
                match wgc {
                    Ok((frames, px)) => println!(
                        "{:<30} {:>8} {:>5} {:>7} {:>#10x} {:>#9x}",
                        state.name, chromium, obs, frames, px, print
                    ),
                    Err(e) => println!(
                        "{:<30} {:>8} {:>5} capture failed: {e}",
                        state.name, chromium, obs
                    ),
                }
            }
            reset(hwnd);
            println!("\nexpect centre 0xffff00ff (magenta, opaque) where the picture is recorded");
            DONE.store(true, Ordering::Relaxed);
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _: BOOL = BOOL(DONE.load(Ordering::Relaxed) as i32);
    }
    Ok(())
}
