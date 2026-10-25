//! Windows input capture via RawInput.
//!
//! The source side of input sharing on Windows: read the machine's real
//! mouse/keyboard as **relative** motion + buttons + keys and emit neutral
//! [`InputEvent`]s on a channel, symmetric with the Linux [`crate::capture`]
//! backend so `run_session` can forward them to the peer.
//!
//! RawInput delivers events as `WM_INPUT` window messages, so we spin up a
//! dedicated thread that owns a **message-only window** (`HWND_MESSAGE`),
//! registers for raw mouse+keyboard with `RIDEV_INPUTSINK` (so we receive input
//! even when unfocused), and pumps its message loop. The window procedure
//! translates each event and pushes it onto the capture channel.
//!
//! ## Avoiding the feedback loop
//!
//! Our own [`crate::win::WinInjector`] posts synthetic input with `SendInput`,
//! which RawInput reports back to us. Left unchecked that would loop (inject →
//! capture → inject …). We tag every injected event with a sentinel in its
//! `ExtraInformation`/`dwExtraInfo` field ([`INJECTED_TAG`]) and drop any raw
//! event carrying that tag here.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

use cursio_protocol::{Edge, InputEvent, MouseButton};
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    PostThreadMessageW, RegisterClassW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK,
    HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, RI_KEY_BREAK, RI_KEY_E0, RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP,
    RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP, RI_MOUSE_HWHEEL, RI_MOUSE_LEFT_BUTTON_DOWN,
    RI_MOUSE_LEFT_BUTTON_UP, RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP,
    RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP, RI_MOUSE_WHEEL, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_INPUT, WM_KEYDOWN, WM_MOUSEMOVE, WM_QUIT, WM_SYSKEYDOWN, WNDCLASSW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClipCursor, GetCursorInfo, GetCursorPos, GetSystemMetrics, SetCursorPos, CURSORINFO,
    CURSOR_SHOWING, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::edge_gate::EdgeGate;
use crate::win_keymap::scancode_to_evdev;
use crate::{win::INJECTED_TAG, CaptureHandle, InputError};

/// Custom thread message setting the capture mode. `wparam` is a
/// [`mode_to_code`] value. Drives hook install/remove, grab, and edge-watch.
/// `WM_APP` (0x8000) is the first value reserved for app-defined messages.
const WM_SET_MODE: u32 = 0x8000;

/// No session: hooks down, input entirely local.
const MODE_IDLE: usize = 0;
/// Holding the cursor: input stays local, but the mouse hook watches the peer
/// edge so a crossing can hand off.
const MODE_ACTIVE: usize = 1;
/// Driving the peer: local mouse and keyboard are swallowed and streamed.
const MODE_DORMANT: usize = 2;

/// Current capture mode as seen by the hook callbacks (`MODE_*` above). Mirrors
/// [`crate::InputMode`] as a plain int, because the hooks are context-free
/// `extern "system"` fns and can't carry state. **Both** hooks read this and only
/// this — see [`keyboard_hook`] for why that matters. Only written on the capture
/// thread, from [`apply_mode`].
static HOOK_MODE: AtomicUsize = AtomicUsize::new(MODE_IDLE);

/// The installed low-level hooks, as raw `HHOOK` pointer bits (0 = not
/// installed). Stored so the capture thread can uninstall them. `AtomicIsize`
/// holds the pointer value; only touched on the capture thread.
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);
static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    /// Sender for the capture window's thread. Set before the message loop
    /// starts so the window procedure (which runs on this same thread) can push
    /// translated events without threading a pointer through `WM_INPUT`.
    static SENDER: RefCell<Option<crate::EventSender>> = const { RefCell::new(None) };

    /// Sender for edge crossings, on the same capture thread as the hooks (the
    /// mouse hook runs on the thread that installed it). Set before the loop so
    /// the hook can report a jump-zone crossing.
    static EDGE_SENDER: RefCell<Option<mpsc::UnboundedSender<crate::EdgeCrossing>>> = const { RefCell::new(None) };

    /// Whether a pointer at the edge means a crossing; see [`EdgeGate`].
    /// Thread-local because the hook and [`apply_mode`] both run on the
    /// capture thread and nowhere else.
    static GATE: RefCell<EdgeGate> = RefCell::new(EdgeGate::default());
}

