//! macOS input injection via CoreGraphics events.
//!
//! The receiving half of input sharing on a Mac: a peer's neutral
//! [`InputEvent`]s become synthetic `CGEvent`s posted at the HID tap, which is
//! the lowest level available to a user-space process and therefore the one
//! games and full-screen applications also read.
//!
//! Three things differ from the Linux and Windows backends, and each is why
//! this file is not a transliteration of `win.rs`:
//!
//! **There is no relative motion.** `CGEvent` carries an absolute point and
//! nothing else; a delta cannot be posted. So the cursor position is tracked
//! here, seeded from the real one whenever this machine starts being driven,
//! and each delta moves that shadow before the event is posted at it. The raw
//! delta is still attached as `kCGMouseEventDeltaX`/`Y`, because that is what a
//! game reading raw input actually consumes — an application that only ever saw
//! the absolute point would get smooth motion while the crosshair stayed put.
//! Re-seeding on activation, rather than reading the live position per event,
//! is deliberate: posting is asynchronous through the window server, so reading
//! back after each post loses whatever motion arrived in between.
//!
//! **Motion during a click is a different event.** macOS has no "the button
//! happens to be down" modifier on a move: dragging must be posted as
//! `LeftMouseDragged` and so on, or the drag does not happen at all — the file
//! does not move, the selection does not extend, the slider does not slide.
//!
//! **Modifiers are state, not keystrokes.** A `CGEvent` carries the full
//! modifier mask with it, and the mask is what applications read. Posting
//! Command-down followed by C-down does not produce Command-C unless the C
//! event itself says Command is held. So the held set is maintained here and
//! stamped onto every event, keyboard and mouse alike.
//!
//! Posting requires the Accessibility permission; without it every call
//! silently succeeds and nothing happens, which is why it is checked up front
//! rather than discovered as "the mouse does not move".

use std::collections::HashSet;
use std::ffi::c_void;

use cursio_protocol::{InputEvent, MouseButton};

use crate::mac_keymap::{self, ModifierStyle};
use crate::{InputError, InputInjector};

/// A macOS-flavoured injector, posting `CGEvent`s at the HID tap.
pub struct MacInjector {
    active: bool,
    source: EventSource,
    style: ModifierStyle,
    /// Where the shared cursor is, in global display coordinates. macOS has no
    /// relative motion, so this is what deltas accumulate into.
    position: Point,
    /// Modifier mask stamped onto every posted event.
    flags: u64,
    /// macOS keycodes currently held down through us, so a session that ends
    /// mid-press does not leave Command stuck on.
    held_keys: HashSet<u16>,
    held_buttons: HashSet<MouseButton>,
    /// Counts injected motion events, purely so the motion log can be sampled
    /// instead of firing on every event.
    motion_seq: u64,
}

impl MacInjector {
    /// Create the injector, or explain why the Mac will not accept input.
    pub fn new(style: ModifierStyle) -> Result<Self, InputError> {
        if !accessibility_granted() {
            return Err(InputError::Unavailable(
                "macOS has not granted the Accessibility permission, so injected input is \
                 discarded without an error; grant it under System Settings > Privacy & Security \
                 > Accessibility, to whichever application launched cursio (a terminal grants it \
                 on behalf of what it runs), then start cursio again"
                    .into(),
            ));
        }
        let source = EventSource::new()?;
        let position = current_pointer_position();
        tracing::info!(?style, "macOS CGEvent injector ready");
        Ok(Self {
            active: true,
            source,
            style,
            position,
            flags: 0,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            motion_seq: 0,
        })
    }

    /// The button whose drag events motion should be posted as, if any.
    ///
    /// Only one: macOS names the event after a single button, and two buttons
    /// held at once is rare enough that picking left over right — the order
    /// below — costs nothing a user would notice.
    fn dragging(&self) -> Option<MouseButton> {
        [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward,
        ]
        .into_iter()
        .find(|button| self.held_buttons.contains(button))
    }

    fn post_motion(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.position.x += f64::from(dx);
        self.position.y += f64::from(dy);
        let (event_type, button) = match self.dragging() {
            Some(button) => (drag_type(button), mouse_button_number(button)),
            None => (EVENT_MOUSE_MOVED, 0),
        };
        let event = Event::mouse(&self.source, event_type, self.position, button)?;
        event.set_flags(self.flags);
        // The absolute point moves the cursor; the delta is what raw input
        // reads. Both have to be present or one of the two audiences is blind.
        event.set_field(FIELD_DELTA_X, i64::from(dx));
        event.set_field(FIELD_DELTA_Y, i64::from(dy));
        event.post();
        Ok(())
    }

