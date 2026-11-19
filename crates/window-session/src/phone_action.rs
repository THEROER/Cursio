//! Something a program here does on a phone's screen — a tap, a swipe, a
//! key, text — as the window input the phone already understands.
//!
//! The way back from [`crate::phone_gesture`]: that file turns a surrogate's
//! pointer and keys into touch on the phone; this turns "tap at 40% across,
//! 70% down" into the button press a surrogate would have sent, so the phone
//! needs nothing new. Points are fractions of the screen, 0 to 1, so the
//! caller — a person at `cursio phone`, an AI agent through `cursio mcp` —
//! need not know the phone's resolution.
//!
//! Pure, like `phone_gesture`, and built on every target: the control socket
//! names these whatever modules a build has.

use cursio_protocol::{MouseButton, WindowId, WindowInput};
use serde::{Deserialize, Serialize};

use crate::phone_gesture::{us_key, LEFT_SHIFT};

/// A key a phone has as a button, not on a keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhoneKey {
    Back,
    Home,
    Recents,
}

/// Something done on the phone's screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum PhoneAction {
    /// Touch at a point; `hold_ms` long makes it a long press.
    Tap {
        x: f32,
        y: f32,
        #[serde(default)]
        hold_ms: u32,
    },
    /// Drag a finger from one point to another over `ms`.
    Swipe {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        #[serde(default = "swipe_ms")]
        ms: u32,
    },
    Key {
        key: PhoneKey,
    },
    /// Type into the field that has focus.
    Text {
        text: String,
    },
}

fn swipe_ms() -> u32 {
    300
}

/// Longest press or swipe a request may ask for.
const MAX_MS: u32 = 10_000;

/// Longest text one request types.
const MAX_TEXT: usize = 2_000;

/// One step of an action: an input, then how long to wait before the next.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub input: WindowInput,
    pub then_ms: u32,
}

fn to_pixels(fraction: f32, size: u32) -> Result<u32, String> {
    if !(0.0..=1.0).contains(&fraction) {
        return Err(format!(
            "{fraction} is outside the screen; points are fractions from 0 to 1"
        ));
    }
    Ok(((fraction * size as f32).round() as u32).min(size.saturating_sub(1)))
}

fn button(id: WindowId, x: u32, y: u32, button: MouseButton, pressed: bool) -> WindowInput {
    WindowInput::PointerButton {
        id,
        x,
        y,
        button,
        pressed,
    }
}

/// The inputs `action` is, on a window `id` of `width` x `height`. Checked
/// whole before anything is sent: half a text typed is worse than none.
pub fn steps(
    id: WindowId,
    width: u32,
    height: u32,
    action: &PhoneAction,
) -> Result<Vec<Step>, String> {
    let step = |input, then_ms| Step { input, then_ms };
    Ok(match action {
        PhoneAction::Tap { x, y, hold_ms } => {
            let (x, y) = (to_pixels(*x, width)?, to_pixels(*y, height)?);
            vec![
                step(
                    button(id, x, y, MouseButton::Left, true),
                    (*hold_ms).min(MAX_MS),
                ),
                step(button(id, x, y, MouseButton::Left, false), 0),
            ]
        }
        PhoneAction::Swipe { x1, y1, x2, y2, ms } => {
            let (ax, ay) = (to_pixels(*x1, width)?, to_pixels(*y1, height)?);
            let (bx, by) = (to_pixels(*x2, width)?, to_pixels(*y2, height)?);
            let ms = (*ms).clamp(1, MAX_MS);
            // A move every ~16 ms, as a finger reports one per frame.
            let moves = (ms / 16).clamp(2, 120);
            let mut out = vec![step(
                button(id, ax, ay, MouseButton::Left, true),
                ms / moves,
            )];
            for i in 1..=moves {
                let t = i as f32 / moves as f32;
                let x = (ax as f32 + (bx as f32 - ax as f32) * t).round() as u32;
                let y = (ay as f32 + (by as f32 - ay as f32) * t).round() as u32;
                // The finger lifts as soon as it arrives.
                let then = if i == moves { 0 } else { ms / moves };
                out.push(step(WindowInput::PointerMove { id, x, y }, then));
            }
            out.push(step(button(id, bx, by, MouseButton::Left, false), 0));
            out
        }
        PhoneAction::Key { key } => {
            // What the surrogate sends for the phone's buttons: the right
            // button is Back, the middle Home, the forward side button
            // Recents (`phone_gesture`).
            let which = match key {
                PhoneKey::Back => MouseButton::Right,
                PhoneKey::Home => MouseButton::Middle,
                PhoneKey::Recents => MouseButton::Forward,
            };
            let (x, y) = (width / 2, height / 2);
            vec![
                step(button(id, x, y, which, true), 0),
                step(button(id, x, y, which, false), 0),
            ]
        }
        PhoneAction::Text { text } => {
            if text.chars().count() > MAX_TEXT {
                return Err(format!("one request types at most {MAX_TEXT} characters"));
            }
            let key = |key, pressed| WindowInput::Key { id, key, pressed };
            let mut out = Vec::new();
            for c in text.chars() {
                let Some((code, shift)) = us_key(c) else {
                    return Err(format!(
                        "{c:?} cannot be typed: the phone is typed on as a US keyboard, so only \
                         ASCII letters, digits, punctuation, space and newline reach it"
                    ));
                };
                if shift {
                    out.push(step(key(LEFT_SHIFT, true), 0));
                }
                out.push(step(key(code, true), 0));
                out.push(step(key(code, false), 0));
                if shift {
                    out.push(step(key(LEFT_SHIFT, false), 0));
                }
            }
            out
        }
    })
}

