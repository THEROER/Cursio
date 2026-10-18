//! This phone's screen for a computer: the encoder's output down into the
//! mesh, the computer's requests and touches up into Kotlin.
//!
//! `ScreenShare` (Kotlin) owns the projection and the hardware encoder, and
//! hands each encoded unit down through `nativeScreenUnit` as a slice of the
//! codec's own direct buffer — read in place, copied once into the mesh.
//! `ScreenTouch`, the accessibility service, carries out what
//! [`phone_gesture`](cursio_core::phone_gesture) decided. Everything in
//! between is [`phone_screen`](cursio_core::phone_screen), which is the same
//! on every machine.

use std::sync::Arc;

use cursio_core::phone_gesture::Gesture;
use cursio_core::phone_screen::ScreenBackend;
use jni::objects::{JObject, JValue};

pub struct AndroidScreen;

impl AndroidScreen {
    pub fn shared() -> Arc<dyn ScreenBackend> {
        Arc::new(AndroidScreen)
    }
}

/// Call a `void` method of `CursioNative`, saying in the log what failed.
fn call(method: &str, signature: &str, args: &[JValue]) {
    if let Err(e) = crate::jvm::with_native(|env, class| {
        env.call_static_method(class, method, signature, args)?;
        Ok(())
    }) {
        tracing::warn!(method, error = %e, "the phone's screen share did not take a request");
    }
}

/// How a gesture crosses JNI: a kind, up to two points, a scroll and a
/// character. One flat call rather than an object per gesture because a drag
/// sends one per pointer move.
struct Flat {
    kind: i32,
    a: (f32, f32),
    b: (f32, f32),
    scroll: (i32, i32),
    text: Option<char>,
}

fn flat(gesture: Gesture) -> Flat {
    let none = (0.0, 0.0);
    let p = |p: cursio_core::phone_gesture::Point| (p.x, p.y);
    let simple = |kind| Flat {
        kind,
        a: none,
        b: none,
        scroll: (0, 0),
        text: None,
    };
    match gesture {
        Gesture::Down(at) => Flat {
            a: p(at),
            ..simple(0)
        },
        Gesture::Move(at) => Flat {
            a: p(at),
            ..simple(1)
        },
        Gesture::Up(at) => Flat {
            a: p(at),
            ..simple(2)
        },
        Gesture::PinchDown(a, b) => Flat {
            a: p(a),
            b: p(b),
            ..simple(3)
        },
        Gesture::PinchMove(a, b) => Flat {
            a: p(a),
            b: p(b),
            ..simple(4)
        },
        Gesture::PinchUp(a, b) => Flat {
            a: p(a),
            b: p(b),
            ..simple(5)
        },
        Gesture::Scroll { at, dx, dy } => Flat {
            a: p(at),
            scroll: (dx, dy),
            ..simple(6)
        },
        Gesture::Back => simple(7),
        Gesture::Home => simple(8),
        Gesture::Text(c) => Flat {
            text: Some(c),
            ..simple(9)
        },
        Gesture::Backspace => simple(10),
        Gesture::Enter => simple(11),
    }
}

impl ScreenBackend for AndroidScreen {
    fn ask(&self, computer: &str) {
        if let Err(e) = crate::jvm::with_native(|env, class| {
            let computer = JObject::from(env.new_string(computer)?);
            env.call_static_method(
                class,
                "onScreenAsk",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&computer)],
            )?;
            Ok(())
        }) {
            tracing::warn!(error = %e, "could not ask the phone's user to share the screen");
        }
    }

    fn stop(&self) {
        call("onScreenStop", "()V", &[]);
    }

    fn keyframe(&self) {
        call("onScreenKeyframe", "()V", &[]);
    }

    fn set_bitrate(&self, bits_per_second: u32) {
        let rate = i32::try_from(bits_per_second).unwrap_or(i32::MAX);
        call("onScreenBitrate", "(I)V", &[JValue::Int(rate)]);
    }

    fn gesture(&self, gesture: Gesture) {
        let f = flat(gesture);
        if let Err(e) = crate::jvm::with_native(|env, class| {
            let text = match f.text {
                Some(c) => JObject::from(env.new_string(c.to_string())?),
                None => JObject::null(),
            };
            env.call_static_method(
                class,
                "onScreenGesture",
                "(IFFFFIILjava/lang/String;)V",
                &[
                    JValue::Int(f.kind),
                    JValue::Float(f.a.0),
                    JValue::Float(f.a.1),
                    JValue::Float(f.b.0),
                    JValue::Float(f.b.1),
                    JValue::Int(f.scroll.0),
                    JValue::Int(f.scroll.1),
                    JValue::Object(&text),
                ],
            )?;
            Ok(())
        }) {
            tracing::warn!(error = %e, "the phone could not carry out a touch");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_core::phone_gesture::Point;

    #[test]
    fn a_pinch_carries_both_fingers_and_text_its_character() {
        let pinch = flat(Gesture::PinchMove(
            Point { x: 0.7, y: 0.5 },
            Point { x: 0.3, y: 0.5 },
        ));
        assert_eq!((pinch.kind, pinch.a, pinch.b), (4, (0.7, 0.5), (0.3, 0.5)));
        let typed = flat(Gesture::Text('é'));
        assert_eq!((typed.kind, typed.text), (9, Some('é')));
    }
}
