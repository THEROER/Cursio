//! A phone's screen read and driven by a program here — `cursio phone`, an
//! AI agent through `cursio mcp` — rather than by a person at the window.
//!
//! Nothing new crosses the link. The phone's screen already arrives as an
//! ordinary H.264 window and already takes [`WindowInput`] back, which the
//! phone turns into touch ([`crate::phone_gesture`]). So a screenshot is the
//! stream this computer is receiving, decoded once on request
//! ([`super::shot`]), and a tap is the button press the surrogate would have
//! sent ([`crate::phone_action`]). What this keeps is only what a request
//! needs: the window's id and size, and the stream from its last keyframe —
//! the shortest run a decoder can start from.
//!
//! The screen must be shared first, which only the phone's person can allow
//! (Android shows its own consent for a capture); a request with no screen
//! says exactly that rather than waiting forever or pretending.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use cursio_protocol::diag::{DiagRequest, DiagResponse};
use cursio_protocol::{TileCodec, WindowFrame, WindowId, WindowInput, WindowMeta};
use tokio::sync::mpsc;

use crate::phone_action::{describe, steps, PhoneAction};

/// Most stream bytes kept between keyframes. A phone's encoder sends one
/// every few seconds; past this a keyframe is asked for instead of holding
/// more.
const MAX_HELD: usize = 32 * 1024 * 1024;

/// The phone's screen as this computer is showing it.
#[derive(Debug, Clone)]
struct Shown {
    id: WindowId,
    width: u32,
    height: u32,
    /// Access units from the newest keyframe on, in order.
    units: Vec<Vec<u8>>,
    held: usize,
    /// Asked the phone for a keyframe and not yet sent it.
    want_key: bool,
}

struct Phone {
    shown: Option<Shown>,
    inputs: mpsc::UnboundedSender<WindowInput>,
}

fn phones() -> std::sync::MutexGuard<'static, HashMap<String, Phone>> {
    static PHONES: OnceLock<Mutex<HashMap<String, Phone>>> = OnceLock::new();
    PHONES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// One link's end of this: what its window engine sends on to the phone.
pub struct Link {
    pub phone: String,
    pub inputs: mpsc::UnboundedReceiver<WindowInput>,
}

/// A link to `phone` came up; its engine carries what the registry asks.
pub fn register(phone: &str) -> Link {
    let (tx, rx) = mpsc::unbounded_channel();
    phones().insert(
        phone.to_owned(),
        Phone {
            shown: None,
            inputs: tx,
        },
    );
    Link {
        phone: phone.to_owned(),
        inputs: rx,
    }
}

/// The phone offered a window: its screen, when it carries the screen's title.
pub fn offered(phone: &str, meta: &WindowMeta) {
    if meta.title != super::title(phone) {
        return;
    }
    if let Some(entry) = phones().get_mut(phone) {
        entry.shown = Some(Shown {
            id: meta.id,
            width: meta.width,
            height: meta.height,
            units: Vec::new(),
            held: 0,
            want_key: false,
        });
    }
}

/// A frame of one of the phone's windows arrived.
pub fn frame(phone: &str, frame: &WindowFrame) {
    if frame.codec != TileCodec::LossyVideo {
        return;
    }
    let mut phones = phones();
    let Some(shown) = phones
        .get_mut(phone)
        .and_then(|p| p.shown.as_mut())
        .filter(|s| s.id == frame.id)
    else {
        return;
    };
    if cursio_window::video::contains_keyframe(&frame.data) {
        shown.units.clear();
        shown.held = 0;
        shown.want_key = false;
    } else if shown.units.is_empty() {
        // Nothing to build on yet; a decoder would only drop it.
        return;
    }
    // A video frame's dirty rect is the whole picture: the size the phone's
    // touch mapping reads positions against.
    if frame.dirty.width > 0 && frame.dirty.height > 0 {
        shown.width = frame.dirty.width;
        shown.height = frame.dirty.height;
    }
    shown.held += frame.data.len();
    shown.units.push(frame.data.clone());
    if shown.held > MAX_HELD {
        shown.units.clear();
        shown.held = 0;
        shown.want_key = true;
    }
}