    fn post_button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InputError> {
        let event_type = button_type(button, pressed);
        let number = mouse_button_number(button);
        let event = Event::mouse(&self.source, event_type, self.position, number)?;
        event.set_flags(self.flags);
        event.set_field(FIELD_BUTTON_NUMBER, i64::from(number));
        // A click state of zero is "no click happened": buttons still report
        // pressed, but nothing that waits for a click — a button, a menu, a
        // text selection — reacts to it.
        event.set_field(FIELD_CLICK_STATE, 1);
        event.post();
        Ok(())
    }

    /// Post a key, keeping the modifier mask in step with it.
    fn post_key(&mut self, mac_key: u16, pressed: bool) -> Result<(), InputError> {
        if mac_keymap::is_caps_lock(mac_key) {
            // A latch, not a held key: the flag stays after the key is
            // released, so it flips on the press and the release does nothing.
            if pressed {
                self.flags ^= mac_keymap::CAPS_LOCK_FLAG;
                let event = Event::keyboard(&self.source, mac_key, true)?;
                event.set_type(EVENT_FLAGS_CHANGED);
                event.set_flags(self.flags);
                event.post();
            }
            return Ok(());
        }

        if let Some(bits) = mac_keymap::modifier_flags(mac_key) {
            if pressed {
                self.flags |= bits;
            } else {
                self.flags &= !bits;
            }
            // A modifier announces itself by a change of state, not by a
            // keystroke: posted as a key-down it would type nothing and leave
            // applications reading a mask that never moved.
            let event = Event::keyboard(&self.source, mac_key, pressed)?;
            event.set_type(EVENT_FLAGS_CHANGED);
            event.set_flags(self.flags);
            event.post();
            return Ok(());
        }

        let event = Event::keyboard(&self.source, mac_key, pressed)?;
        event.set_flags(self.flags | mac_keymap::intrinsic_flags(mac_key));
        event.post();
        Ok(())
    }
}

impl InputInjector for MacInjector {
    fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        if !self.active {
            // Same reasoning as the Windows backend: dropping is correct while
            // inactive, but silence hides an ownership desync, and buttons are
            // rare enough to log each time.
            if matches!(event, InputEvent::PointerButton { .. }) {
                tracing::debug!(?event, "dropping peer input: injector is inactive");
            }
            return Ok(());
        }
        match event {
            InputEvent::PointerMotion { dx, dy } => {
                let result = self.post_motion(dx, dy);
                self.motion_seq = self.motion_seq.wrapping_add(1);
                if result.is_err() || self.motion_seq.is_multiple_of(120) {
                    tracing::debug!(
                        dx,
                        dy,
                        seq = self.motion_seq,
                        x = self.position.x,
                        y = self.position.y,
                        held_buttons = self.held_buttons.len(),
                        ok = result.is_ok(),
                        "injecting pointer motion"
                    );
                }
                result
            }
            InputEvent::PointerButton { button, pressed } => {
                if pressed {
                    self.held_buttons.insert(button);
                } else {
                    self.held_buttons.remove(&button);
                }
                let result = self.post_button(button, pressed);
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
                if dx == 0 && dy == 0 {
                    return Ok(());
                }
                let event = Event::scroll(&self.source, dy, dx)?;
                event.set_flags(self.flags);
                event.post();
                Ok(())
            }
            InputEvent::Key { key, pressed } => match mac_keymap::evdev_to_mac(key, self.style) {
                Some(mac_key) => {
                    if pressed {
                        self.held_keys.insert(mac_key);
                    } else {
                        self.held_keys.remove(&mac_key);
                    }
                    self.post_key(mac_key, pressed)
                }
                None => {
                    tracing::trace!(key, "evdev key has no macOS equivalent; dropping");
                    Ok(())
                }
            },
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn set_active(&mut self, active: bool) {
        // Re-seed from the real cursor on taking over: while this machine was
        // not being driven, its own mouse moved the pointer and the shadow
        // position went stale. Without this the first remote motion teleports
        // the cursor back to wherever the last session left off.
        if active && !self.active {
            self.position = current_pointer_position();
        }
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
        for mac_key in std::mem::take(&mut self.held_keys) {
            let _ = self.post_key(mac_key, false);
        }
        for button in std::mem::take(&mut self.held_buttons) {
            let _ = self.post_button(button, false);
        }
        // Whatever the individual releases managed, the mask must end empty:
        // a Command bit left set makes every later keystroke a shortcut.
        self.flags &= mac_keymap::CAPS_LOCK_FLAG;
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "cgevent"
    }
}

/// The shared event source every posted event is created from.
///
/// Owning one, rather than passing null per event, is what lets local input
/// suppression be turned off: by default macOS ignores the real mouse for a
/// moment after a synthetic event, so a user sitting at the Mac would find
/// their own trackpad going dead while the peer typed.
struct EventSource(*mut c_void);

impl EventSource {
    fn new() -> Result<Self, InputError> {
        // SAFETY: the state id is a documented constant and the call returns
        // either a retained source or null, both handled below.
        let source = unsafe { CGEventSourceCreate(SOURCE_STATE_HID_SYSTEM) };
        if source.is_null() {
            return Err(InputError::Unavailable(
                "macOS refused to create a CGEventSource for injected input".into(),
            ));
        }
        // SAFETY: `source` is non-null and owned by us.
        unsafe {
            CGEventSourceSetLocalEventsSuppressionInterval(source, 0.0);
            // Stamps every event posted from this source, so the capture tap
            // recognises our own injection and does not send it back to the
            // peer that produced it.
            CGEventSourceSetUserData(source, crate::mac_capture::INJECTED_TAG);
        }
        Ok(Self(source))
    }
}

impl Drop for EventSource {
    fn drop(&mut self) {
        // SAFETY: created by CGEventSourceCreate and released exactly once.
        unsafe { CFRelease(self.0) };
    }
}

/// A single `CGEvent`, released when it goes out of scope.
struct Event(*mut c_void);

impl Event {
    fn wrap(raw: *mut c_void, what: &'static str) -> Result<Self, InputError> {
        if raw.is_null() {
            return Err(InputError::Unavailable(format!(
                "macOS refused to create a {what} event"
            )));
        }
        Ok(Self(raw))
    }