/// Signalled once by the capture thread when it's up: `Ok(thread_id)` when the
/// window and raw devices are ready (the id lets us `PostThreadMessageW` grab
/// toggles to it), `Err(msg)` if bring-up failed.
type Ready = std::sync::mpsc::Sender<Result<u32, String>>;

/// Start Windows RawInput capture on a dedicated thread, returning a
/// [`CaptureHandle`] whose channel yields neutral [`InputEvent`]s.
///
/// Symmetric with the Linux `spawn_capture`; the caller (session loop) doesn't
/// care which backend produced the events. Blocks until the capture thread has
/// either come up or failed to, so a setup error surfaces here rather than
/// silently producing no events.
///
/// The edge-crossing watch runs in the low-level mouse hook (kept installed
/// while Active, like Deskflow's jump-zone hook) and reads the edges armed by
/// [`crate::set_watched_edges`], so a cursor shoved off any of them fires an
/// [`EdgeCrossing`]; with none armed, nothing fires.
pub fn spawn_capture() -> Result<CaptureHandle, InputError> {
    let (tx, rx) = crate::event_channel();
    let (edges_tx, edges_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, String>>();

    let tx_for_thread = tx.clone();
    let edges_for_thread = edges_tx.clone();
    std::thread::Builder::new()
        .name("cursio-rawinput".into())
        .spawn(move || {
            // The hook reports the cursor in physical pixels whatever this
            // thread's awareness, but GetSystemMetrics answers an unaware
            // thread in scaled ones. On a laptop at 150% the right and bottom
            // edges then lay a third of the way inside the screen, and a
            // crossing's position along the edge came out clamped to 1.
            // SAFETY: affects only this thread; no preconditions.
            unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }
            SENDER.with(|s| *s.borrow_mut() = Some(tx_for_thread));
            EDGE_SENDER.with(|s| *s.borrow_mut() = Some(edges_for_thread));
            // `run_message_loop` signals `ready_tx` itself once registration
            // succeeds, then blocks pumping messages. If it returns an error it
            // never signalled, so report it here.
            if let Err(e) = run_message_loop(&ready_tx) {
                let _ = ready_tx.send(Err(e.clone()));
                tracing::warn!(error = %e, "rawinput capture thread failed");
            }
        })
        .map_err(InputError::Io)?;

    // Grab control: local input freeze via low-level hooks living on the capture
    // thread. `set_grabbed` drives this watch; a bridge task relays changes to
    // that thread as WM_SET_MODE messages (hooks must be (un)installed there).
    let (grab, mut grab_rx) = crate::GrabControl::new();

    let thread_id = match ready_rx.recv() {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => return Err(InputError::Unavailable(e)),
        Err(_) => {
            return Err(InputError::Unavailable(
                "capture thread ended before signalling readiness".into(),
            ))
        }
    };

    // Bridge: watch the mode, post each change to the capture thread as a
    // WM_SET_MODE. The hook install/remove AND the edge-watch both key off the
    // mode: Dormant grabs, Active watches the peer edge (hook installed but not
    // swallowing), Idle removes the hook entirely.
    tokio::spawn(async move {
        while grab_rx.changed().await.is_ok() {
            let mode = *grab_rx.borrow_and_update();
            // SAFETY: PostThreadMessageW just enqueues a message; thread_id is a
            // live thread we own for the session.
            unsafe {
                PostThreadMessageW(thread_id, WM_SET_MODE, mode_to_code(mode), 0);
            }
            if matches!(mode, crate::InputMode::Shutdown) {
                // Explicit teardown: release the hooks (the Idle above) and then
                // end the thread, so a later session starts from a clean slate
                // instead of stacking a second capture on this one.
                break;
            }
        }
        // Every GrabControl was dropped, so this capture is over. Tell the thread
        // to quit: its message loop exits and tears the hooks down. Without this
        // the thread lives on with hooks installed while nothing drains them —
        // which wedges local input, and a reconnecting `serve` would stack a
        // second set of hooks on top.
        // SAFETY: same as above; a stale thread id simply makes this a no-op.
        unsafe {
            PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
        }
        tracing::info!("capture control dropped; asked capture thread to quit");
    });

    Ok(CaptureHandle {
        events: rx,
        edges: edges_rx,
        devices: vec!["RawInput (mouse+keyboard)".into()],
        grab,
    })
}

