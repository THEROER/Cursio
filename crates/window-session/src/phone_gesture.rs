//! A mouse on a computer, turned into a finger on a phone's screen.
//!
//! The phone's screen arrives on the computer as an ordinary transferred
//! window, and the viewer sends back what it sends for any window: pointer
//! positions in the window's pixels, buttons, wheel notches and evdev keys.
//! A phone has no pointer, so this maps that vocabulary onto touch, the way
//! scrcpy does: the left button is a finger (pressed, moved, lifted — which
//! makes a tap, a long press and a drag all the same three steps), the wheel a
//! short swipe, the right button Back and the middle Home, and Ctrl+drag a
//! second finger mirrored about the first for a pinch.
//!
//! Pure on purpose: what it decides is the whole behaviour, and keeping it
//! free of Android lets the mapping be tested on any machine. The phone's
//! accessibility service only carries the result out.

use cursio_protocol::{MouseButton, WindowInput};

/// A point on the phone's screen, each axis 0..=1 of its width and height.
///
/// Normalised rather than in pixels because the stream is scaled down from
/// the display, and only the phone knows the size it actually draws at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// One thing to do on the phone's screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gesture {
    /// A finger touches down and stays down.
    Down(Point),
    /// The finger that is down moves here.
    Move(Point),
    /// The finger lifts here.
    Up(Point),
    /// Two fingers touch down (a pinch).
    PinchDown(Point, Point),
    PinchMove(Point, Point),
    PinchUp(Point, Point),
    /// Scroll at a point; `dy` positive moves the content up the way a wheel
    /// turned away from the user does, in wheel notches.
    Scroll {
        at: Point,
        dx: i32,
        dy: i32,
    },
    Back,
    Home,
    /// The list of recent apps.
    Recents,
    /// Type this into the field that has focus.
    Text(char),
    Backspace,
    Enter,
}

/// What the mouse is doing right now, if a button is down.
#[derive(Debug, Clone, Copy)]
struct Press {
    /// Where the press began: the pivot a pinch mirrors its second finger
    /// about.
    origin: Point,
    pinch: bool,
}

/// Turns a window's input into gestures, holding the state a gesture spans.
#[derive(Debug, Default)]
pub struct GestureMapper {
    width: u32,
    height: u32,
    press: Option<Press>,
    ctrl: bool,
    shift: bool,
}

/// evdev key codes this mapper reads.
mod key {
    pub const ESC: u16 = 1;
    pub const BACKSPACE: u16 = 14;
    pub const ENTER: u16 = 28;
    pub const LEFT_CTRL: u16 = 29;
    pub const LEFT_SHIFT: u16 = 42;
    pub const RIGHT_SHIFT: u16 = 54;
    pub const SPACE: u16 = 57;
    pub const RIGHT_CTRL: u16 = 97;
    pub const KP_ENTER: u16 = 96;
}

