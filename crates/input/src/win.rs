//! Windows input injection via `SendInput`.
//!
//! Injects the peer's neutral [`InputEvent`]s as synthetic Windows input.
//! Pointer motion uses **relative** deltas (`MOUSEEVENTF_MOVE` without
//! `MOUSEEVENTF_ABSOLUTE`), matching the "relative, not absolute" design
//! requirement so games reading raw input behave correctly.
//!
//! Unlike the Linux uinput backend there's no persistent device to create —
//! `SendInput` posts events straight into the OS input stream. The `active`
//! flag still gates injection so the shared-cursor model works identically.

use cursio_protocol::{InputEvent, MouseButton};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT,
};
// XBUTTON1/2 select which side button an XDOWN/XUP refers to. They live in
// WindowsAndMessaging, not KeyboardAndMouse alongside the MOUSEEVENTF_* flags.
use windows_sys::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};

use crate::win_keymap::evdev_to_scancode;
use crate::{InputError, InputInjector};

/// One wheel notch in `WHEEL_DELTA` units, what `MOUSEEVENTF_WHEEL` expects.
const WHEEL_DELTA: i32 = 120;

/// Sentinel we stamp into every injected event's `dwExtraInfo` /
/// `ExtraInformation`. The RawInput capture side ([`crate::win_capture`]) reads
/// it back and drops these events, so our own injection never feeds the capture
/// loop. Value is arbitrary but distinctive ("CURS" in ASCII-ish hex).
pub(crate) const INJECTED_TAG: u32 = 0x0C_09_50_10;

/// A `SendInput`-backed injector for Windows.
pub struct WinInjector {
    active: bool,
    /// Keys currently held down via us, as `(scancode, extended)`, so we can
    /// release them if the session ends mid-press (otherwise they stick).
    held_keys: std::collections::HashSet<(u16, bool)>,
    /// Mouse buttons currently held down via us.
    held_buttons: std::collections::HashSet<MouseButton>,
    /// Counts injected motion events, purely so the motion log can be sampled
    /// instead of firing on every event.
    motion_seq: u64,
}

impl WinInjector {
    /// Create the injector. Cannot fail — `SendInput` needs no setup — but
    /// returns `Result` to match the Linux backend's shape. Starts active.
    pub fn new() -> Result<Self, InputError> {
        tracing::info!("Windows SendInput injector ready");
        Ok(Self {
            active: true,
            held_keys: std::collections::HashSet::new(),
            held_buttons: std::collections::HashSet::new(),
            motion_seq: 0,
        })
    }

    /// Send one prepared mouse input. Returns an error if the OS rejected it.
    fn send_mouse(&self, flags: u32, dx: i32, dy: i32, mouse_data: i32) -> Result<(), InputError> {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: mouse_data as u32,
                    dwFlags: flags,
                    time: 0,
                    // Tag so our own capture drops this (see INJECTED_TAG).
                    dwExtraInfo: INJECTED_TAG as usize,
                },
            },
        };
        send_input(&input)
    }

    /// Send one key by scancode. `extended` sets the E0 flag; `pressed` chooses
    /// down vs. up. Tagged as injected so capture ignores it.
    fn send_key(&self, scancode: u16, extended: bool, pressed: bool) -> Result<(), InputError> {
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scancode,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTED_TAG as usize,
                },
            },
        };
        send_input(&input)
    }
}