/// Create the message-only window, register raw devices, signal `ready` on
/// success, and pump messages until the window is destroyed. Runs entirely on
/// the capture thread. Returns `Err` (without signalling) if bring-up fails.
fn run_message_loop(ready: &Ready) -> Result<(), String> {
    // SAFETY: standard Win32 window bring-up. Every pointer we pass is to a
    // local that outlives the call, and we check return values.
    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("CursioRawInputSink");

        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&wc) == 0 {
            // ERROR_CLASS_ALREADY_EXISTS (1410) is not a failure: the class was
            // registered by a previous session in this same process and never
            // unregistered (a process-global registration). On reconnect we hit
            // exactly this, and treating it as fatal killed Windows-side input
            // capture after every reconnect ("RegisterClassW failed (err 1410)").
            // The existing class is identical, so reusing it is correct.
            const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
            let err = last_error();
            if err != ERROR_CLASS_ALREADY_EXISTS {
                return Err(format!("RegisterClassW failed (err {err})"));
            }
        }

        let hwnd: HWND = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("Cursio RawInput").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err(format!("CreateWindowExW failed (err {})", last_error()));
        }

        // Register raw mouse (usage page 0x01, usage 0x02) and keyboard
        // (usage 0x06). INPUTSINK => receive input even when not focused,
        // which is what a background input-sharing daemon needs.
        let devices = [
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02, // mouse
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x06, // keyboard
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
        ];
        if RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            size_of::<RAWINPUTDEVICE>() as u32,
        ) == 0
        {
            return Err(format!(
                "RegisterRawInputDevices failed (err {})",
                last_error()
            ));
        }

        // Signal readiness (with our thread id, so grab toggles can be posted
        // here) now that capture is fully wired.
        let _ = ready.send(Ok(GetCurrentThreadId()));

        tracing::info!("rawinput capture window up; pumping messages");

        // Standard message loop. GetMessageW returns 0 on WM_QUIT, -1 on error.
        // Thread messages (posted via PostThreadMessageW, e.g. WM_SET_MODE) have
        // a null hwnd and don't go through DispatchMessageW, so handle those
        // inline; everything else goes to the window procedure.
        let mut msg: MSG = zeroed();
        loop {
            let got = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if got == 0 || got == -1 {
                break;
            }
            if msg.hwnd.is_null() && msg.message == WM_SET_MODE {
                apply_mode(msg.wParam);
                continue;
            }
            DispatchMessageW(&msg);
        }

        // Leaving: make sure we never exit in a mode that keeps hooks installed.
        apply_mode(mode_to_code(crate::InputMode::Idle));
    }
    Ok(())
}