/// One of the phone's windows closed.
pub fn closed(phone: &str, id: WindowId) {
    if let Some(entry) = phones().get_mut(phone) {
        if entry.shown.as_ref().is_some_and(|s| s.id == id) {
            entry.shown = None;
        }
    }
}

/// The window to ask the phone for a keyframe of, once.
pub fn wants_keyframe(phone: &str) -> Option<WindowId> {
    let mut phones = phones();
    let shown = phones.get_mut(phone)?.shown.as_mut()?;
    std::mem::take(&mut shown.want_key).then_some(shown.id)
}

/// The phone's name as the registry knows it, matched without case.
fn known(phones: &HashMap<String, Phone>, phone: &str) -> Result<String, String> {
    phones
        .keys()
        .find(|name| name.eq_ignore_ascii_case(phone))
        .cloned()
        .ok_or_else(|| {
            let mut names: Vec<&str> = phones.keys().map(String::as_str).collect();
            names.sort_unstable();
            match names.is_empty() {
                true => format!("{phone} is not connected, and no phone is"),
                false => format!(
                    "{phone} is not connected; phones that are: {}",
                    names.join(", ")
                ),
            }
        })
}

/// Why nothing can be done with `phone`'s screen yet.
fn not_shown(phone: &str) -> String {
    format!(
        "{phone}'s screen is not shared with this computer. Ask for it (`cursio screen {phone}`, \
         or a screenshot with ask), then allow screen sharing in the dialog on the phone"
    )
}

/// What a screenshot is made from: the picture's size and the stream from
/// its keyframe.
#[derive(Debug, Clone)]
pub struct Stream {
    pub phone: String,
    pub width: u32,
    pub height: u32,
    pub units: Vec<Vec<u8>>,
}

/// The stream of `phone`'s screen as it stands, or why there is none.
pub fn stream(phone: &str) -> Result<Stream, String> {
    let mut phones = phones();
    let name = known(&phones, phone)?;
    let entry = phones.get_mut(&name).expect("just found");
    let shown = entry.shown.as_mut().ok_or_else(|| not_shown(&name))?;
    if shown.units.is_empty() {
        shown.want_key = true;
        return Err(format!(
            "{name}'s screen is shared but no picture has arrived yet; try again in a moment"
        ));
    }
    Ok(Stream {
        phone: name,
        width: shown.width,
        height: shown.height,
        units: shown.units.clone(),
    })
}

/// How long a phone gets to answer whether its touch works, or to take text.
/// A phone older than the question drops it unanswered; past this the action
/// goes ahead the old way.
const ASK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Ask `phone` over diag; `None` when its link carries no diag or it did not
/// answer in [`ASK_WAIT`].
async fn ask(phone: &str, request: DiagRequest) -> Option<Result<DiagResponse, String>> {
    let hub = cursio_exchange::diag_hub::hub();
    if !hub
        .peers()
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(phone))
    {
        return None;
    }
    tokio::time::timeout(ASK_WAIT, hub.ask(phone, request))
        .await
        .ok()
}

/// A still `phone` took of its own screen through its shell, as PNG; `None`
/// when it did not answer — an older phone, or one not linked — so the
/// caller takes the picture from the stream instead.
pub async fn screencap(phone: &str) -> Option<Result<Vec<u8>, String>> {
    let name = known(&phones(), phone).ok()?;
    let hub = cursio_exchange::diag_hub::hub();
    let answer = tokio::time::timeout(SCREENCAP_WAIT, hub.ask(&name, DiagRequest::PhoneShot))
        .await
        .ok()?;
    Some(match answer {
        Ok(DiagResponse::WindowShot { png, .. }) => Ok(png),
        Ok(other) => Err(format!("{name} answered a screenshot with {other:?}")),
        Err(why) => Err(why),
    })
}

