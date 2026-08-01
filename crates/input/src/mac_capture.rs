//! macOS input capture via a `CGEventTap`.
//!
//! The sending half on a Mac: the real keyboard and mouse are read before any
//! application sees them, turned into neutral [`InputEvent`]s and, while this
//! machine is driving a peer, swallowed so they do not also act locally. That
//! last part is what an event tap gives us and a passive listener does not —
//! returning null from the callback deletes the event, which is macOS' answer
//! to evdev's `EVIOCGRAB`.
//!
//! ## A run loop of its own
//!
//! A tap only delivers while a `CFRunLoop` is running, and the process's main
//! run loop belongs to Tokio here. So the tap lives on a dedicated thread that
//! does nothing but run its loop, and events cross to async over a channel —
//! the same arrangement the audio backends use for their thread-bound main
//! loops, and the reason nothing here needs a lock.
//!
//! ## The system will switch the tap off
//!
//! If a callback takes too long macOS disables the tap and says so by sending
//! one final event, after which nothing arrives ever again. That is not a
//! failure worth ending capture over — it happens under load — so the disable
//! is caught and the tap switched straight back on. A tap that is not
//! re-enabled looks exactly like a keyboard that stopped working.
//!
//! ## Not capturing our own injection
//!
//! [`MacInjector`](crate::MacInjector) stamps every event it posts, and this
//! drops anything carrying that stamp. Without it a peer's keystroke would be
//! injected here, captured here, and sent back — an echo that grows.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cursio_protocol::{InputEvent, MouseButton};

use crate::mac_keymap::{self, ModifierStyle};
use crate::{CaptureHandle, EdgeCrossing, GrabControl, InputError, InputMode};

/// Stamped into every event [`MacInjector`](crate::MacInjector) posts, and
/// dropped here. Arbitrary but distinctive, and matching the Windows backend's
/// tag in spirit.
pub(crate) const INJECTED_TAG: i64 = 0x0C_09_50_10;

/// Start capturing this Mac's keyboard and mouse.
///
/// `peer_edge` is accepted for signature parity with the other platforms and
/// ignored: macOS has no pointer barriers, so edge crossing is not detected
/// here and the hotkey is what hands control over. The edges channel stays
/// empty rather than being absent, so callers do not branch on the platform.
pub fn spawn_capture(
    peer_edge: Option<cursio_protocol::Edge>,
) -> Result<CaptureHandle, InputError> {
    if peer_edge.is_some() {
        tracing::info!(
            "macOS has no pointer barriers; edge crossing is off and Ctrl+Alt+G hands over control"
        );
    }
    let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
    // Never written to: macOS detects no crossings. Held so the receiver stays
    // open and a caller awaiting it simply never wakes, instead of seeing the
    // channel close and treating capture as dead.
    let (edges_tx, edges) = tokio::sync::mpsc::unbounded_channel::<EdgeCrossing>();
    std::mem::forget(edges_tx);

    let (grab, mode_rx) = GrabControl::new();
    // The callback runs on a run-loop thread and cannot await a watch channel,
    // so the two states it needs are mirrored into atomics by a task that can.
    let grabbed = Arc::new(AtomicBool::new(false));
    let forwarding = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    spawn_mode_mirror(
        mode_rx,
        grabbed.clone(),
        forwarding.clone(),
        stopped.clone(),
    );

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let state = Box::new(TapState {
        events: events_tx,
        grabbed,
        forwarding,
        stopped,
        flags: 0,
        style: ModifierStyle::default(),
    });

    std::thread::Builder::new()
        .name("cursio-input-tap".into())
        .spawn(move || run_tap(state, ready_tx))
        .map_err(|error| {
            InputError::Unavailable(format!("could not start the input tap thread: {error}"))
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(InputError::Unavailable(error)),
        Err(_) => {
            return Err(InputError::Unavailable(
                "the input tap thread ended before it reported a tap".into(),
            ))
        }
    }

    Ok(CaptureHandle {
        events,
        edges,
        devices: vec!["macOS event tap".to_string()],
        grab,
    })
}

/// Keep the callback's atomics in step with the [`GrabControl`] watch channel.
fn spawn_mode_mirror(
    mut mode_rx: tokio::sync::watch::Receiver<InputMode>,
    grabbed: Arc<AtomicBool>,
    forwarding: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        loop {
            {
                let mode = *mode_rx.borrow_and_update();
                grabbed.store(mode.grabbed(), Ordering::Relaxed);
                forwarding.store(mode.forwards_events(), Ordering::Relaxed);
                if mode == InputMode::Shutdown {
                    // The run loop checks this and returns, which drops the tap
                    // and gives the keyboard back. Leaving a tap installed is
                    // how one edge crossing turned into four handoffs on Linux.
                    stopped.store(true, Ordering::Relaxed);
                    return;
                }
            }
            if mode_rx.changed().await.is_err() {
                stopped.store(true, Ordering::Relaxed);
                return;
            }
        }
    });
}

