//! Linux input injection via uinput (`evdev` crate).
//!
//! Creates two **persistent** virtual devices at construction — a pointer and a
//! keyboard — and injects relative motion, buttons, scroll, and key presses
//! into them. The devices live for the injector's lifetime; an `active` flag
//! gates whether events are actually emitted (see [`crate::InputInjector`]).
//!
//! Pointer and keyboard are separate uinput devices because a single evdev
//! device mixing relative axes with a full keyboard key set is classified
//! oddly by the compositor (e.g. as a touchpad). Keeping them apart makes each
//! enumerate as the plain "mouse" / "keyboard" the OS expects.

use cursio_protocol::{InputEvent, MouseButton};
use evdev::{
    uinput::VirtualDevice, AttributeSet, InputEvent as EvdevEvent, KeyCode, KeyEvent,
    RelativeAxisCode, RelativeAxisEvent,
};

use crate::{InputError, InputInjector};

/// The name the virtual pointer advertises to the OS. Shows up in
/// `libinput list-devices` / `/proc/bus/input/devices`.
const POINTER_NAME: &str = "Cursio Virtual Pointer";

/// The name the virtual keyboard advertises to the OS.
const KEYBOARD_NAME: &str = "Cursio Virtual Keyboard";

/// First mouse/gamepad button code (`BTN_0`). Keyboard keys are all below it;
/// codes at or above it are pointer buttons that belong on the pointer device,
/// not the keyboard. Used to bound the keyboard's declared key set.
const BTN_0: u16 = KeyCode::BTN_0.0;

/// Turn a failure to open `/dev/uinput` into something the user can act on.
///
/// "Permission denied" is the overwhelmingly common case on a fresh machine and
/// the least useful thing to print: the device exists, the daemon is running,
/// and nothing on screen says that a peer's keyboard will simply do nothing.
/// Every other error is passed through as it came — inventing a remedy for a
/// cause we did not identify is how a user ends up adding a udev rule that was
/// never the problem.
fn open_uinput_error(e: std::io::Error) -> InputError {
    if e.kind() != std::io::ErrorKind::PermissionDenied {
        return InputError::Unavailable(format!("open uinput: {e}"));
    }
    InputError::Unavailable(format!(
        "open uinput: {e} — this user cannot write /dev/uinput, so a peer's keyboard and mouse \
         will not reach this machine. Grant it with a udev rule: \
         echo 'KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\"' | \
         sudo tee /etc/udev/rules.d/99-cursio-uinput.rules, then \
         sudo usermod -aG input $USER and log out and back in"
    ))
}

/// A uinput-backed injector owning persistent virtual pointer and keyboard
/// devices.
pub struct UinputInjector {
    pointer: VirtualDevice,
    keyboard: VirtualDevice,
    active: bool,
    /// evdev keycodes currently held down via us (keyboard keys and mouse
    /// buttons), so we can release them if the session ends mid-press.
    held: std::collections::HashSet<u16>,
}

impl UinputInjector {
    /// Create the virtual pointer device.
    ///
    /// Starts **active**. Requires write access to `/dev/uinput` (a udev rule
    /// or the `input` group may be needed on some hosts). The device may take
    /// ~1-2s to be enumerated by the compositor after creation.
    pub fn new() -> Result<Self, InputError> {
        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::BTN_LEFT);
        keys.insert(KeyCode::BTN_RIGHT);
        keys.insert(KeyCode::BTN_MIDDLE);
        // Thumb buttons must be declared here too, not just mapped: the kernel
        // silently drops events for codes a uinput device never advertised, so
        // a missing line here looks like "back/forward do nothing" with no error.
        keys.insert(KeyCode::BTN_SIDE);
        keys.insert(KeyCode::BTN_EXTRA);

        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        axes.insert(RelativeAxisCode::REL_X);
        axes.insert(RelativeAxisCode::REL_Y);
        axes.insert(RelativeAxisCode::REL_WHEEL);
        axes.insert(RelativeAxisCode::REL_HWHEEL);

        let pointer = VirtualDevice::builder()
            .map_err(open_uinput_error)?
            .name(POINTER_NAME)
            .with_keys(&keys)
            .map_err(|e| InputError::Unavailable(format!("declare pointer keys: {e}")))?
            .with_relative_axes(&axes)
            .map_err(|e| InputError::Unavailable(format!("declare axes: {e}")))?
            .build()
            .map_err(|e| InputError::Unavailable(format!("build pointer: {e}")))?;

        tracing::info!(device = POINTER_NAME, "virtual pointer created");

        // Declare the full keyboard range (KEY_ESC..BTN_0). We can't know in
        // advance which keys the peer will send, and a uinput device can only
        // emit codes it declared at build time, so declare them all up to the
        // first pointer-button code.
        let mut keyboard_keys = AttributeSet::<KeyCode>::new();
        for code in KeyCode::KEY_ESC.0..BTN_0 {
            keyboard_keys.insert(KeyCode::new(code));
        }

        let keyboard = VirtualDevice::builder()
            .map_err(open_uinput_error)?
            .name(KEYBOARD_NAME)
            .with_keys(&keyboard_keys)
            .map_err(|e| InputError::Unavailable(format!("declare keyboard keys: {e}")))?
            .build()
            .map_err(|e| InputError::Unavailable(format!("build keyboard: {e}")))?;