impl GestureMapper {
    /// A mapper for a picture `width` x `height` pixels as the viewer sees it.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            ..Self::default()
        }
    }

    /// The picture is now `width` x `height`; a finger already down stays down.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
    }

    fn point(&self, x: u32, y: u32) -> Point {
        Point {
            x: (x as f32 / self.width as f32).clamp(0.0, 1.0),
            y: (y as f32 / self.height as f32).clamp(0.0, 1.0),
        }
    }

    /// The finger a pinch places opposite the pointer, about where it began.
    fn mirror(origin: Point, at: Point) -> Point {
        Point {
            x: (2.0 * origin.x - at.x).clamp(0.0, 1.0),
            y: (2.0 * origin.y - at.y).clamp(0.0, 1.0),
        }
    }

    /// What one input event means on the phone; often nothing.
    pub fn map(&mut self, input: &WindowInput) -> Option<Gesture> {
        match *input {
            WindowInput::PointerButton {
                x,
                y,
                button,
                pressed,
                ..
            } => self.button(self.point(x, y), button, pressed),
            WindowInput::PointerMove { x, y, .. } => {
                let at = self.point(x, y);
                let press = self.press?;
                Some(if press.pinch {
                    Gesture::PinchMove(at, Self::mirror(press.origin, at))
                } else {
                    Gesture::Move(at)
                })
            }
            WindowInput::PointerScroll { dx, dy, .. } => {
                // The wheel has no position of its own; the last press, or
                // the middle of the screen, is where a finger would swipe.
                let at = self
                    .press
                    .map(|p| p.origin)
                    .unwrap_or(Point { x: 0.5, y: 0.5 });
                (dx != 0 || dy != 0).then_some(Gesture::Scroll { at, dx, dy })
            }
            WindowInput::Key { key, pressed, .. } => self.key(key, pressed),
        }
    }

    fn button(&mut self, at: Point, button: MouseButton, pressed: bool) -> Option<Gesture> {
        match (button, pressed) {
            (MouseButton::Left, true) => {
                let pinch = self.ctrl;
                self.press = Some(Press { origin: at, pinch });
                Some(if pinch {
                    // Both fingers start together, so the pinch opens from the
                    // point pressed rather than jumping to a span.
                    Gesture::PinchDown(at, at)
                } else {
                    Gesture::Down(at)
                })
            }
            (MouseButton::Left, false) => {
                let press = self.press.take()?;
                Some(if press.pinch {
                    Gesture::PinchUp(at, Self::mirror(press.origin, at))
                } else {
                    Gesture::Up(at)
                })
            }
            // On press, as scrcpy does: Back and Home are buttons on a phone,
            // and a button acts when it goes down.
            (MouseButton::Right | MouseButton::Back, true) => Some(Gesture::Back),
            (MouseButton::Middle, true) => Some(Gesture::Home),
            // scrcpy's "app switch" sits on a side button too.
            (MouseButton::Forward, true) => Some(Gesture::Recents),
            _ => None,
        }
    }

    fn key(&mut self, key: u16, pressed: bool) -> Option<Gesture> {
        match key {
            key::LEFT_CTRL | key::RIGHT_CTRL => {
                self.ctrl = pressed;
                return None;
            }
            key::LEFT_SHIFT | key::RIGHT_SHIFT => {
                self.shift = pressed;
                return None;
            }
            _ => {}
        }
        if !pressed || self.ctrl {
            return None;
        }
        match key {
            key::ESC => Some(Gesture::Back),
            key::BACKSPACE => Some(Gesture::Backspace),
            key::ENTER | key::KP_ENTER => Some(Gesture::Enter),
            key::SPACE => Some(Gesture::Text(' ')),
            _ => us_char(key, self.shift).map(Gesture::Text),
        }
    }
}

/// The evdev Shift key, for a caller typing through this mapping.
pub const LEFT_SHIFT: u16 = key::LEFT_SHIFT;

/// Each row of a US keyboard: its first evdev code, then the characters it
/// gives without and with Shift.
const US_ROWS: [(u16, &str, &str); 4] = [
    (2, "1234567890-=", "!@#$%^&*()_+"),
    (16, "qwertyuiop[]", "QWERTYUIOP{}"),
    (30, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
    (43, "\\zxcvbnm,./", "|ZXCVBNM<>?"),
];

/// The evdev key, and whether Shift is held, that this mapping turns into
/// `c` — the way back from [`us_char`], for typing text through it.
pub fn us_key(c: char) -> Option<(u16, bool)> {
    match c {
        ' ' => return Some((key::SPACE, false)),
        '\n' => return Some((key::ENTER, false)),
        _ => {}
    }
    US_ROWS.iter().find_map(|(first, plain, shifted)| {
        let at = |row: &str| row.chars().position(|r| r == c);
        at(plain)
            .map(|i| (first + i as u16, false))
            .or_else(|| at(shifted).map(|i| (first + i as u16, true)))
    })
}

/// The character a US layout gives an evdev key, with or without Shift.
///
/// US because the wire keycode is a physical position and the phone has no
/// layout of the computer's to consult; anything beyond ASCII is pasted, not
/// typed.
fn us_char(key: u16, shift: bool) -> Option<char> {
    US_ROWS.iter().find_map(|(first, plain, shifted)| {
        let index = usize::from(key.checked_sub(*first)?);
        let row = if shift { shifted } else { plain };
        row.chars().nth(index)
    })
}

/// What a gesture becomes on a phone whose own shell carries touch
/// (`docs/phone-shell-design.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellStroke {
    /// Run this `input` command.
    Run(String),
    /// Taken; the command comes when the finger lifts.
    Held,
    /// Not one the shell carries: the accessibility service does it.
    Pass,
}

/// Turns gestures into the phone shell's `input` commands.
///
/// Why the shell at all: an app may mark its screens as closed to
/// accessibility services, which Android 14+ honours by dropping their
/// injected touch — a bank's documents opened to `input tap` and not to the
/// same tap from Cursio's service. `input` is the shell's, and the app cannot
/// tell it from a finger.
///
/// Each `input` starts a process of its own, a few hundred milliseconds, so a
/// stroke is not followed as it moves: the press is held and the whole of it
/// is played when the finger lifts — a tap, a long press, or a swipe taking as
/// long as the drag did. Pinching and typing stay with the accessibility
/// service, which carries both.
#[derive(Debug, Default)]
pub struct ShellStrokes {
    press: Option<(Point, Point, std::time::Instant)>,
}