/// What the tap callback needs, handed to it as its user pointer.
struct TapState {
    events: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    grabbed: Arc<AtomicBool>,
    forwarding: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    /// The modifier mask as of the previous `flagsChanged`, which is the only
    /// way to tell a modifier going down from one coming up.
    flags: u64,
    style: ModifierStyle,
}

impl TapState {
    fn emit(&self, event: InputEvent) {
        if !self.forwarding.load(Ordering::Relaxed) {
            return;
        }
        let _ = self.events.send(event);
    }
}

fn run_tap(state: Box<TapState>, ready_tx: std::sync::mpsc::Sender<Result<(), String>>) {
    let stopped = state.stopped.clone();
    let user_info = Box::into_raw(state).cast::<c_void>();

    // SAFETY: the tap is created with a callback and user pointer that both
    // outlive the run loop below; every raw pointer is released before return.
    unsafe {
        let tap = CGEventTapCreate(
            TAP_HID,
            TAP_PLACE_HEAD,
            TAP_OPTION_DEFAULT,
            EVENT_MASK,
            tap_callback,
            user_info,
        );
        if tap.is_null() {
            drop(Box::from_raw(user_info.cast::<TapState>()));
            let _ = ready_tx.send(Err(
                "macOS refused an event tap, which needs the Accessibility permission; grant it \
                 under System Settings > Privacy & Security > Accessibility to whichever \
                 application launched cursio"
                    .into(),
            ));
            return;
        }

        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
        if source.is_null() {
            CFRelease(tap);
            drop(Box::from_raw(user_info.cast::<TapState>()));
            let _ = ready_tx.send(Err("macOS refused a run-loop source for the tap".into()));
            return;
        }
        let run_loop = CFRunLoopGetCurrent();
        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
        let _ = ready_tx.send(Ok(()));
        tracing::info!("macOS event tap ready");

        // `CFRunLoopRun` never returns on its own, so it is run in slices and
        // the stop flag checked between them. A tenth of a second is far below
        // anything a person notices when a session ends, and costs ten wakeups
        // a second on an otherwise idle thread.
        while !stopped.load(Ordering::Relaxed) {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, false);
        }

        tracing::info!("macOS event tap released");
        CGEventTapEnable(tap, false);
        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
        CFRelease(source);
        CFRelease(tap);
        drop(Box::from_raw(user_info.cast::<TapState>()));
    }
}

/// Called by the window server for every event before anything else sees it.
///
/// Returning the event passes it on; returning null deletes it, which is how
/// local input is frozen while this machine drives the peer.
unsafe extern "C" fn tap_callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: *mut c_void,
    user_info: *mut c_void,
) -> *mut c_void {
    // SAFETY: `user_info` is the pointer handed to CGEventTapCreate, alive for
    // the whole life of the tap, and the callback is not re-entered.
    let state = unsafe { &mut *user_info.cast::<TapState>() };

    // The system switched the tap off. Nothing arrives after this unless it is
    // turned back on, so this branch is the difference between a hiccup and a
    // keyboard that is dead for the rest of the session.
    if event_type == EVENT_TAP_DISABLED_BY_TIMEOUT || event_type == EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        tracing::warn!(event_type, "macOS disabled the input tap; re-enabling");
        // SAFETY: re-enabling by proxy is what the callback is given it for.
        unsafe { CGEventTapEnable(_proxy, true) };
        return event;
    }

    // SAFETY: `event` is a live CGEventRef for the duration of the callback.
    let injected = unsafe { CGEventGetIntegerValueField(event, FIELD_SOURCE_USER_DATA) };
    if injected == INJECTED_TAG {
        // Our own injection, on its way into this machine. Passing it through
        // is the point; capturing it would send it straight back.
        return event;
    }

    // SAFETY: as above; the field ids are documented constants.
    unsafe { translate(state, event_type, event) };

    if state.grabbed.load(Ordering::Relaxed) {
        std::ptr::null_mut()
    } else {
        event
    }
}

