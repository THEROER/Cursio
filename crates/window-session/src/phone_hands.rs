//! A phone answering for its touch, and typing what a computer sends.
//!
//! Touch from a computer goes through an accessibility service the phone's
//! person turns on by hand (`ScreenTouch.kt`). With it off, Android simply
//! drops the gesture: a tap from `cursio phone` or an AI agent said "tapped"
//! and nothing happened. So a computer asks first ([`DiagRequest::PhoneTouch`])
//! and says what to turn on instead.
//!
//! Text goes the same way, whole, rather than as keys of a US keyboard: the
//! service sets the focused field's text, so any script reaches it, and the
//! answer says when no field had focus — which typing key by key could only
//! fail at silently.
//!
//! [`DiagRequest::PhoneTouch`]: cursio_protocol::diag::DiagRequest::PhoneTouch

use std::sync::{Arc, OnceLock};

use cursio_protocol::diag::DiagResponse;

/// What carries out touch and text on this phone.
pub trait PhoneHands: Send + Sync {
    /// `Ok` when a computer's touch would land; otherwise what the phone's
    /// person has to turn on.
    fn touch(&self) -> Result<(), String>;
    /// Put `text` into the field that has focus.
    fn type_text(&self, text: &str) -> Result<(), String>;
    /// The whole screen as PNG, taken by the phone's own shell; why not
    /// when it has none.
    fn screencap(&self) -> Result<Vec<u8>, String> {
        Err("this phone takes no screenshot of its own".into())
    }
}

static HANDS: OnceLock<Arc<dyn PhoneHands>> = OnceLock::new();

/// This phone's hands; the first call wins. A computer installs none.
pub fn install(hands: Arc<dyn PhoneHands>) {
    let _ = HANDS.set(hands);
}

fn hands() -> Result<&'static Arc<dyn PhoneHands>, String> {
    HANDS
        .get()
        .ok_or_else(|| "this machine is not a phone; it takes no touch".to_owned())
}

/// The answer to a computer asking whether touch would land.
pub fn touch() -> Result<DiagResponse, String> {
    let reason = hands()?.touch().err();
    Ok(DiagResponse::Touch {
        ready: reason.is_none(),
        reason,
    })
}

/// Why a computer's touch would not land here now, or `None` when it would.
/// A machine that is not a phone has nothing to say.
pub fn touch_blocked() -> Option<String> {
    hands().ok()?.touch().err()
}

/// The answer to a computer asking for a still of the screen.
pub fn screencap() -> Result<DiagResponse, String> {
    let png = hands()?.screencap()?;
    let (width, height) = png_size(&png).ok_or("the phone's screenshot is not a PNG")?;
    Ok(DiagResponse::WindowShot {
        png,
        width,
        height,
        title: "screen".into(),
    })
}

/// A PNG's size, from its header.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || !png.starts_with(SIGNATURE) || &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((width, height))
}

/// Type `text` into the focused field, answering how much was typed.
pub fn type_text(text: &str) -> Result<DiagResponse, String> {
    hands()?.type_text(text)?;
    Ok(DiagResponse::Typed {
        chars: text.chars().count() as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Off(Mutex<Vec<String>>);

    impl PhoneHands for Off {
        fn touch(&self) -> Result<(), String> {
            Err("turn on Cursio's accessibility service".into())
        }
        fn type_text(&self, text: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(text.to_owned());
            Ok(())
        }
    }

    #[test]
    fn a_phone_says_its_touch_is_off_and_takes_text_in_any_script() {
        let off = Arc::new(Off(Mutex::new(Vec::new())));
        install(off.clone());
        assert_eq!(
            touch().unwrap(),
            DiagResponse::Touch {
                ready: false,
                reason: Some("turn on Cursio's accessibility service".into())
            }
        );
        assert_eq!(
            type_text("Привет, мир").unwrap(),
            DiagResponse::Typed { chars: 11 }
        );
        assert_eq!(off.0.lock().unwrap().as_slice(), ["Привет, мир"]);
    }

    #[test]
    fn a_screenshot_is_sized_from_its_png_header() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1220u32.to_be_bytes());
        png.extend_from_slice(&2712u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((1220, 2712)));
        assert_eq!(png_size(b"not a png at all, longer than 24"), None);
    }
}