/// How long a phone gets to take and send its screenshot: megabytes of PNG
/// behind a sweep for its debugging port, when that moved.
const SCREENCAP_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Do `action` on `phone`'s screen, pacing the steps as asked.
///
/// Text is set in the focused field by the phone itself, any script; touch
/// is checked first, since a phone with its accessibility service off takes
/// a tap and does nothing with it.
pub async fn act(phone: &str, action: &PhoneAction) -> Result<String, String> {
    let name = known(&phones(), phone)?;
    match action {
        PhoneAction::Text { text } => {
            let typed = ask(&name, DiagRequest::PhoneType { text: text.clone() }).await;
            match typed {
                Some(Ok(_)) => return Ok(describe(&name, action)),
                Some(Err(why)) => return Err(format!("{name}: {why}")),
                // An older phone: type it key by key, ASCII only.
                None => {}
            }
        }
        _ => {
            if let Some(Ok(DiagResponse::Touch {
                ready: false,
                reason,
            })) = ask(&name, DiagRequest::PhoneTouch).await
            {
                return Err(format!(
                    "{name}: {}",
                    reason.unwrap_or_else(|| "touch from computers is off".into())
                ));
            }
        }
    }
    let (name, tx, steps) = {
        let phones = phones();
        let name = known(&phones, phone)?;
        let entry = &phones[&name];
        let shown = entry.shown.as_ref().ok_or_else(|| not_shown(&name))?;
        let steps = steps(shown.id, shown.width, shown.height, action)?;
        (name, entry.inputs.clone(), steps)
    };
    for step in steps {
        tx.send(step.input)
            .map_err(|_| format!("the link to {name} went away"))?;
        if step.then_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(step.then_ms.into())).await;
        }
    }
    Ok(describe(&name, action))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::Rect;

    fn meta(id: WindowId, title: &str) -> WindowMeta {
        WindowMeta {
            id,
            title: title.into(),
            width: 1080,
            height: 2400,
        }
    }

    fn video(id: WindowId, data: Vec<u8>) -> WindowFrame {
        WindowFrame {
            id,
            seq: 1,
            dirty: Rect {
                x: 0,
                y: 0,
                width: 540,
                height: 1200,
            },
            codec: TileCodec::LossyVideo,
            data,
        }
    }

    /// An Annex B packet holding one NAL unit of `kind`.
    fn nal(kind: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, kind, 0xAA, 0xBB]
    }

    #[test]
    fn the_stream_is_kept_from_its_newest_keyframe_and_only_for_the_screen() {
        let _link = register("Test Phone A");
        assert!(stream("test phone a")
            .unwrap_err()
            .contains("allow screen sharing"));
        offered("Test Phone A", &meta(7, "some app"));
        assert!(stream("Test Phone A").is_err(), "not the screen's title");

        offered(
            "Test Phone A",
            &meta(3, &super::super::title("Test Phone A")),
        );
        frame("Test Phone A", &video(3, nal(1)));
        assert!(stream("Test Phone A").unwrap_err().contains("no picture"));
        frame("Test Phone A", &video(3, nal(5)));
        frame("Test Phone A", &video(3, nal(1)));
        frame("Test Phone A", &video(9, nal(1)));
        let got = stream("Test Phone A").unwrap();
        assert_eq!(got.units.len(), 2, "from the keyframe, this window only");
        assert_eq!((got.width, got.height), (540, 1200), "the stream's size");
        frame("Test Phone A", &video(3, nal(5)));
        assert_eq!(stream("Test Phone A").unwrap().units.len(), 1);

        closed("Test Phone A", 3);
        assert!(stream("Test Phone A").is_err());
    }

    #[tokio::test]
    async fn an_action_reaches_the_link_as_the_window_input_it_is() {
        let mut link = register("Test Phone B");
        let title = super::super::title("Test Phone B");
        assert!(act("Test Phone B", &PhoneAction::Text { text: "a".into() })
            .await
            .unwrap_err()
            .contains("not shared"));
        offered("Test Phone B", &meta(4, &title));
        let what = act("test phone b", &PhoneAction::Text { text: "a".into() })
            .await
            .unwrap();
        assert!(what.contains("Test Phone B"));
        assert!(matches!(
            link.inputs.try_recv(),
            Ok(WindowInput::Key {
                id: 4,
                pressed: true,
                ..
            })
        ));
    }
}
