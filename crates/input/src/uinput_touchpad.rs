//! Replaying a peer's touch gestures on a virtual touchpad.
//!
//! A gesture cannot be injected on Linux as a gesture: libinput recognises
//! swipes and pinches only from a touchpad's contacts, and the compositor acts
//! only on what libinput reports. So a [`GestureSwipe`](InputEvent::GestureSwipe)
//! or [`GesturePinch`](InputEvent::GesturePinch) is played back as the fingers
//! that would have made it, on a uinput device that declares itself a
//! touchpad. The desktop then runs its own gesture — a three-finger swipe moves
//! the workspace with the fingers, a pinch zooms the photo under the pointer —
//! with the user's own settings, rather than a shortcut picked here.
//!
//! The fingers are invented, so where they start is a choice: around the
//! centre of the pad, far enough apart that libinput reads separate touches.
//! A swipe moves them all by the centroid's motion; a pinch places them on a
//! circle whose radius follows the scale and whose angle follows the rotation.
//! They are clamped to the pad, so a very long swipe stops at its edge — the
//! desktop has long since committed by then.
//!
//! The device is separate from the pointer for the same reason the keyboard
//! is: one device declaring relative axes and multitouch is classified as
//! neither.

use cursio_protocol::{GesturePhase, InputEvent, GESTURE_SCALE_ONE};
use evdev::{
    uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AbsoluteAxisEvent, AttributeSet,
    InputEvent as EvdevEvent, KeyCode, KeyEvent, PropType, UinputAbsSetup,
};

use crate::InputError;

const NAME: &str = "Cursio Virtual Touchpad";

/// Pad size in device units, and units per millimetre: 100 × 62 mm, a laptop
/// touchpad. libinput reasons in millimetres, so the resolution is what makes
/// its gesture thresholds mean what they mean on real hardware.
const WIDTH: i32 = 4000;
const HEIGHT: i32 = 2480;
const UNITS_PER_MM: i32 = 40;
/// Device units per unit of gesture motion: a pointer pixel is a tenth of a
/// millimetre of finger travel, so a swipe across a phone's screen is a few
/// centimetres here — what a workspace switch takes on a real pad.
const UNITS_PER_PIXEL: f64 = 4.0;
/// Most fingers the pad tracks; more than any gesture uses.
const SLOTS: usize = 5;
/// How far apart invented fingers start: 15 mm, clearly separate touches.
const SPREAD: f64 = 15.0 * UNITS_PER_MM as f64;

/// A gesture in progress, as fingers on the pad.
#[derive(Debug, Clone, PartialEq)]
struct Contacts {
    fingers: usize,
    /// The centroid, in device units.
    centre: (f64, f64),
    /// Pinch only: radius scale and accumulated turn, in radians.
    pinch: Option<(f64, f64)>,
}

impl Contacts {
    fn new(fingers: usize, pinch: bool) -> Self {
        Contacts {
            fingers: fingers.clamp(2, SLOTS),
            centre: (f64::from(WIDTH) / 2.0, f64::from(HEIGHT) / 2.0),
            pinch: pinch.then_some((1.0, 0.0)),
        }
    }

    fn shift(&mut self, dx: i32, dy: i32) {
        self.centre.0 += f64::from(dx) * UNITS_PER_PIXEL;
        self.centre.1 += f64::from(dy) * UNITS_PER_PIXEL;
    }

    /// Where each finger is now, clamped to the pad.
    fn positions(&self) -> Vec<(i32, i32)> {
        let n = self.fingers;
        (0..n)
            .map(|i| {
                let (x, y) = match self.pinch {
                    // On a circle, evenly spaced: spreading scales the radius,
                    // turning rotates every finger the same way.
                    Some((scale, angle)) => {
                        let theta = angle + std::f64::consts::TAU * i as f64 / n as f64;
                        let r = SPREAD / 2.0 * scale;
                        (
                            self.centre.0 + r * theta.cos(),
                            self.centre.1 + r * theta.sin(),
                        )
                    }
                    // In a row across the centre, as fingers lie for a swipe.
                    None => (
                        self.centre.0 + SPREAD * (i as f64 - (n - 1) as f64 / 2.0),
                        self.centre.1,
                    ),
                };
                (
                    (x.round() as i32).clamp(0, WIDTH),
                    (y.round() as i32).clamp(0, HEIGHT),
                )
            })
            .collect()
    }
}