/// Window procedure: translate `WM_INPUT` into neutral events, forward
/// everything else to the default handler.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT {
        handle_raw_input(lparam as HRAWINPUT);
        // Per docs, still call DefWindowProc for WM_INPUT cleanup.
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Read the `RAWINPUT` struct for a `WM_INPUT` and emit neutral events.
unsafe fn handle_raw_input(hrawinput: HRAWINPUT) {
    // First query the size, then read into a suitably-sized buffer.
    let mut size: u32 = 0;
    let header_size = size_of::<RAWINPUTHEADER>() as u32;
    if GetRawInputData(
        hrawinput,
        RID_INPUT,
        std::ptr::null_mut(),
        &mut size,
        header_size,
    ) != 0
    {
        // Non-zero on this "query size" call means failure.
        return;
    }
    if size == 0 {
        return;
    }

    // RAWINPUT is large enough for mouse/keyboard variants; use it as the
    // buffer directly (its size >= the reported `size` for these usages).
    let mut raw: RAWINPUT = zeroed();
    let mut buf_size = size_of::<RAWINPUT>() as u32;
    let read = GetRawInputData(
        hrawinput,
        RID_INPUT,
        &mut raw as *mut RAWINPUT as *mut c_void,
        &mut buf_size,
        header_size,
    );
    if read == u32::MAX || read == 0 {
        return;
    }

    match raw.header.dwType {
        t if t == RIM_TYPEMOUSE => emit_mouse(&raw),
        t if t == RIM_TYPEKEYBOARD => emit_keyboard(&raw),
        _ => {}
    }
}

/// Translate the mouse variant of a RawInput event and push resulting events.
unsafe fn emit_mouse(raw: &RAWINPUT) {
    let mouse = raw.data.mouse;

    // Drop our own injected events to avoid the inject→capture loop.
    if mouse.ulExtraInformation == INJECTED_TAG {
        return;
    }

    // Relative motion. RawInput mice normally report relative deltas; if a
    // device reports absolute (touchpads/tablets) we skip motion rather than
    // teleport, keeping the "relative only" guarantee.
    let flags = mouse.usFlags;
    let is_absolute = flags & MOUSE_MOVE_ABSOLUTE != 0;
    if !is_absolute && (mouse.lLastX != 0 || mouse.lLastY != 0) {
        push(InputEvent::PointerMotion {
            dx: mouse.lLastX,
            dy: mouse.lLastY,
        });
    }

    let button_flags = mouse.Anonymous.Anonymous.usButtonFlags as u32;
    for (flag, button, pressed) in [
        (RI_MOUSE_LEFT_BUTTON_DOWN, MouseButton::Left, true),
        (RI_MOUSE_LEFT_BUTTON_UP, MouseButton::Left, false),
        (RI_MOUSE_RIGHT_BUTTON_DOWN, MouseButton::Right, true),
        (RI_MOUSE_RIGHT_BUTTON_UP, MouseButton::Right, false),
        (RI_MOUSE_MIDDLE_BUTTON_DOWN, MouseButton::Middle, true),
        (RI_MOUSE_MIDDLE_BUTTON_UP, MouseButton::Middle, false),
        (RI_MOUSE_BUTTON_4_DOWN, MouseButton::Back, true),
        (RI_MOUSE_BUTTON_4_UP, MouseButton::Back, false),
        (RI_MOUSE_BUTTON_5_DOWN, MouseButton::Forward, true),
        (RI_MOUSE_BUTTON_5_UP, MouseButton::Forward, false),
    ] {
        if button_flags & flag != 0 {
            push(InputEvent::PointerButton { button, pressed });
        }
    }

    // Wheel: usButtonData carries a signed WHEEL_DELTA-scaled amount, which is
    // the neutral unit already. A precision touchpad sends a fraction of a
    // notch per report (15, 40), so dividing to whole notches here dropped
    // two-finger scrolling entirely.
    let wheel_amount = i32::from(mouse.Anonymous.Anonymous.usButtonData as i16);
    if wheel_amount != 0 {
        if button_flags & RI_MOUSE_WHEEL != 0 {
            push(InputEvent::PointerScroll {
                dx: 0,
                dy: wheel_amount,
            });
        }
        if button_flags & RI_MOUSE_HWHEEL != 0 {
            push(InputEvent::PointerScroll {
                dx: wheel_amount,
                dy: 0,
            });
        }
    }
}

