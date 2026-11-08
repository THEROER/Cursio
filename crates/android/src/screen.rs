//! This phone's screen for a computer: the encoder's output down into the
//! mesh, the computer's requests and touches up into Kotlin.
//!
//! `ScreenShare` (Kotlin) owns the projection and the hardware encoder, and
//! hands each encoded unit down through `nativeScreenUnit` as a slice of the
//! codec's own direct buffer — read in place, copied once into the mesh.
//! What [`phone_gesture`](cursio_core::phone_gesture) decided is carried out
//! by the phone's own shell when it is paired — its `input` reaches screens an
//! app closed to accessibility services — and otherwise, or for pinching and
//! text, by `ScreenTouch`, the accessibility service. Everything in between is
//! [`phone_screen`](cursio_core::phone_screen), which is the same on every
//! machine.

use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cursio_core::phone_gesture::{Gesture, ShellStroke, ShellStrokes};
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
        Gesture::Recents => simple(12),
    }
}

/// The accessibility service answering for itself: whether touch from a
/// computer lands, and text set in the focused field.
pub struct AndroidHands;

/// Call a `CursioNative` method answering `""` or why not.
fn answer(method: &str, text: Option<&str>) -> Result<(), String> {
    let why = crate::jvm::with_native(|env, class| {
        let value = match text {
            Some(text) => {
                let text = JObject::from(env.new_string(text)?);
                env.call_static_method(
                    class,
                    method,
                    "(Ljava/lang/String;)Ljava/lang/String;",
                    &[JValue::Object(&text)],
                )?
            }
            None => env.call_static_method(class, method, "()Ljava/lang/String;", &[])?,
        };
        let answer = jni::objects::JString::from(value.l()?);
        let answer: String = env.get_string(&answer)?.into();
        Ok(answer)
    })
    .map_err(|e| format!("the phone could not answer: {e}"))?;
    match why.is_empty() {
        true => Ok(()),
        false => Err(why),
    }
}

/// How long touch stays with the accessibility service after the shell
/// refused one: debugging turned off, or HyperOS' "USB debugging (Security
/// settings)" not on, which `input` needs there.
const SHELL_TOUCH_RETRY: Duration = Duration::from_secs(30);

static STROKES: Mutex<ShellStrokes> = Mutex::new(ShellStrokes::new());
static SHELL_REFUSED: Mutex<Option<Instant>> = Mutex::new(None);
static SHELL_QUEUE: OnceLock<Mutex<mpsc::Sender<String>>> = OnceLock::new();

fn lock_strokes() -> std::sync::MutexGuard<'static, ShellStrokes> {
    STROKES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a computer's touch goes through the shell now.
fn shell_touch_usable() -> bool {
    if !crate::shell::paired() {
        return false;
    }
    let refused = SHELL_REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    refused.is_none_or(|at| at.elapsed() >= SHELL_TOUCH_RETRY)
}

/// The screen in pixels as it is turned now; what the stream's fractions
/// are of.
fn screen_size() -> (u32, u32) {
    let packed = crate::jvm::with_native(|env, class| {
        Ok(env
            .call_static_method(class, "onScreenSize", "()J", &[])?
            .j()?)
    });
    match packed {
        Ok(packed) => ((packed >> 32) as u32, packed as u32),
        Err(e) => {
            tracing::warn!(error = %e, "the phone did not say its screen's size");
            (0, 0)
        }
    }
}

/// Hand `command` to the thread that runs the shell's `input`, in order.
///
/// Off the caller's thread: each command is a connection through the
/// debugging port and a process on the phone, a few hundred milliseconds,
/// which the module carrying the computer's input must not wait out.
fn shell_queue(command: String) {
    let queue = SHELL_QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<String>();
        let spawned = std::thread::Builder::new()
            .name("cursio-shell-touch".into())
            .spawn(move || {
                for command in rx {
                    // `input` prints nothing when it injected, and the
                    // exception when the system refused it.
                    let why = match crate::shell::run(&command) {
                        Ok(out) if out.trim().is_empty() => continue,
                        Ok(out) => out.trim().to_owned(),
                        Err(why) => why,
                    };
                    tracing::warn!(%command, %why, "the phone's shell refused a touch; the accessibility service carries touch for now");
                    *SHELL_REFUSED.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Instant::now());
                    *lock_strokes() = ShellStrokes::new();
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the shell's touch thread");
        }
        Mutex::new(tx)
    });
    if queue
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(command)
        .is_err()
    {
        tracing::warn!("the shell's touch thread is gone; the accessibility service carries touch");
        *SHELL_REFUSED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }
}

impl cursio_core::phone_hands::PhoneHands for AndroidHands {
    fn touch(&self) -> Result<(), String> {
        match answer("onTouchState", None) {
            Err(_) if shell_touch_usable() => Ok(()),
            other => other,
        }
    }

    fn type_text(&self, text: &str) -> Result<(), String> {
        answer("onTypeText", Some(text))
    }

    fn screencap(&self) -> Result<Vec<u8>, String> {
        crate::shell::run_bytes("screencap -p")
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
        let (width, height) = match shell_touch_usable() {
            true => screen_size(),
            false => (0, 0),
        };
        if width > 0 && height > 0 {
            let stroke = lock_strokes().take(gesture, Instant::now(), width, height);
            match stroke {
                ShellStroke::Run(command) => return shell_queue(command),
                ShellStroke::Held => return,
                ShellStroke::Pass => {}
            }
        }
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