/// Turn one `CGEvent` into neutral events and hand them to the channel.
///
/// # Safety
///
/// `event` must be a live `CGEventRef`.
unsafe fn translate(state: &mut TapState, event_type: u32, event: *mut c_void) {
    unsafe {
        match event_type {
            EVENT_MOUSE_MOVED
            | EVENT_LEFT_MOUSE_DRAGGED
            | EVENT_RIGHT_MOUSE_DRAGGED
            | EVENT_OTHER_MOUSE_DRAGGED => {
                // The deltas the hardware reported, not a difference of
                // positions: Cursio's wire format is relative precisely so the
                // peer's cursor behaves like a second mouse rather than being
                // teleported.
                let dx = CGEventGetIntegerValueField(event, FIELD_DELTA_X) as i32;
                let dy = CGEventGetIntegerValueField(event, FIELD_DELTA_Y) as i32;
                if dx != 0 || dy != 0 {
                    state.emit(InputEvent::PointerMotion { dx, dy });
                }
            }
            EVENT_LEFT_MOUSE_DOWN | EVENT_LEFT_MOUSE_UP => {
                let pressed = event_type == EVENT_LEFT_MOUSE_DOWN;
                state.emit(InputEvent::PointerButton {
                    button: MouseButton::Left,
                    pressed,
                });
            }
            EVENT_RIGHT_MOUSE_DOWN | EVENT_RIGHT_MOUSE_UP => {
                let pressed = event_type == EVENT_RIGHT_MOUSE_DOWN;
                state.emit(InputEvent::PointerButton {
                    button: MouseButton::Right,
                    pressed,
                });
            }
            EVENT_OTHER_MOUSE_DOWN | EVENT_OTHER_MOUSE_UP => {
                let pressed = event_type == EVENT_OTHER_MOUSE_DOWN;
                let number = CGEventGetIntegerValueField(event, FIELD_BUTTON_NUMBER);
                if let Some(button) = button_from_number(number) {
                    state.emit(InputEvent::PointerButton { button, pressed });
                }
            }
            EVENT_SCROLL_WHEEL => {
                let dy = CGEventGetIntegerValueField(event, FIELD_SCROLL_AXIS_1) as i32;
                let dx = CGEventGetIntegerValueField(event, FIELD_SCROLL_AXIS_2) as i32;
                if dx != 0 || dy != 0 {
                    state.emit(InputEvent::PointerScroll { dx, dy });
                }
            }
            EVENT_KEY_DOWN | EVENT_KEY_UP => {
                let mac_key = CGEventGetIntegerValueField(event, FIELD_KEYCODE) as u16;
                if let Some(key) = mac_keymap::mac_to_evdev(mac_key, state.style) {
                    state.emit(InputEvent::Key {
                        key,
                        pressed: event_type == EVENT_KEY_DOWN,
                    });
                }
            }
            EVENT_FLAGS_CHANGED => {
                let flags = CGEventGetFlags(event);
                emit_modifier_changes(state, flags);
                state.flags = flags;
            }
            _ => {}
        }
    }
}

/// Work out which modifier moved, by comparing the new mask against the old.
///
/// A `flagsChanged` event names a keycode but says nothing about direction, and
/// the mask is shared between the two sides of each modifier — so the
/// device-dependent bits are what make "left Shift went down" distinguishable
/// from "right Shift came up".
fn emit_modifier_changes(state: &TapState, flags: u64) {
    for &mac_key in mac_keymap::MODIFIER_KEYS {
        let Some(bits) = mac_keymap::modifier_flags(mac_key) else {
            continue;
        };
        // Only the side bit: the shared mask stays set while the *other* side
        // of the same modifier is held, so testing it would report no change.
        let side = bits & !MODIFIER_MASKS;
        let was = state.flags & side != 0;
        let now = flags & side != 0;
        if was == now {
            continue;
        }
        if let Some(key) = mac_keymap::mac_to_evdev(mac_key, state.style) {
            state.emit(InputEvent::Key { key, pressed: now });
        }
    }
}

fn button_from_number(number: i64) -> Option<MouseButton> {
    match number {
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::Back),
        4 => Some(MouseButton::Forward),
        // Anything past the thumb cluster is dropped rather than guessed at,
        // matching the neutral vocabulary's own limit.
        _ => None,
    }
}

/// Every documented `CGEventFlags` mask bit, so the device-dependent bits can
/// be isolated from them.
const MODIFIER_MASKS: u64 = 0x00FF_0000;

// `CGEventType`, as in `mac.rs`.
const EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const EVENT_LEFT_MOUSE_UP: u32 = 2;
const EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const EVENT_RIGHT_MOUSE_UP: u32 = 4;
const EVENT_MOUSE_MOVED: u32 = 5;
const EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const EVENT_KEY_DOWN: u32 = 10;
const EVENT_KEY_UP: u32 = 11;
const EVENT_FLAGS_CHANGED: u32 = 12;
const EVENT_SCROLL_WHEEL: u32 = 22;
const EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const EVENT_OTHER_MOUSE_UP: u32 = 26;
const EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
const EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