/// Translate the keyboard variant of a RawInput event and push a `Key` event.
unsafe fn emit_keyboard(raw: &RAWINPUT) {
    let kb = raw.data.keyboard;

    if kb.ExtraInformation == INJECTED_TAG {
        return;
    }

    // Some keys (e.g. PrintScreen, Pause) arrive as fake/placeholder events
    // with MakeCode 0 or the "overrun" code; skip those rather than guess.
    if kb.MakeCode == 0 {
        return;
    }

    let e0 = kb.Flags & RI_KEY_E0 as u16 != 0;
    let pressed = kb.Flags & RI_KEY_BREAK as u16 == 0; // BREAK set => key up.

    match scancode_to_evdev(kb.MakeCode, e0) {
        Some(evdev) => push(InputEvent::Key {
            key: evdev,
            pressed,
        }),
        None => tracing::trace!(
            make = kb.MakeCode,
            e0,
            vkey = kb.VKey,
            "unmapped key; dropping"
        ),
    }
}

/// Push one neutral event onto the capture channel for this thread.
fn push(event: InputEvent) {
    if HOOK_MODE.load(Ordering::SeqCst) == MODE_IDLE {
        return;
    }
    SENDER.with(|s| {
        if let Some(tx) = s.borrow().as_ref() {
            let _ = tx.send(event);
        }
    });
}

/// Encode an [`crate::InputMode`] as the [`HOOK_MODE`] code: Idle=0, Active=1,
/// Dormant=2.
fn mode_to_code(mode: crate::InputMode) -> usize {
    match mode {
        // Shutdown maps to Idle for the hooks (everything released); the bridge
        // separately posts WM_QUIT to end the capture thread itself.
        crate::InputMode::Idle | crate::InputMode::Shutdown => MODE_IDLE,
        crate::InputMode::Active => MODE_ACTIVE,
        crate::InputMode::Dormant => MODE_DORMANT,
    }
}