    fn mouse(
        source: &EventSource,
        event_type: u32,
        at: Point,
        button: u32,
    ) -> Result<Self, InputError> {
        // SAFETY: `source.0` is a live event source and `at` is a plain
        // two-double struct passed by value, as CGPoint is.
        let raw = unsafe { CGEventCreateMouseEvent(source.0, event_type, at, button) };
        Self::wrap(raw, "mouse")
    }

    fn keyboard(source: &EventSource, key: u16, pressed: bool) -> Result<Self, InputError> {
        // SAFETY: `source.0` is a live event source; the keycode is unchecked
        // by the API and simply produces nothing if it names no key.
        let raw = unsafe { CGEventCreateKeyboardEvent(source.0, key, pressed) };
        Self::wrap(raw, "keyboard")
    }

    /// Vertical and horizontal scroll, in lines.
    ///
    /// Lines rather than pixels because the neutral event counts notches, and a
    /// pixel-unit scroll of "1" would be an imperceptible twitch.
    fn scroll(source: &EventSource, vertical: i32, horizontal: i32) -> Result<Self, InputError> {
        // SAFETY: the variadic tail is the second wheel, matching the wheel
        // count of 2 passed just before it.
        let raw = unsafe {
            CGEventCreateScrollWheelEvent(source.0, SCROLL_UNIT_LINE, 2, vertical, horizontal)
        };
        Self::wrap(raw, "scroll")
    }

    fn set_type(&self, event_type: u32) {
        // SAFETY: `self.0` is a live event for as long as `self` exists.
        unsafe { CGEventSetType(self.0, event_type) };
    }

    fn set_flags(&self, flags: u64) {
        // SAFETY: as above.
        unsafe { CGEventSetFlags(self.0, flags) };
    }

    fn set_field(&self, field: u32, value: i64) {
        // SAFETY: as above; the field ids are documented constants.
        unsafe { CGEventSetIntegerValueField(self.0, field, value) };
    }