/// The `BTN_TOOL_*` that says how many fingers are down. libinput counts
/// fingers by it, not by slots.
fn tool(fingers: usize) -> KeyCode {
    match fingers {
        1 => KeyCode::BTN_TOOL_FINGER,
        2 => KeyCode::BTN_TOOL_DOUBLETAP,
        3 => KeyCode::BTN_TOOL_TRIPLETAP,
        4 => KeyCode::BTN_TOOL_QUADTAP,
        _ => KeyCode::BTN_TOOL_QUINTTAP,
    }
}

pub(crate) struct VirtualTouchpad {
    device: VirtualDevice,
    active: Option<Contacts>,
    next_tracking_id: i32,
}

impl VirtualTouchpad {
    pub(crate) fn new() -> Result<Self, InputError> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for key in [
            KeyCode::BTN_LEFT,
            KeyCode::BTN_TOUCH,
            KeyCode::BTN_TOOL_FINGER,
            KeyCode::BTN_TOOL_DOUBLETAP,
            KeyCode::BTN_TOOL_TRIPLETAP,
            KeyCode::BTN_TOOL_QUADTAP,
            KeyCode::BTN_TOOL_QUINTTAP,
        ] {
            keys.insert(key);
        }
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::POINTER);
        props.insert(PropType::BUTTONPAD);
        let axis =
            |code, max| UinputAbsSetup::new(code, AbsInfo::new(0, 0, max, 0, 0, UNITS_PER_MM));
        let id_axis = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_MT_TRACKING_ID,
            AbsInfo::new(0, 0, i32::from(u16::MAX), 0, 0, 0),
        );
        let slot_axis = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_MT_SLOT,
            AbsInfo::new(0, 0, SLOTS as i32 - 1, 0, 0, 0),
        );
        let declare = |e: std::io::Error| InputError::Unavailable(format!("declare touchpad: {e}"));
        let device = VirtualDevice::builder()
            .map_err(|e| InputError::Unavailable(format!("open uinput: {e}")))?
            .name(NAME)
            .with_keys(&keys)
            .map_err(declare)?
            .with_properties(&props)
            .map_err(declare)?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, WIDTH))
            .map_err(declare)?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, HEIGHT))
            .map_err(declare)?
            .with_absolute_axis(&slot_axis)
            .map_err(declare)?
            .with_absolute_axis(&id_axis)
            .map_err(declare)?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_X, WIDTH))
            .map_err(declare)?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_Y, HEIGHT))
            .map_err(declare)?
            .build()
            .map_err(|e| InputError::Unavailable(format!("build touchpad: {e}")))?;
        tracing::info!(device = NAME, "virtual touchpad created");
        Ok(VirtualTouchpad {
            device,
            active: None,
            next_tracking_id: 0,
        })
    }

    /// Play one gesture event. Anything other than a gesture is not ours.
    pub(crate) fn play(&mut self, event: InputEvent) -> Result<(), InputError> {
        let (phase, fingers, pinch, dx, dy) = match event {
            InputEvent::GestureSwipe {
                phase,
                fingers,
                dx,
                dy,
            } => (phase, fingers, None, dx, dy),
            InputEvent::GesturePinch {
                phase,
                fingers,
                scale,
                rotation,
                dx,
                dy,
            } => (phase, fingers, Some((scale, rotation)), dx, dy),
            _ => return Ok(()),
        };
        match phase {
            GesturePhase::Begin => {
                // A gesture that began before the last one ended — a lost End,
                // or two sources at once — lifts the old fingers first, or
                // their slots stay pressed under the new ones.
                self.lift()?;
                let contacts = Contacts::new(usize::from(fingers), pinch.is_some());
                self.down(&contacts)?;
                self.active = Some(contacts);
                Ok(())
            }
            GesturePhase::Update => {
                let Some(mut contacts) = self.active.take() else {
                    return Ok(());
                };
                contacts.shift(dx, dy);
                if let (Some(state), Some((scale, rotation))) = (contacts.pinch.as_mut(), pinch) {
                    state.0 = f64::from(scale) / f64::from(GESTURE_SCALE_ONE);
                    state.1 += f64::from(rotation).to_radians() / 100.0;
                }
                let result = self.moved(&contacts);
                self.active = Some(contacts);
                result
            }
            GesturePhase::End | GesturePhase::Cancel => self.lift(),
        }
    }

    /// Lift whatever is down. Also what a session ending mid-gesture calls,
    /// so no invented finger outlives its peer.
    pub(crate) fn lift(&mut self) -> Result<(), InputError> {
        let Some(contacts) = self.active.take() else {
            return Ok(());
        };
        let mut events = Vec::with_capacity(contacts.fingers * 2 + 2);
        for slot in 0..contacts.fingers {
            events.push(abs(AbsoluteAxisCode::ABS_MT_SLOT, slot as i32));
            events.push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1));
        }
        events.push(KeyEvent::new(KeyCode::BTN_TOUCH, 0).into());
        events.push(KeyEvent::new(tool(contacts.fingers), 0).into());
        self.device.emit(&events)?;
        Ok(())
    }

    fn down(&mut self, contacts: &Contacts) -> Result<(), InputError> {
        let mut events = Vec::new();
        for (slot, (x, y)) in contacts.positions().into_iter().enumerate() {
            let id = self.next_tracking_id;
            self.next_tracking_id = (self.next_tracking_id + 1) % i32::from(u16::MAX);
            events.push(abs(AbsoluteAxisCode::ABS_MT_SLOT, slot as i32));
            events.push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, id));
            events.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x));
            events.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y));
            if slot == 0 {
                events.push(abs(AbsoluteAxisCode::ABS_X, x));
                events.push(abs(AbsoluteAxisCode::ABS_Y, y));
            }
        }
        events.push(KeyEvent::new(KeyCode::BTN_TOUCH, 1).into());
        events.push(KeyEvent::new(tool(contacts.fingers), 1).into());
        self.device.emit(&events)?;
        Ok(())
    }

    fn moved(&mut self, contacts: &Contacts) -> Result<(), InputError> {
        let mut events = Vec::new();
        for (slot, (x, y)) in contacts.positions().into_iter().enumerate() {
            events.push(abs(AbsoluteAxisCode::ABS_MT_SLOT, slot as i32));
            events.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x));
            events.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y));
            if slot == 0 {
                events.push(abs(AbsoluteAxisCode::ABS_X, x));
                events.push(abs(AbsoluteAxisCode::ABS_Y, y));
            }
        }
        self.device.emit(&events)?;
        Ok(())
    }
}