/// Wrap a Rust string as a NUL-terminated UTF-16 buffer for the `*W` APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The OS last-error code, for logging failed Win32 calls.
fn last_error() -> u32 {
    // SAFETY: GetLastError has no preconditions.
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

// ---------------------------------------------------------------------------
// Local input grab via low-level hooks.
//
// Windows has no `EVIOCGRAB` equivalent. To freeze local input while this
// machine drives the peer, we install `WH_MOUSE_LL`/`WH_KEYBOARD_LL` hooks on
// the capture thread. While grabbed, the hook callbacks return non-zero without
// calling `CallNextHookEx`, which **swallows** the event so it never reaches the
// desktop — the local cursor freezes and nothing types locally. RawInput still
// sees the same input (it's a separate path) and streams it to the peer. This
// is exactly how Synergy/MWB do it.
//
// Hooks must be installed and serviced on a thread with a message loop, which is
// why this all lives on the capture thread and is toggled via WM_SET_MODE.
// ---------------------------------------------------------------------------

/// Apply a capture [`crate::InputMode`] (as a code from [`mode_to_code`]) on the
/// capture thread. Sets [`HOOK_MODE`] and (un)installs the hooks:
///
/// - **Dormant** (`2`) — grab: hooks installed, mouse/keyboard swallowed.
/// - **Active** (`1`) — watch: hooks installed but NOT swallowing; the mouse
///   hook only checks the jump zone to emit edge crossings. Hooks stay up so we
///   see the cursor reach the peer edge (Deskflow's jump-zone approach). With no
///   peer edge configured this is harmless (the hook just passes everything on).
/// - **Idle** (`0`) — hooks removed entirely.
fn apply_mode(code: usize) {
    let was = HOOK_MODE.swap(code, Ordering::SeqCst);

    // Taking control: the cursor arrives sitting ON the peer-facing edge (that's
    // where it crossed in), so the jump zone would catch it again immediately and
    // bounce control straight back. Move it inward, off the edge — the same thing
    // Deskflow's `avoidJumpZone()` does after a switch. Crossing again then means
    // actually travelling back to the edge, which is the intent we want.
    //
    // Belt and braces: the zone is treated as occupied until the cursor is
    // seen outside it, in case the nudge was clamped (edge of the desktop).
    //
    // Giving control AWAY: the cursor stays where it was — on the edge it just
    // crossed — so the zone is occupied for the same reason, and the last
    // position is dropped: while dormant the hook stops sampling, so the next
    // step measured from it would be meaningless. Same failure this guards
    // against as the Dormant check in wl_capture: a second, spurious crossing
    // after a handoff, which ends with BOTH sides dormant and input frozen
    // until Ctrl+Alt+G.
    if (code == MODE_ACTIVE) != (was == MODE_ACTIVE) {
        if code == MODE_ACTIVE && !place_at_entry() {
            nudge_off_edge();
        }
        GATE.with(|gate| gate.borrow_mut().ownership_changed());
    }
    // Hooks needed for Active (edge watch) or Dormant (grab); gone in Idle.
    let want_hooks = code == MODE_ACTIVE || code == MODE_DORMANT;
    let have_hooks = MOUSE_HOOK.load(Ordering::SeqCst) != 0;
    if want_hooks && !have_hooks {
        install_hooks();
    } else if !want_hooks && have_hooks {
        remove_hooks();
    }
}

/// Install both low-level hooks and mark the grab active.
fn install_hooks() {
    // SAFETY: standard hook install. The callbacks are valid `extern "system"`
    // fns; a null module handle is allowed for a hook proc in this process.
    unsafe {
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), std::ptr::null_mut(), 0);
        let keyboard =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), std::ptr::null_mut(), 0);
        if mouse.is_null() || keyboard.is_null() {
            tracing::warn!(
                err = last_error(),
                "failed to install input hooks; grab is partial"
            );
            // Clean up whichever succeeded so we don't half-grab.
            if !mouse.is_null() {
                UnhookWindowsHookEx(mouse);
            }
            if !keyboard.is_null() {
                UnhookWindowsHookEx(keyboard);
            }
            return;
        }
        MOUSE_HOOK.store(mouse as isize, Ordering::SeqCst);
        KEYBOARD_HOOK.store(keyboard as isize, Ordering::SeqCst);
    }
    tracing::info!("input hooks installed");
}

/// Remove both hooks.
fn remove_hooks() {
    for hook in [&MOUSE_HOOK, &KEYBOARD_HOOK] {
        let h = hook.swap(0, Ordering::SeqCst);
        if h != 0 {
            // SAFETY: `h` is an HHOOK we installed and haven't yet removed.
            unsafe {
                UnhookWindowsHookEx(h as HHOOK);
            }
        }
    }
    tracing::info!("input hooks removed");
}

