//! Replaying the viewer's input onto a real window on Windows.
//!
//! ## Why `PostMessage` and not `SendInput`
//!
//! `SendInput` synthesises input at the *system* level: it moves the real
//! cursor and delivers to whatever window is under it. That is wrong here twice
//! over — the target window is deliberately **invisible** (see
//! [`hide`](crate::windows_capture::HiddenWindow)), so there is nothing to point
//! at, and hijacking the host's real cursor would fight the user sitting at that
//! machine.
//!
//! `PostMessage` addresses a window directly, which is exactly the semantics we
//! need: the transferred window receives input regardless of what is in front of
//! it or where the host's pointer is. Measured behaviour (Windows 11 build
//! 26200): messages reach hidden, background, and offscreen windows — but
//! **not minimised** ones, which is one more reason hiding never minimises.
//!
//! The trade-off is honest: `PostMessage` drives the window's message loop, so
//! applications that read input another way — DirectInput, raw input, most
//! full-screen games — will ignore it. Those need `SendInput` with the window
//! focused, which is a different mode this doesn't attempt.

use cursio_protocol::{MouseButton, WindowInput};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, WHEEL_DELTA, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
};

/// Pack window-local coordinates the way mouse messages expect: low word x,
/// high word y.
fn coords(x: u32, y: u32) -> LPARAM {
    LPARAM(((y as isize & 0xFFFF) << 16) | (x as isize & 0xFFFF))
}

/// The `wParam` X-button code for back/forward, in its high word.
const XBUTTON1: usize = 0x0001; // "back"
const XBUTTON2: usize = 0x0002; // "forward"

/// Replay one event onto `hwnd`.
pub fn replay(hwnd: HWND, input: WindowInput) -> Result<(), String> {
    unsafe {
        match input {
            WindowInput::PointerMove { x, y, .. } => {
                PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), coords(x, y))
                    .map_err(|e| format!("WM_MOUSEMOVE: {e}"))
            }

            WindowInput::PointerButton {
                x,
                y,
                button,
                pressed,
                ..
            } => {
                // A move first, so the click lands where the viewer's pointer
                // is: the window tracks position from mouse messages, and
                // without this it would use wherever the pointer was last.
                let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), coords(x, y));

                let (msg, wparam) = match (button, pressed) {
                    (MouseButton::Left, true) => (WM_LBUTTONDOWN, WPARAM(0x0001)),
                    (MouseButton::Left, false) => (WM_LBUTTONUP, WPARAM(0)),
                    (MouseButton::Right, true) => (WM_RBUTTONDOWN, WPARAM(0x0002)),
                    (MouseButton::Right, false) => (WM_RBUTTONUP, WPARAM(0)),
                    (MouseButton::Middle, true) => (WM_MBUTTONDOWN, WPARAM(0x0010)),
                    (MouseButton::Middle, false) => (WM_MBUTTONUP, WPARAM(0)),
                    // X-buttons carry which one in the high word of wParam.
                    (MouseButton::Back, true) => (WM_XBUTTONDOWN, WPARAM(XBUTTON1 << 16)),
                    (MouseButton::Back, false) => (WM_XBUTTONUP, WPARAM(XBUTTON1 << 16)),
                    (MouseButton::Forward, true) => (WM_XBUTTONDOWN, WPARAM(XBUTTON2 << 16)),
                    (MouseButton::Forward, false) => (WM_XBUTTONUP, WPARAM(XBUTTON2 << 16)),
                };
                // Far end of the trace started in linux_surrogate.rs.
                tracing::debug!(?button, pressed, "replaying pointer button to host window");
                PostMessageW(Some(hwnd), msg, wparam, coords(x, y))
                    .map_err(|e| format!("mouse button: {e}"))
            }

            WindowInput::PointerScroll { dx, dy, .. } => {
                // The protocol counts notches with +y = up, which matches
                // Windows' sign convention, so no negation here (unlike the
                // Linux portal path, which counts the other way).
                if dy != 0 {
                    let delta = (dy * WHEEL_DELTA as i32) as i16 as usize;
                    PostMessageW(
                        Some(hwnd),
                        WM_MOUSEWHEEL,
                        WPARAM(delta << 16),
                        LPARAM(0),
                    )
                    .map_err(|e| format!("WM_MOUSEWHEEL: {e}"))?;
                }
                if dx != 0 {
                    let delta = (dx * WHEEL_DELTA as i32) as i16 as usize;
                    PostMessageW(
                        Some(hwnd),
                        WM_MOUSEHWHEEL,
                        WPARAM(delta << 16),
                        LPARAM(0),
                    )
                    .map_err(|e| format!("WM_MOUSEHWHEEL: {e}"))?;
                }
                Ok(())
            }

            WindowInput::Key { key, pressed, .. } => {
                let vk = vk_from_evdev(key)
                    .ok_or_else(|| format!("no Windows key for evdev code {key}"))?;
                let msg = if pressed { WM_KEYDOWN } else { WM_KEYUP };
                PostMessageW(Some(hwnd), msg, WPARAM(vk as usize), LPARAM(0))
                    .map_err(|e| format!("key: {e}"))
            }
        }
    }
}