fn abs(code: AbsoluteAxisCode, value: i32) -> EvdevEvent {
    AbsoluteAxisEvent::new(code, value).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_swipe_moves_every_finger_by_the_same_amount() {
        let mut contacts = Contacts::new(3, false);
        let before = contacts.positions();
        contacts.shift(50, -20);
        let after = contacts.positions();
        for (b, a) in before.iter().zip(&after) {
            assert_eq!((a.0 - b.0, a.1 - b.1), (200, -80));
        }
        assert!(
            before[0].0 < before[1].0 && before[1].0 < before[2].0,
            "a row, not a pile"
        );
    }

    #[test]
    fn spreading_doubles_the_distance_between_pinching_fingers() {
        let mut contacts = Contacts::new(2, true);
        let gap = |p: &[(i32, i32)]| {
            let (dx, dy) = (f64::from(p[1].0 - p[0].0), f64::from(p[1].1 - p[0].1));
            dx.hypot(dy)
        };
        let start = gap(&contacts.positions());
        contacts.pinch = Some((2.0, 0.0));
        assert!((gap(&contacts.positions()) - 2.0 * start).abs() <= 2.0);
    }

    #[test]
    fn fingers_never_leave_the_pad() {
        let mut contacts = Contacts::new(4, false);
        contacts.shift(100_000, 100_000);
        for (x, y) in contacts.positions() {
            assert!((0..=WIDTH).contains(&x) && (0..=HEIGHT).contains(&y));
        }
    }
}