    /// Hand the event to the window server.
    ///
    /// `CGEventPost` reports nothing — a process without Accessibility gets the
    /// same silence as one that succeeded, which is why the permission is
    /// checked when the injector is built rather than here.
    fn post(&self) {
        // SAFETY: as above; the tap location is a documented constant.
        unsafe { CGEventPost(TAP_HID, self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: created by one of the CGEventCreate* calls above and released
        // exactly once.
        unsafe { CFRelease(self.0) };
    }
}

/// Whether macOS will let this process post events, asking the user if it has
/// not been decided yet.
///
/// The prompting form rather than the silent one, because the failure it
/// prevents is invisible: without Accessibility every `CGEventPost` is
/// discarded and reports success, so a user who was never asked experiences it
/// as "the mouse does not move" with nothing anywhere to search for. macOS
/// shows the dialog once per application and remembers the answer, so this
/// cannot turn into nagging.
///
/// The permission belongs to whatever macOS holds *responsible* for the
/// process, which for a binary started from a shell is the terminal — the same
/// attribution that decides Screen Recording. So the dialog may name Terminal
/// rather than cursio, and granting it there is what actually works.
fn accessibility_granted() -> bool {
    // SAFETY: the dictionary is built from one borrowed CoreFoundation
    // constant key and value with the standard retaining callbacks, and is
    // released before returning. A null result falls back to the check that
    // needs no dictionary at all.
    unsafe {
        let key = kAXTrustedCheckOptionPrompt;
        let value = kCFBooleanTrue;
        let options = CFDictionaryCreate(
            std::ptr::null(),
            &key,
            &value,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        if options.is_null() {
            return AXIsProcessTrusted();
        }
        let trusted = AXIsProcessTrustedWithOptions(options);
        CFRelease(options as *mut c_void);
        trusted
    }
}

/// Where the cursor is right now, according to the window server.
///
/// A null event source is valid here and means "whatever the system state is",
/// which is exactly the question being asked.
fn current_pointer_position() -> Point {
    // SAFETY: CGEventCreate accepts a null source; the returned event is
    // released before the point (a plain copy) is returned.
    unsafe {
        let event = CGEventCreate(std::ptr::null_mut());
        if event.is_null() {
            return Point { x: 0.0, y: 0.0 };
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        point
    }
}

fn button_type(button: MouseButton, pressed: bool) -> u32 {
    match (button, pressed) {
        (MouseButton::Left, true) => EVENT_LEFT_MOUSE_DOWN,
        (MouseButton::Left, false) => EVENT_LEFT_MOUSE_UP,
        (MouseButton::Right, true) => EVENT_RIGHT_MOUSE_DOWN,
        (MouseButton::Right, false) => EVENT_RIGHT_MOUSE_UP,
        (_, true) => EVENT_OTHER_MOUSE_DOWN,
        (_, false) => EVENT_OTHER_MOUSE_UP,
    }
}

fn drag_type(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => EVENT_LEFT_MOUSE_DRAGGED,
        MouseButton::Right => EVENT_RIGHT_MOUSE_DRAGGED,
        _ => EVENT_OTHER_MOUSE_DRAGGED,
    }
}

/// `CGMouseButton`. Middle is "center"; the side buttons continue the numbering
/// as macOS itself does for anything past the first three.
fn mouse_button_number(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

/// `CGPoint`: two doubles, in global display coordinates with the origin at the
/// top-left of the main display.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

// `CGEventType`, from CoreGraphics/CGEventTypes.h.
const EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const EVENT_LEFT_MOUSE_UP: u32 = 2;
const EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const EVENT_RIGHT_MOUSE_UP: u32 = 4;
const EVENT_MOUSE_MOVED: u32 = 5;
const EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const EVENT_FLAGS_CHANGED: u32 = 12;
const EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const EVENT_OTHER_MOUSE_UP: u32 = 26;
const EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

// `CGEventField`.
const FIELD_CLICK_STATE: u32 = 1;
const FIELD_BUTTON_NUMBER: u32 = 3;
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;

/// `kCGScrollEventUnitLine`.
const SCROLL_UNIT_LINE: u32 = 1;
/// `kCGHIDEventTap` — the lowest point a process may post at, and so the only
/// one that reaches an application reading raw input.
const TAP_HID: u32 = 0;
/// `kCGEventSourceStateHIDSystemState` — events indistinguishable from hardware.
const SOURCE_STATE_HID_SYSTEM: i32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceCreate(state: i32) -> *mut c_void;
    fn CGEventSourceSetLocalEventsSuppressionInterval(source: *mut c_void, seconds: f64);
    fn CGEventSourceSetUserData(source: *mut c_void, data: i64);
    fn CGEventCreate(source: *mut c_void) -> *mut c_void;
    fn CGEventCreateMouseEvent(
        source: *mut c_void,
        event_type: u32,
        position: Point,
        button: u32,
    ) -> *mut c_void;
    fn CGEventCreateKeyboardEvent(source: *mut c_void, key: u16, down: bool) -> *mut c_void;
    fn CGEventCreateScrollWheelEvent(
        source: *mut c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        ...
    ) -> *mut c_void;
    fn CGEventGetLocation(event: *mut c_void) -> Point;
    fn CGEventSetType(event: *mut c_void, event_type: u32);
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: *mut c_void);
}

/// The callback tables `CFDictionaryCreate` takes by address. Opaque because
/// nothing here reads them — only CoreFoundation does, and only through the
/// pointer.
#[repr(C)]
struct CFDictionaryCallBacks {
    _opaque: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;

    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const CFDictionaryCallBacks,
        value_callbacks: *const CFDictionaryCallBacks,
    ) -> *const c_void;
    fn CFRelease(reference: *mut c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: *const c_void;

    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}