/// Low-level mouse hook. Behaviour depends on the current [`crate::InputMode`]:
///
/// - **Dormant** (grabbed): swallow real (non-injected) mouse input so it
///   doesn't act locally. Injected events pass through.
/// - **Active**: don't swallow, but check whether the cursor has reached the
///   peer-facing edge's jump zone; if so, emit an [`EdgeCrossing`] so the session
///   hands off. This is the Windows counterpart of the Wayland portal barrier.
///
/// The jump-zone check must run for **injected** motion too: while Active the
/// cursor is being driven by the peer's input, replayed here through SendInput
/// (tagged [`INJECTED_TAG`]). Those injected moves are exactly what carry the
/// cursor to the edge, so skipping them (as the grab path does) would mean the
/// edge never triggers — the bug where crossing back never fired. `pt` is the
/// real current cursor position regardless of injection, so it's accurate.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        let injected =
            info.flags & LLMHF_INJECTED != 0 || info.dwExtraInfo == INJECTED_TAG as usize;
        match HOOK_MODE.load(Ordering::SeqCst) {
            // Dormant: swallow real input so it doesn't act locally; let injected
            // (our own / others') through.
            MODE_DORMANT if !injected => return 1,
            // Active: watch the peer edge for a crossing, including injected
            // motion (that's what moves the cursor while the peer drives us).
            //
            // MOVES ONLY. A click, a wheel tick or a button release carries the
            // cursor's current position too, and with the pointer already
            // resting in the 1px jump zone such an event looks exactly like a
            // fresh crossing: the position is in the zone and the step since the
            // last event is 0 (nothing moved), so it passes the flick filter and
            // hands the machine back mid-click. Symptom: a left click loses
            // control of the machine. Only a genuine move can mean the user
            // pushed into the edge, so only WM_MOUSEMOVE triggers a handoff.
            MODE_ACTIVE if wparam as u32 == WM_MOUSEMOVE => check_jump_zone(info.pt),
            _ => {}
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

/// Pass the cursor at `pt` (physical virtual-desktop coords) to the
/// [`EdgeGate`], and emit the [`EdgeCrossing`] it decides on, if any.
fn check_jump_zone(pt: POINT) {
    // Virtual-desktop bounds (spans all monitors), physical like `pt` because
    // the capture thread is per-monitor DPI aware. SAFETY: GetSystemMetrics
    // has no preconditions.
    let bounds = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let watched = crate::watched_edges();
    let spans = crate::crossing_spans();
    let Some(crossing) = GATE.with(|gate| {
        let mut gate = gate.borrow_mut();
        gate.spans = spans;
        gate.on_move((pt.x, pt.y), bounds, watched)
    }) else {
        return;
    };
    crate::note_crossed(crossing.edge);
    EDGE_SENDER.with(|s| {
        if let Some(tx) = s.borrow().as_ref() {
            let _ = tx.send(crate::EdgeCrossing {
                edge: crossing.edge,
                pos_norm: crossing.pos_norm,
            });
        }
    });
}

/// How far to pull the cursor back from the peer-facing edge when this machine
/// takes control, so the jump zone doesn't immediately re-trigger. Mirrors
/// Deskflow's jump-zone inset.
const EDGE_NUDGE_PX: i32 = 12;

/// Put the cursor where the peer's left it: on the edge it came in across, at
/// the same fraction of the way along, [`EDGE_NUDGE_PX`] inside. Returns
/// whether it did, so the caller falls back to the plain nudge otherwise.
///
/// `SetCursorPos` is the one way to do it a game does not see: it produces no
/// `WM_INPUT` and passes no low-level hook, so nothing reading raw input — and
/// not our own jump-zone watch — takes it for motion. A `SendInput` move of
/// the same distance is exactly what a game reads as a flick.
///
/// Left alone while something owns the pointer. A game that hid the cursor or
/// confined it with `ClipCursor` is steering by it, and has no free pointer
/// for a height to apply to; a held button is a drag that a jump would tear.
fn place_at_entry() -> bool {
    let Some(entry) = crate::take_entry() else {
        return false;
    };
    // Physical pixels, like the hook's, since this thread is per-monitor DPI
    // aware. SAFETY: GetSystemMetrics has no preconditions.
    let bounds = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if let Some(reason) = pointer_owned(bounds) {
        tracing::debug!(reason, "pointer left where it was on entry");
        return false;
    }
    let Some((x, y)) =
        crate::edge_gate::entry_point(entry.edge, entry.pos_norm, bounds, EDGE_NUDGE_PX)
    else {
        return false;
    };
    // SAFETY: SetCursorPos takes plain screen coordinates; the OS clamps them to
    // the virtual desktop.
    if unsafe { SetCursorPos(x, y) } == 0 {
        tracing::debug!(err = last_error(), "could not place the pointer on entry");
        return false;
    }
    tracing::debug!(
        edge = ?entry.edge,
        pos_norm = entry.pos_norm,
        x,
        y,
        "pointer placed where it left the peer"
    );
    true
}

/// Why the pointer belongs to someone else right now, if it does.
fn pointer_owned(bounds: crate::edge_gate::Bounds) -> Option<&'static str> {
    let mut info = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is ours, with cbSize set as the call requires.
    if unsafe { GetCursorInfo(&mut info) } != 0 && info.flags & CURSOR_SHOWING == 0 {
        return Some("cursor hidden");
    }
    let mut clip = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let (vx, vy, vw, vh) = bounds;
    // SAFETY: `clip` is ours; the call only writes it.
    if unsafe { GetClipCursor(&mut clip) } != 0
        && (clip.left > vx || clip.top > vy || clip.right < vx + vw || clip.bottom < vy + vh)
    {
        return Some("cursor confined");
    }
    // SAFETY: GetAsyncKeyState has no preconditions. The high bit is "down now".
    let held = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
        .iter()
        .any(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } as u16 & 0x8000 != 0);
    held.then_some("button held")
}