/// What `action` did, in a sentence, on `phone`.
pub fn describe(phone: &str, action: &PhoneAction) -> String {
    match action {
        PhoneAction::Tap { .. } => format!("tapped {phone}'s screen"),
        PhoneAction::Swipe { .. } => format!("swiped on {phone}'s screen"),
        PhoneAction::Key { key } => format!("pressed {key:?} on {phone}"),
        PhoneAction::Text { text } => {
            format!("typed {} characters on {phone}", text.chars().count())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phone_gesture::{Gesture, GestureMapper};

    #[test]
    fn a_tap_is_a_press_and_a_release_at_the_point_in_pixels() {
        let tap = PhoneAction::Tap {
            x: 0.5,
            y: 0.25,
            hold_ms: 600,
        };
        assert_eq!(
            steps(3, 1000, 2000, &tap).unwrap(),
            vec![
                Step {
                    input: button(3, 500, 500, MouseButton::Left, true),
                    then_ms: 600
                },
                Step {
                    input: button(3, 500, 500, MouseButton::Left, false),
                    then_ms: 0
                },
            ]
        );
        let outside = PhoneAction::Tap {
            x: 1.5,
            y: 0.0,
            hold_ms: 0,
        };
        assert!(steps(3, 1000, 2000, &outside).is_err());
    }

    #[test]
    fn a_swipe_moves_from_one_point_to_the_other_over_its_time() {
        let swipe = PhoneAction::Swipe {
            x1: 0.0,
            y1: 0.0,
            x2: 1.0,
            y2: 0.5,
            ms: 160,
        };
        let steps = steps(1, 100, 100, &swipe).unwrap();
        assert_eq!(
            steps.first().unwrap().input,
            button(1, 0, 0, MouseButton::Left, true)
        );
        assert_eq!(
            steps.last().unwrap().input,
            button(1, 99, 50, MouseButton::Left, false)
        );
        let paced: u32 = steps.iter().map(|s| s.then_ms).sum();
        assert!((140..=170).contains(&paced), "{paced} ms");
    }

    #[test]
    fn text_is_typed_as_keys_and_refused_whole_when_a_character_cannot_be() {
        let typed = steps(
            1,
            10,
            10,
            &PhoneAction::Text {
                text: "Hi !".into(),
            },
        )
        .unwrap();
        let mut mapper = GestureMapper::new(10, 10);
        let gestures: Vec<_> = typed.iter().filter_map(|s| mapper.map(&s.input)).collect();
        assert_eq!(
            gestures,
            vec![
                Gesture::Text('H'),
                Gesture::Text('i'),
                Gesture::Text(' '),
                Gesture::Text('!'),
            ]
        );
        let refused = steps(
            1,
            10,
            10,
            &PhoneAction::Text {
                text: "héllo".into(),
            },
        );
        assert!(refused.unwrap_err().contains("'é'"));
    }

    #[test]
    fn each_phone_key_is_the_button_the_phone_reads_it_as() {
        let mut mapper = GestureMapper::new(10, 10);
        for (key, gesture) in [
            (PhoneKey::Back, Gesture::Back),
            (PhoneKey::Home, Gesture::Home),
            (PhoneKey::Recents, Gesture::Recents),
        ] {
            let pressed = steps(1, 10, 10, &PhoneAction::Key { key }).unwrap();
            assert_eq!(mapper.map(&pressed[0].input), Some(gesture));
        }
    }

    #[test]
    fn an_action_reads_as_the_control_socket_writes_it() {
        let action: PhoneAction =
            serde_json::from_str(r#"{"do":"swipe","x1":0.5,"y1":0.8,"x2":0.5,"y2":0.2}"#).unwrap();
        assert!(matches!(action, PhoneAction::Swipe { ms: 300, .. }));
        let key: PhoneAction = serde_json::from_str(r#"{"do":"key","key":"recents"}"#).unwrap();
        assert_eq!(
            key,
            PhoneAction::Key {
                key: PhoneKey::Recents
            }
        );
    }
}