/// Movement under this many pixels is a tap, not a swipe.
const SHELL_TAP_SLOP: f32 = 16.0;
/// A press held this long without moving is a long press.
const SHELL_LONG_PRESS_MS: u128 = 450;
/// How long a wheel notch's swipe takes, as the accessibility service's does.
const SHELL_SCROLL_MS: u32 = 120;
/// One wheel notch swipes this share of the screen.
const SHELL_SCROLL_STEP: f32 = 0.12;

impl ShellStrokes {
    pub const fn new() -> Self {
        Self { press: None }
    }

    /// The command for `gesture` at `now`, on a screen `width` x `height`
    /// pixels as it is turned now.
    pub fn take(
        &mut self,
        gesture: Gesture,
        now: std::time::Instant,
        width: u32,
        height: u32,
    ) -> ShellStroke {
        let px = |p: Point| {
            (
                (p.x.clamp(0.0, 1.0) * (width.max(1) - 1) as f32).round(),
                (p.y.clamp(0.0, 1.0) * (height.max(1) - 1) as f32).round(),
            )
        };
        match gesture {
            Gesture::Down(at) => {
                self.press = Some((at, at, now));
                ShellStroke::Held
            }
            Gesture::Move(at) => match self.press.as_mut() {
                Some(press) => {
                    press.1 = at;
                    ShellStroke::Held
                }
                None => ShellStroke::Held,
            },
            Gesture::Up(at) => {
                let Some((from, _, since)) = self.press.take() else {
                    return ShellStroke::Held;
                };
                let ((x1, y1), (x2, y2)) = (px(from), px(at));
                let held = now.saturating_duration_since(since).as_millis();
                let moved = ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt();
                ShellStroke::Run(if moved < SHELL_TAP_SLOP {
                    if held < SHELL_LONG_PRESS_MS {
                        format!("input tap {x1} {y1}")
                    } else {
                        format!("input swipe {x1} {y1} {x1} {y1} {held}")
                    }
                } else {
                    format!("input swipe {x1} {y1} {x2} {y2} {}", held.max(80))
                })
            }
            Gesture::Scroll { at, dx, dy } => {
                // Wheel away from the person (dy > 0) shows what is above:
                // the finger moves down.
                let to = Point {
                    x: at.x - dx as f32 * SHELL_SCROLL_STEP,
                    y: at.y + dy as f32 * SHELL_SCROLL_STEP,
                };
                let ((x1, y1), (x2, y2)) = (px(at), px(to));
                ShellStroke::Run(format!("input swipe {x1} {y1} {x2} {y2} {SHELL_SCROLL_MS}"))
            }
            Gesture::Back => ShellStroke::Run("input keyevent 4".into()),
            Gesture::Home => ShellStroke::Run("input keyevent 3".into()),
            Gesture::Recents => ShellStroke::Run("input keyevent 187".into()),
            Gesture::Enter => ShellStroke::Run("input keyevent 66".into()),
            Gesture::Backspace => ShellStroke::Run("input keyevent 67".into()),
            Gesture::PinchDown(..)
            | Gesture::PinchMove(..)
            | Gesture::PinchUp(..)
            | Gesture::Text(_) => ShellStroke::Pass,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_plays_a_stroke_when_the_finger_lifts() {
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        let p = |x, y| Point { x, y };
        let mut strokes = ShellStrokes::default();
        assert_eq!(
            strokes.take(Gesture::Down(p(0.5, 0.5)), t0, 1001, 2001),
            ShellStroke::Held
        );
        assert_eq!(
            strokes.take(Gesture::Up(p(0.5, 0.5)), ms(80), 1001, 2001),
            ShellStroke::Run("input tap 500 1000".into())
        );
        strokes.take(Gesture::Down(p(0.5, 0.5)), t0, 1001, 2001);
        assert_eq!(
            strokes.take(Gesture::Up(p(0.5, 0.5)), ms(900), 1001, 2001),
            ShellStroke::Run("input swipe 500 1000 500 1000 900".into()),
            "held still: a long press"
        );
        strokes.take(Gesture::Down(p(0.5, 0.8)), t0, 1001, 2001);
        strokes.take(Gesture::Move(p(0.5, 0.5)), ms(100), 1001, 2001);
        assert_eq!(
            strokes.take(Gesture::Up(p(0.5, 0.2)), ms(300), 1001, 2001),
            ShellStroke::Run("input swipe 500 1600 500 400 300".into())
        );
        assert_eq!(
            strokes.take(Gesture::Back, t0, 1001, 2001),
            ShellStroke::Run("input keyevent 4".into())
        );
        assert_eq!(
            strokes.take(Gesture::Text('a'), t0, 1001, 2001),
            ShellStroke::Pass
        );
        assert_eq!(
            strokes.take(Gesture::Up(p(0.1, 0.1)), t0, 1001, 2001),
            ShellStroke::Held,
            "a lift with no press does nothing"
        );
    }

    fn button(x: u32, y: u32, button: MouseButton, pressed: bool) -> WindowInput {
        WindowInput::PointerButton {
            id: 1,
            x,
            y,
            button,
            pressed,
        }
    }

    fn key(key: u16, pressed: bool) -> WindowInput {
        WindowInput::Key {
            id: 1,
            key,
            pressed,
        }
    }

    #[test]
    fn a_click_is_a_finger_down_and_up_where_the_pointer_is() {
        let mut m = GestureMapper::new(200, 400);
        assert_eq!(
            m.map(&button(100, 100, MouseButton::Left, true)),
            Some(Gesture::Down(Point { x: 0.5, y: 0.25 }))
        );
        assert_eq!(
            m.map(&button(100, 100, MouseButton::Left, false)),
            Some(Gesture::Up(Point { x: 0.5, y: 0.25 }))
        );
    }

    #[test]
    fn moving_the_pointer_drags_only_while_the_button_is_down() {
        let mut m = GestureMapper::new(100, 100);
        assert_eq!(
            m.map(&WindowInput::PointerMove {
                id: 1,
                x: 10,
                y: 10
            }),
            None
        );
        m.map(&button(10, 10, MouseButton::Left, true));
        assert_eq!(
            m.map(&WindowInput::PointerMove {
                id: 1,
                x: 50,
                y: 10
            }),
            Some(Gesture::Move(Point { x: 0.5, y: 0.1 }))
        );
    }

    #[test]
    fn right_is_back_and_middle_is_home_on_press_only() {
        let mut m = GestureMapper::new(100, 100);
        assert_eq!(
            m.map(&button(1, 1, MouseButton::Right, true)),
            Some(Gesture::Back)
        );
        assert_eq!(m.map(&button(1, 1, MouseButton::Right, false)), None);
        assert_eq!(
            m.map(&button(1, 1, MouseButton::Middle, true)),
            Some(Gesture::Home)
        );
    }

    #[test]
    fn ctrl_drag_pinches_with_a_finger_mirrored_about_the_press() {
        let mut m = GestureMapper::new(100, 100);
        m.map(&key(key::LEFT_CTRL, true));
        assert!(matches!(
            m.map(&button(50, 50, MouseButton::Left, true)),
            Some(Gesture::PinchDown(_, _))
        ));
        assert_eq!(
            m.map(&WindowInput::PointerMove {
                id: 1,
                x: 70,
                y: 50
            }),
            Some(Gesture::PinchMove(
                Point { x: 0.7, y: 0.5 },
                Point { x: 0.3, y: 0.5 }
            ))
        );
        assert!(matches!(
            m.map(&button(70, 50, MouseButton::Left, false)),
            Some(Gesture::PinchUp(_, _))
        ));
    }

    #[test]
    fn the_wheel_swipes_and_a_still_wheel_does_nothing() {
        let mut m = GestureMapper::new(100, 100);
        assert_eq!(
            m.map(&WindowInput::PointerScroll {
                id: 1,
                dx: 0,
                dy: 2
            }),
            Some(Gesture::Scroll {
                at: Point { x: 0.5, y: 0.5 },
                dx: 0,
                dy: 2
            })
        );
        assert_eq!(
            m.map(&WindowInput::PointerScroll {
                id: 1,
                dx: 0,
                dy: 0
            }),
            None
        );
    }

    #[test]
    fn keys_type_us_characters_with_shift_and_ignore_ctrl_chords() {
        let mut m = GestureMapper::new(100, 100);
        assert_eq!(m.map(&key(30, true)), Some(Gesture::Text('a')));
        assert_eq!(m.map(&key(30, false)), None);
        m.map(&key(key::LEFT_SHIFT, true));
        assert_eq!(m.map(&key(2, true)), Some(Gesture::Text('!')));
        m.map(&key(key::LEFT_SHIFT, false));
        m.map(&key(key::LEFT_CTRL, true));
        assert_eq!(m.map(&key(46, true)), None);
        m.map(&key(key::LEFT_CTRL, false));
        assert_eq!(m.map(&key(key::BACKSPACE, true)), Some(Gesture::Backspace));
        assert_eq!(m.map(&key(key::ENTER, true)), Some(Gesture::Enter));
        assert_eq!(m.map(&key(key::ESC, true)), Some(Gesture::Back));
    }

    #[test]
    fn points_outside_the_picture_are_held_to_its_edge() {
        let mut m = GestureMapper::new(100, 100);
        assert_eq!(
            m.map(&button(250, 0, MouseButton::Left, true)),
            Some(Gesture::Down(Point { x: 1.0, y: 0.0 }))
        );
    }
}