/// The events asked for, as `1 << CGEventType` per type.
const EVENT_MASK: u64 = (1 << EVENT_LEFT_MOUSE_DOWN)
    | (1 << EVENT_LEFT_MOUSE_UP)
    | (1 << EVENT_RIGHT_MOUSE_DOWN)
    | (1 << EVENT_RIGHT_MOUSE_UP)
    | (1 << EVENT_MOUSE_MOVED)
    | (1 << EVENT_LEFT_MOUSE_DRAGGED)
    | (1 << EVENT_RIGHT_MOUSE_DRAGGED)
    | (1 << EVENT_KEY_DOWN)
    | (1 << EVENT_KEY_UP)
    | (1 << EVENT_FLAGS_CHANGED)
    | (1 << EVENT_SCROLL_WHEEL)
    | (1 << EVENT_OTHER_MOUSE_DOWN)
    | (1 << EVENT_OTHER_MOUSE_UP)
    | (1 << EVENT_OTHER_MOUSE_DRAGGED);

// `CGEventField`.
const FIELD_BUTTON_NUMBER: u32 = 3;
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;
const FIELD_KEYCODE: u32 = 9;
const FIELD_SCROLL_AXIS_1: u32 = 11;
const FIELD_SCROLL_AXIS_2: u32 = 12;
/// `kCGEventSourceUserData` — what the injector stamps itself with.
const FIELD_SOURCE_USER_DATA: u32 = 42;

/// `kCGHIDEventTap`: before any application, which is what makes swallowing
/// work for full-screen and raw-input readers too.
const TAP_HID: u32 = 0;
/// `kCGHeadInsertEventTap`: ahead of any other tap already installed.
const TAP_PLACE_HEAD: u32 = 0;
/// `kCGEventTapOptionDefault`: an active tap, allowed to delete events. The
/// listen-only option cannot freeze local input, which is half the job.
const TAP_OPTION_DEFAULT: u32 = 0;

type TapCallback = unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    fn CGEventGetFlags(event: *mut c_void) -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopCommonModes: *const c_void;
    static kCFRunLoopDefaultMode: *const c_void;

    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: *mut c_void,
        order: isize,
    ) -> *mut c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRunInMode(
        mode: *const c_void,
        seconds: f64,
        return_after_source_handled: bool,
    ) -> i32;
    fn CFRelease(reference: *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device-dependent bits must survive the mask being stripped, or no
    /// modifier change is ever detected.
    #[test]
    fn a_modifiers_side_bit_is_not_part_of_the_shared_mask() {
        for &key in mac_keymap::MODIFIER_KEYS {
            let bits = mac_keymap::modifier_flags(key).unwrap();
            assert_ne!(
                bits & !MODIFIER_MASKS,
                0,
                "keycode {key:#x} has no side bit"
            );
        }
    }

    /// Left and right of the same modifier share a mask; only the side bits
    /// tell them apart, which is what `emit_modifier_changes` relies on.
    #[test]
    fn the_two_sides_of_a_modifier_have_different_side_bits() {
        let left = mac_keymap::modifier_flags(0x37).unwrap() & !MODIFIER_MASKS;
        let right = mac_keymap::modifier_flags(0x36).unwrap() & !MODIFIER_MASKS;
        assert_ne!(left, right);
        assert_ne!(left, 0);
        assert_ne!(right, 0);
    }

    #[test]
    fn buttons_past_the_thumb_cluster_are_dropped() {
        assert_eq!(button_from_number(2), Some(MouseButton::Middle));
        assert_eq!(button_from_number(3), Some(MouseButton::Back));
        assert_eq!(button_from_number(4), Some(MouseButton::Forward));
        assert_eq!(button_from_number(7), None);
    }

    /// Every event the translator handles has to be asked for, or it simply
    /// never arrives.
    #[test]
    fn the_mask_covers_every_translated_event() {
        for event_type in [
            EVENT_MOUSE_MOVED,
            EVENT_LEFT_MOUSE_DRAGGED,
            EVENT_KEY_DOWN,
            EVENT_KEY_UP,
            EVENT_FLAGS_CHANGED,
            EVENT_SCROLL_WHEEL,
            EVENT_OTHER_MOUSE_DOWN,
        ] {
            assert_ne!(EVENT_MASK & (1 << event_type), 0, "type {event_type}");
        }
    }
}
