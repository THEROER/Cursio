//! Does `Windows.Graphics.Capture` keep delivering frames when the source
//! window is hidden?
//!
//! This settles the question the whole "drag the window away and it disappears
//! on the host" design rests on. Microsoft's guidance says a captured window
//! must stay "at least one pixel on screen and must be layered" or DWM stops
//! refreshing it — but guidance is not measurement, and two earlier attempts to
//! measure it from PowerShell were both invalid:
//!
//! 1. `PrintWindow` appeared to show every state as live — but it can itself
//!    provoke a repaint (`WM_PRINT`), so it would mask a frozen window.
//! 2. Reading a DWM thumbnail with `GetPixel` returned nothing at all: DWM
//!    composites the preview outside GDI, so the sampled pixels were just the
//!    sink window's own background.
//!
//! Only the real capture API answers it, so this drives that API directly and
//! counts frames — the number that actually matters, since a frozen window
//! produces no new frames.
//!
//! Usage (on the Windows machine):
//! ```text
//! wgc_probe.exe <window-title-substring>
//! ```
//! It finds a matching top-level window, captures it in each state, and reports
//! frames per second observed in each.
//!
//! `wgc_probe.exe --selftest` asks the remote-screen question instead — which
//! states of a window the user never sees still land in Discord's and OBS's
//! pickers and still record a picture; see `selftest.rs`.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("wgc_probe only runs on Windows — it tests Windows.Graphics.Capture.");
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
mod selftest;

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("--selftest") => return selftest::run(),
        Some("--inspect") => {
            let needle = std::env::args().nth(2).unwrap_or_default();
            let (hwnd, title) = windows_probe::find_window(&needle)
                .ok_or_else(|| format!("no visible window matching {needle:?}"))?;
            return selftest::inspect(hwnd, &title);
        }
        _ => {}
    }
    windows_probe::run()
}

#[cfg(target_os = "windows")]
mod windows_probe {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use windows::core::{Interface, HSTRING};
    use windows::Foundation::TypedEventHandler;
    use windows::Graphics::Capture::{
        Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
    };
    use windows::Graphics::DirectX::DirectXPixelFormat;
    // `BOOL` lives in `windows_core` in 0.61, not in `Win32::Foundation`.
    use windows::core::BOOL;
    use windows::Win32::Foundation::{FALSE, HMODULE, HWND, LPARAM, TRUE};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongW, GetWindowTextW, IsWindowVisible, SetLayeredWindowAttributes,
        SetWindowLongW, SetWindowPos, ShowWindow, GWL_EXSTYLE, LWA_ALPHA, SWP_NOSIZE, SWP_NOZORDER,
        SW_MINIMIZE, SW_RESTORE, WS_EX_LAYERED,
    };

    /// Collected while enumerating windows, to find one by title.
    struct Search {
        needle: String,
        found: Option<(HWND, String)>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len > 0 {
            let title = String::from_utf16_lossy(&buf[..len as usize]);
            if title.to_lowercase().contains(&search.needle) {
                search.found = Some((hwnd, title));
                return FALSE; // stop
            }
        }
        TRUE
    }

    pub(crate) fn find_window(needle: &str) -> Option<(HWND, String)> {
        let mut search = Search {
            needle: needle.to_lowercase(),
            found: None,
        };
        unsafe {
            let _ = EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut Search as isize));
        }
        search.found
    }

    /// Count frames the capture API delivers over `dur` for the given window.
    ///
    /// Frames — not "does it composite" — is the decisive measure: a window DWM
    /// has stopped refreshing simply stops producing them.
    fn count_frames(hwnd: HWND, dur: Duration) -> Result<u32, Box<dyn std::error::Error>> {
        unsafe {
            // A D3D device is required; capture hands back GPU textures.
            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                // `software` is an HMODULE, only used for a software rasteriser.
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
            let device = device.ok_or("no D3D11 device")?;
            let dxgi: IDXGIDevice = device.cast()?;
            let d3d = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?;
            let d3d: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice = d3d.cast()?;

            // GraphicsCaptureItem for a specific HWND comes via the interop
            // interface — the WinRT surface alone has no by-handle constructor.
            let interop: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd)?;
            let size = item.Size()?;

            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &d3d,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )?;
            let session: GraphicsCaptureSession = pool.CreateCaptureSession(&item)?;

            let frames = Arc::new(AtomicU32::new(0));
            let counter = frames.clone();
            pool.FrameArrived(&TypedEventHandler::new(
                move |pool: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
                    if let Some(pool) = pool.as_ref() {
                        // Must take the frame, or the pool stalls after 2.
                        if let Ok(frame) = pool.TryGetNextFrame() {
                            drop(frame);
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(())
                },
            ))?;

            session.StartCapture()?;
            std::thread::sleep(dur);
            session.Close()?;
            pool.Close()?;

            Ok(frames.load(Ordering::Relaxed))
        }
    }

    /// Make a window invisible the way the design proposes: layered, alpha 0,
    /// left where it is on screen.
    unsafe fn set_alpha(hwnd: HWND, alpha: u8) {
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as i32);
        let _ = SetLayeredWindowAttributes(
            hwnd,
            windows::Win32::Foundation::COLORREF(0),
            alpha,
            LWA_ALPHA,
        );
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let needle = std::env::args()
            .nth(1)
            .unwrap_or_else(|| "minecraft".into());
        let (hwnd, title) =
            find_window(&needle).ok_or_else(|| format!("no visible window matching {needle:?}"))?;
        println!("target: {title:?} (hwnd {:?})\n", hwnd.0);

        let window = Duration::from_secs(3);
        let fps = |n: u32| n as f64 / window.as_secs_f64();

        println!("{:<34} {:>8}  {:>9}", "STATE", "frames", "fps");
        println!("{}", "-".repeat(56));

        let base = count_frames(hwnd, window)?;
        println!(
            "{:<34} {:>8}  {:>9.1}",
            "1. normal, visible",
            base,
            fps(base)
        );

        unsafe { set_alpha(hwnd, 0) };
        std::thread::sleep(Duration::from_millis(500));
        let hidden = count_frames(hwnd, window)?;
        println!(
            "{:<34} {:>8}  {:>9.1}",
            "2. layered alpha=0 (invisible)",
            hidden,
            fps(hidden)
        );

        // Off-screen as well, still layered — the state the docs warn about.
        unsafe {
            let _ = SetWindowPos(hwnd, None, -4000, -4000, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
        }
        std::thread::sleep(Duration::from_millis(500));
        let offscreen = count_frames(hwnd, window)?;
        println!(
            "{:<34} {:>8}  {:>9.1}",
            "3. layered + fully offscreen",
            offscreen,
            fps(offscreen)
        );

        // Restore position and opacity before the minimise test.
        unsafe {
            let _ = SetWindowPos(hwnd, None, 200, 200, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
            set_alpha(hwnd, 255);
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
        std::thread::sleep(Duration::from_millis(800));
        let minimised = count_frames(hwnd, window)?;
        println!(
            "{:<34} {:>8}  {:>9.1}",
            "4. minimised",
            minimised,
            fps(minimised)
        );

        // Leave the window as we found it.
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            set_alpha(hwnd, 255);
        }

        println!("\nwindow restored (visible, opaque, on screen)");
        println!(
            "\nverdict: hiding via layered alpha=0 is {}",
            if hidden > 0 {
                "VIABLE — frames keep arriving while invisible"
            } else {
                "NOT viable — capture froze when hidden"
            }
        );
        let _ = HSTRING::new();
        Ok(())
    }
}