/// Move the cursor inward, away from the edge it last left by, by
/// [`EDGE_NUDGE_PX`]. No-op when no edge is known or the position can't be
/// read.
fn nudge_off_edge() {
    let Some(edge) = crate::last_crossed_edge() else {
        return;
    };
    let mut pt = POINT { x: 0, y: 0 };
    // SAFETY: GetCursorPos writes into our own POINT; failure leaves it unused.
    if unsafe { GetCursorPos(&mut pt) } == 0 {
        return;
    }
    let (dx, dy) = match edge {
        Edge::Left => (EDGE_NUDGE_PX, 0),
        Edge::Right => (-EDGE_NUDGE_PX, 0),
        Edge::Top => (0, EDGE_NUDGE_PX),
        Edge::Bottom => (0, -EDGE_NUDGE_PX),
    };
    // SAFETY: SetCursorPos takes plain screen coordinates; the OS clamps them to
    // the virtual desktop.
    unsafe {
        SetCursorPos(pt.x + dx, pt.y + dy);
    }
}

/// Low-level keyboard hook.
///
/// While grabbed, real key events are swallowed so they don't act locally —
/// **but a swallowed key never reaches RawInput**, so unlike the mouse we must
/// capture the key *here*: we translate it and push it onto the capture channel
/// ourselves, then swallow it. This is the source of forwarded keystrokes while
/// dormant. Injected events pass straight through (not captured, not swallowed).
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Reads HOOK_MODE, the SAME source of truth the mouse hook uses. They used to
    // key off two different statics, which could disagree while a capture was
    // being torn down and replaced (reconnect): the mouse stayed swallowed while
    // the keyboard went through, leaving a machine whose clicks did nothing but
    // whose typing worked. One value, one behaviour.
    if code >= 0 && HOOK_MODE.load(Ordering::SeqCst) == MODE_DORMANT {
        let info = &*(lparam as *const KBDLLHOOKSTRUCT);
        let injected =
            info.flags & LLKHF_INJECTED != 0 || info.dwExtraInfo == INJECTED_TAG as usize;
        if !injected {
            // wparam is the key message; *DOWN => pressed. The hook fires for
            // WM_KEYDOWN/WM_KEYUP and their SYS variants (Alt-combos).
            let msg = wparam as u32;
            let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let extended = info.flags & LLKHF_EXTENDED != 0;
            // scanCode is the set-1 make code, same pivot as RawInput's MakeCode.
            if let Some(evdev) = scancode_to_evdev(info.scanCode as u16, extended) {
                push(InputEvent::Key {
                    key: evdev,
                    pressed,
                });
            } else {
                tracing::trace!(scan = info.scanCode, extended, "hook: unmapped key");
            }
            // Swallow so it doesn't also act locally.
            return 1;
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}