/// Submit one prepared `INPUT` to the OS input stream via `SendInput`.
fn send_input(input: &INPUT) -> Result<(), InputError> {
    // SAFETY: `input` is a fully-initialised INPUT; we pass a valid pointer and
    // the matching struct size, as SendInput requires.
    let sent = unsafe { SendInput(1, input as *const INPUT, std::mem::size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(InputError::Unavailable("SendInput was blocked".into()))
    }
}

impl InputInjector for WinInjector {
    fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        if !self.active {
            // Dropping input while inactive is correct (we don't own this
            // machine), but silently dropping it hides an ownership desync: if
            // the peer thinks it is driving us while we consider ourselves
            // inactive, every event lands here and the cursor simply never
            // moves. Buttons are rare enough to log each time.
            if matches!(event, InputEvent::PointerButton { .. }) {
                tracing::debug!(?event, "dropping peer input: injector is inactive");
            }
            return Ok(());
        }
        match event {
            InputEvent::PointerMotion { dx, dy } => {
                let result = self.send_mouse(MOUSEEVENTF_MOVE, dx, dy, 0);
                // Motion is far too chatty to log per event, but silence here is
                // ambiguous: "cursor stuck" could mean no events arriving, or
                // events arriving that the OS ignores. Log the first failure and
                // then every 120th move, so both cases are distinguishable
                // without drowning the log.
                self.motion_seq = self.motion_seq.wrapping_add(1);
                if result.is_err() || self.motion_seq % 120 == 0 {
                    tracing::debug!(
                        dx,
                        dy,
                        seq = self.motion_seq,
                        held_buttons = self.held_buttons.len(),
                        ok = result.is_ok(),
                        "injecting pointer motion"
                    );
                }
                result
            }
            InputEvent::PointerButton { button, pressed } => {
                // Side buttons share one flag pair; which one is selected by
                // mouseData (XBUTTON1/XBUTTON2) rather than by the flag itself.
                let (flag, mouse_data) = match (button, pressed) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as i32),
                    (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
                    (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as i32),
                    (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
                };
                if pressed {
                    self.held_buttons.insert(button);
                } else {
                    self.held_buttons.remove(&button);
                }
                let result = self.send_mouse(flag, 0, 0, mouse_data);
                // The peer end of the button trace (see wl_capture's "captured
                // pointer button"). `held` is the load-bearing part: if it stays
                // non-empty after a click, a release never arrived and Windows
                // still thinks the button is down — which turns every later move
                // into a drag and reads as "the mouse froze after I clicked".
                tracing::debug!(
                    ?button,
                    pressed,
                    held = self.held_buttons.len(),
                    ok = result.is_ok(),
                    "injected pointer button"
                );
                result
            }
            InputEvent::PointerScroll { dx, dy } => {
                // Neutral scroll is in notches; Windows wants WHEEL_DELTA units.
                if dy != 0 {
                    self.send_mouse(MOUSEEVENTF_WHEEL, 0, 0, dy * WHEEL_DELTA)?;
                }
                if dx != 0 {
                    self.send_mouse(MOUSEEVENTF_HWHEEL, 0, 0, dx * WHEEL_DELTA)?;
                }
                Ok(())
            }
            InputEvent::Key { key, pressed } => {
                // Neutral keys are evdev codes; translate to a Windows scancode
                // (position-based) and inject as a scancode event.
                match evdev_to_scancode(key) {
                    Some((scancode, extended)) => {
                        if pressed {
                            self.held_keys.insert((scancode, extended));
                        } else {
                            self.held_keys.remove(&(scancode, extended));
                        }
                        self.send_key(scancode, extended, pressed)
                    }
                    None => {
                        tracing::trace!(key, "unmapped evdev key; dropping");
                        Ok(())
                    }
                }
            }
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        if self.held_keys.is_empty() && self.held_buttons.is_empty() {
            return Ok(());
        }
        tracing::info!(
            keys = self.held_keys.len(),
            buttons = self.held_buttons.len(),
            "releasing held keys/buttons on session end"
        );
        for (scancode, extended) in std::mem::take(&mut self.held_keys) {
            let _ = self.send_key(scancode, extended, false);
        }
        for button in std::mem::take(&mut self.held_buttons) {
            let (flag, mouse_data) = match button {
                MouseButton::Left => (MOUSEEVENTF_LEFTUP, 0),
                MouseButton::Right => (MOUSEEVENTF_RIGHTUP, 0),
                MouseButton::Middle => (MOUSEEVENTF_MIDDLEUP, 0),
                MouseButton::Back => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
                MouseButton::Forward => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
            };
            let _ = self.send_mouse(flag, 0, 0, mouse_data);
        }
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "sendinput"
    }
}
