//! The phone's clipboard: written from the node, read only by a tap.
//!
//! What crosses and when is `cursio_core::clipboard_session::phone`'s; this
//! is the JVM's half. Writing is `ClipboardManager.setPrimaryClip`, which an
//! app may call from the background, so the node calls up into
//! `CursioNative.putClipboard` (see [`crate::jvm`]). Reading is only ever done
//! by `ClipboardSendActivity`, the moment its window has focus after the user
//! tapped the tile or the notification's button, and the text it read comes
//! down through [`send`].

use std::sync::{Arc, Mutex};

use cursio_core::clipboard_session::{Destination, PhoneClipboard, SystemClipboard};
use cursio_core::module::ModuleSwitches;
use jni::objects::{JObject, JString, JValue};

/// The running node's phone clipboard. Replaced when a node starts, so a
/// stopped and restarted node is the one a tap reaches.
static PHONE: Mutex<Option<PhoneClipboard>> = Mutex::new(None);

fn slot() -> std::sync::MutexGuard<'static, Option<PhoneClipboard>> {
    PHONE.lock().unwrap_or_else(|p| p.into_inner())
}

/// The clipboard every link's module on this node shares.
pub fn phone(switches: ModuleSwitches) -> PhoneClipboard {
    let phone = PhoneClipboard::new(Arc::new(AndroidClipboard), switches);
    *slot() = Some(phone.clone());
    phone
}

struct AndroidClipboard;

impl SystemClipboard for AndroidClipboard {
    fn put_text(&self, text: &str) -> Result<(), String> {
        let answer = crate::jvm::with_native(|env, class| {
            let text = JObject::from(env.new_string(text)?);
            let result = env.call_static_method(
                class,
                "putClipboard",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&text)],
            )?;
            let answer: JString = result.l()?.into();
            let answer: String = env.get_string(&answer)?.into();
            Ok(answer)
        })?;
        if answer.is_empty() {
            Ok(())
        } else {
            Err(answer)
        }
    }
}

/// Where a tap sends the clipboard, as JSON for the activity:
/// `{"to":[id,name]}`, `{"pick":[[id,name],…]}` or `{"nowhere":why}`.
pub fn destination_json() -> String {
    let destination = match slot().as_ref() {
        None => Destination::Nowhere("Cursio is not running".into()),
        Some(phone) => phone.destination(cursio_core::notify_hub::hub().last_used_desktop_id()),
    };
    destination_to_json(&destination).to_string()
}

fn destination_to_json(destination: &Destination) -> serde_json::Value {
    match destination {
        Destination::To { peer, name } => serde_json::json!({ "to": [peer.to_string(), name] }),
        Destination::Pick(computers) => serde_json::json!({
            "pick": computers
                .iter()
                .map(|(peer, name)| [peer.to_string(), name.clone()])
                .collect::<Vec<_>>()
        }),
        Destination::Nowhere(why) => serde_json::json!({ "nowhere": why }),
    }
}

/// Offer `text`, read by the tap, to `peer`. Answers the computer's name.
pub fn send(peer: &str, text: String) -> Result<String, String> {
    let phone = slot().clone().ok_or("Cursio is not running")?;
    phone.hand(peer, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::PeerId;

    #[test]
    fn the_activity_reads_each_destination_by_its_one_key() {
        let peer = PeerId::from_u128(1);
        let to = destination_to_json(&Destination::To {
            peer,
            name: "Nocturne".into(),
        });
        assert_eq!(to["to"][1], "Nocturne");
        assert_eq!(to["to"][0], peer.to_string());
        let pick = destination_to_json(&Destination::Pick(vec![(peer, "Nocturne".into())]));
        assert_eq!(pick["pick"][0][1], "Nocturne");
        let nowhere = destination_to_json(&Destination::Nowhere("off".into()));
        assert_eq!(nowhere["nowhere"], "off");
    }
}
