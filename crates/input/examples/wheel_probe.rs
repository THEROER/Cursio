//! Where a Windows machine's scrolling can be read from: RawInput, the
//! low-level mouse hook, or both — and with which flags.
//!
//! A precision touchpad's two-finger scroll is synthesised by the touchpad
//! stack rather than reported by a mouse, and whether it reaches `WM_INPUT` at
//! all, or only the hook (and marked injected), decides where capture has to
//! read it. Only the real hardware answers that.
//!
//! Run it in the user's desktop session (not over ssh, whose session has no
//! input desktop) and scroll with each device for the given seconds:
//!
//! ```sh
//! cargo run -p cursio-input --example wheel_probe -- 20
//! ```

#[cfg(target_os = "windows")]
fn main() {
    use std::mem::{size_of, zeroed};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
        RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW,
        RegisterClassW, SetWindowsHookExW, UnhookWindowsHookEx, HWND_MESSAGE, LLMHF_INJECTED, MSG,
        MSLLHOOKSTRUCT, PM_REMOVE, RI_MOUSE_HWHEEL, RI_MOUSE_WHEEL, WH_MOUSE_LL, WM_INPUT,
        WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WNDCLASSW,
    };

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 && matches!(wparam as u32, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
            // SAFETY: for WH_MOUSE_LL, lparam points at an MSLLHOOKSTRUCT.
            let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
            let delta = (info.mouseData >> 16) as i16;
            println!(
                "hook {} delta={delta} injected={} extra={:#x}",
                if wparam as u32 == WM_MOUSEWHEEL {
                    "wheel"
                } else {
                    "hwheel"
                },
                info.flags & LLMHF_INJECTED != 0,
                info.dwExtraInfo,
            );
        }
        // SAFETY: passing the hook chain on unchanged.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_INPUT {
            // SAFETY: a zeroed RAWINPUT is a valid buffer for GetRawInputData.
            let mut raw: RAWINPUT = unsafe { zeroed() };
            let mut size = size_of::<RAWINPUT>() as u32;
            // SAFETY: lparam is the HRAWINPUT of this WM_INPUT; the buffer is sized.
            let read = unsafe {
                GetRawInputData(
                    lparam as HRAWINPUT,
                    RID_INPUT,
                    (&mut raw as *mut RAWINPUT).cast(),
                    &mut size,
                    size_of::<RAWINPUTHEADER>() as u32,
                )
            };
            if read != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE {
                // SAFETY: dwType says the union holds a mouse report.
                let mouse = unsafe { raw.data.mouse };
                // SAFETY: the button fields are the active union member.
                let (flags, data) = unsafe {
                    (
                        u32::from(mouse.Anonymous.Anonymous.usButtonFlags),
                        mouse.Anonymous.Anonymous.usButtonData as i16,
                    )
                };
                if flags & (RI_MOUSE_WHEEL | RI_MOUSE_HWHEEL) != 0 {
                    println!(
                        "raw  {} delta={data} device={:?} extra={:#x}",
                        if flags & RI_MOUSE_WHEEL != 0 {
                            "wheel"
                        } else {
                            "hwheel"
                        },
                        raw.header.hDevice,
                        mouse.ulExtraInformation,
                    );
                }
            }
        }
        // SAFETY: default handling for everything else.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let class: Vec<u16> = "CursioWheelProbe\0".encode_utf16().collect();
    // SAFETY: plain Win32 setup on this thread, which also pumps its messages.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };
        RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32);
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(hook), instance, 0);
        println!("listening {seconds}s: scroll with the touchpad, then with a wheel");
        let end = Instant::now() + Duration::from_secs(seconds);
        let mut msg: MSG = zeroed();
        while Instant::now() < end {
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                DispatchMessageW(&msg);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        UnhookWindowsHookEx(hook);
    }
    println!("done");
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("wheel_probe reads Windows' RawInput and mouse hook; run it on Windows");
    std::process::exit(1);
}