        tracing::info!(device = KEYBOARD_NAME, "virtual keyboard created");

        Ok(Self {
            pointer,
            keyboard,
            active: true,
            held: std::collections::HashSet::new(),
        })
    }

    /// Emit a batch of evdev events. `emit` appends the SYN itself, committing
    /// the batch as one atomic update.
    fn emit(&mut self, events: &[EvdevEvent]) -> Result<(), InputError> {
        self.pointer.emit(events)?;
        Ok(())
    }

    /// Emit a batch on the keyboard device, committed atomically by `emit`.
    fn emit_keyboard(&mut self, events: &[EvdevEvent]) -> Result<(), InputError> {
        self.keyboard.emit(events)?;
        Ok(())
    }

    /// Record a key/button as held (`pressed`) or released, so `release_all`
    /// knows what to let go if the session ends mid-press.
    fn track(&mut self, code: u16, pressed: bool) {
        if pressed {
            self.held.insert(code);
        } else {
            self.held.remove(&code);
        }
    }
}

impl InputInjector for UinputInjector {
    fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        if !self.active {
            // Inactive: drop the event so the local cursor stays frozen. The
            // device remains alive for instant reactivation.
            return Ok(());
        }

        match event {
            InputEvent::PointerMotion { dx, dy } => {
                // X and Y are separate REL events, committed together by emit.
                self.emit(&[
                    RelativeAxisEvent::new(RelativeAxisCode::REL_X, dx).into(),
                    RelativeAxisEvent::new(RelativeAxisCode::REL_Y, dy).into(),
                ])
            }
            InputEvent::PointerButton { button, pressed } => {
                let key = map_button(button);
                self.track(key.0, pressed);
                self.emit(&[KeyEvent::new(key, pressed as i32).into()])
            }
            InputEvent::PointerScroll { dx, dy } => {
                let mut evs: Vec<EvdevEvent> = Vec::with_capacity(2);
                if dy != 0 {
                    evs.push(RelativeAxisEvent::new(RelativeAxisCode::REL_WHEEL, dy).into());
                }
                if dx != 0 {
                    evs.push(RelativeAxisEvent::new(RelativeAxisCode::REL_HWHEEL, dx).into());
                }
                if evs.is_empty() {
                    Ok(())
                } else {
                    self.emit(&evs)
                }
            }
            InputEvent::Key { key, pressed } => {
                // Neutral keycodes already use the evdev namespace, so `key` is
                // an evdev code directly. Route to the keyboard device.
                if key >= BTN_0 {
                    // Pointer buttons don't belong on the keyboard; the pointer
                    // path handles those as PointerButton. Drop to be safe.
                    tracing::trace!(key, "dropping non-keyboard code sent as Key");
                    return Ok(());
                }
                self.track(key, pressed);
                self.emit_keyboard(&[KeyEvent::new(KeyCode::new(key), pressed as i32).into()])
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
        if self.held.is_empty() {
            return Ok(());
        }
        // Release each held code on its device (mouse buttons on the pointer,
        // keys on the keyboard), ignoring `active` — we must clean up even if
        // we ended in the inactive state.
        let held: Vec<u16> = self.held.drain().collect();
        tracing::info!(
            count = held.len(),
            "releasing held keys/buttons on session end"
        );
        for code in held {
            let ev = KeyEvent::new(KeyCode::new(code), 0).into();
            let res = if code >= BTN_0 {
                self.pointer.emit(&[ev])
            } else {
                self.keyboard.emit(&[ev])
            };
            if let Err(e) = res {
                tracing::warn!(code, error = %e, "failed to release held code");
            }
        }
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "uinput"
    }
}

/// Map a neutral [`MouseButton`] to its evdev key.
fn map_button(button: MouseButton) -> KeyCode {
    match button {
        MouseButton::Left => KeyCode::BTN_LEFT,
        MouseButton::Right => KeyCode::BTN_RIGHT,
        MouseButton::Middle => KeyCode::BTN_MIDDLE,
        // The thumb buttons. evdev's BTN_SIDE/BTN_EXTRA are what browsers and
        // toolkits read as back/forward, matching XBUTTON1/XBUTTON2 on Windows.
        MouseButton::Back => KeyCode::BTN_SIDE,
        MouseButton::Forward => KeyCode::BTN_EXTRA,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_map_to_expected_evdev_codes() {
        assert_eq!(map_button(MouseButton::Left), KeyCode::BTN_LEFT);
        assert_eq!(map_button(MouseButton::Right), KeyCode::BTN_RIGHT);
        assert_eq!(map_button(MouseButton::Middle), KeyCode::BTN_MIDDLE);
        assert_eq!(map_button(MouseButton::Back), KeyCode::BTN_SIDE);
        assert_eq!(map_button(MouseButton::Forward), KeyCode::BTN_EXTRA);
        assert_eq!(map_button(MouseButton::Back), KeyCode::BTN_SIDE);
        assert_eq!(map_button(MouseButton::Forward), KeyCode::BTN_EXTRA);
    }
}