/// evdev keycode (the wire format) → Windows virtual-key code.
///
/// The protocol speaks evdev everywhere so Linux needs no translation at all;
/// Windows is where the mapping has to happen. This covers the keys a
/// transferred window realistically receives — letters, digits, navigation,
/// modifiers, function keys. Anything unmapped is dropped with a log rather
/// than guessed, since a wrong key is worse than a missing one.
fn vk_from_evdev(code: u16) -> Option<u16> {
    // Values from `input-event-codes.h` on the left, `winuser.h` on the right.
    Some(match code {
        1 => 0x1B,  // ESC
        2..=10 => 0x30 + (code - 1) as u16, // 1..9 -> '1'..'9'
        11 => 0x30, // 0
        12 => 0xBD, // minus
        13 => 0xBB, // equal
        14 => 0x08, // backspace
        15 => 0x09, // tab
        16 => 0x51, // q
        17 => 0x57, // w
        18 => 0x45, // e
        19 => 0x52, // r
        20 => 0x54, // t
        21 => 0x59, // y
        22 => 0x55, // u
        23 => 0x49, // i
        24 => 0x4F, // o
        25 => 0x50, // p
        26 => 0xDB, // [
        27 => 0xDD, // ]
        28 => 0x0D, // enter
        29 => 0x11, // left ctrl
        30 => 0x41, // a
        31 => 0x53, // s
        32 => 0x44, // d
        33 => 0x46, // f
        34 => 0x47, // g
        35 => 0x48, // h
        36 => 0x4A, // j
        37 => 0x4B, // k
        38 => 0x4C, // l
        39 => 0xBA, // ;
        40 => 0xDE, // '
        41 => 0xC0, // `
        42 => 0x10, // left shift
        43 => 0xDC, // backslash
        44 => 0x5A, // z
        45 => 0x58, // x
        46 => 0x43, // c
        47 => 0x56, // v
        48 => 0x42, // b
        49 => 0x4E, // n
        50 => 0x4D, // m
        51 => 0xBC, // ,
        52 => 0xBE, // .
        53 => 0xBF, // /
        54 => 0x10, // right shift
        55 => 0x6A, // numpad *
        56 => 0x12, // left alt
        57 => 0x20, // space
        58 => 0x14, // caps lock
        59..=68 => 0x70 + (code - 59) as u16, // F1..F10
        87 => 0x7A, // F11
        88 => 0x7B, // F12
        96 => 0x0D, // numpad enter
        97 => 0x11, // right ctrl
        98 => 0x6F, // numpad /
        100 => 0x12, // right alt
        102 => 0x24, // home
        103 => 0x26, // up
        104 => 0x21, // page up
        105 => 0x25, // left
        106 => 0x27, // right
        107 => 0x23, // end
        108 => 0x28, // down
        109 => 0x22, // page down
        110 => 0x2D, // insert
        111 => 0x2E, // delete
        125 => 0x5B, // left meta / Windows key
        126 => 0x5C, // right meta
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_map_to_their_ascii_vk() {
        // Windows uses ASCII values for letters and digits, which makes these
        // checkable by eye against `input-event-codes.h`.
        assert_eq!(vk_from_evdev(30), Some(0x41)); // KEY_A -> 'A'
        assert_eq!(vk_from_evdev(44), Some(0x5A)); // KEY_Z -> 'Z'
        assert_eq!(vk_from_evdev(2), Some(0x31)); // KEY_1 -> '1'
        assert_eq!(vk_from_evdev(10), Some(0x39)); // KEY_9 -> '9'
        assert_eq!(vk_from_evdev(11), Some(0x30)); // KEY_0 -> '0'
    }

    #[test]
    fn function_keys_are_contiguous() {
        assert_eq!(vk_from_evdev(59), Some(0x70)); // F1
        assert_eq!(vk_from_evdev(68), Some(0x79)); // F10
        // F11/F12 sit elsewhere in the evdev table, so they're mapped apart.
        assert_eq!(vk_from_evdev(87), Some(0x7A));
        assert_eq!(vk_from_evdev(88), Some(0x7B));
    }

    #[test]
    fn unmapped_keys_are_dropped_not_guessed() {
        // A wrong key is worse than a missing one — better to send nothing.
        assert_eq!(vk_from_evdev(9999), None);
    }

    #[test]
    fn coords_pack_into_low_and_high_words() {
        let p = coords(0x1234, 0x5678);
        assert_eq!(p.0 & 0xFFFF, 0x1234);
        assert_eq!((p.0 >> 16) & 0xFFFF, 0x5678);
    }
}
