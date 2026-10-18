//! This phone's notifications, on their way to the desktop the user is at.
//!
//! Android hands them to `CursioNotificationListener`, which passes each one
//! down through `nativeNotificationPosted`; from there they go to the mesh's
//! [`hub`](cursio_core::notify_hub::hub). What a desktop's user does with one
//! comes back as a call up into `CursioNative.onNotificationInvoke` or
//! `onNotificationDismiss`, which press the button or dismiss it as if on this
//! screen. A desktop asking the phone to ring comes up the same way, into
//! `CursioNative.onRing`.

use std::sync::Arc;

use cursio_core::notify_hub::NotifyActor;
use cursio_protocol::notify::{Notification, NotifyAction, NotifyKind, MAX_ICON_BYTES};
use jni::objects::{JObject, JValue};

pub struct AndroidActor;

impl AndroidActor {
    pub fn shared() -> Arc<dyn NotifyActor> {
        Arc::new(AndroidActor)
    }
}

impl NotifyActor for AndroidActor {
    fn invoke(&self, key: &str, action: u8, reply: Option<&str>) {
        let pressed = crate::jvm::with_native(|env, class| {
            let key = JObject::from(env.new_string(key)?);
            let reply = match reply {
                Some(text) => JObject::from(env.new_string(text)?),
                None => JObject::null(),
            };
            env.call_static_method(
                class,
                "onNotificationInvoke",
                "(Ljava/lang/String;ILjava/lang/String;)V",
                &[
                    JValue::Object(&key),
                    JValue::Int(i32::from(action)),
                    JValue::Object(&reply),
                ],
            )?;
            Ok(())
        });
        if let Err(e) = pressed {
            tracing::warn!(error = %e, "could not press a notification's button");
        }
    }

    fn dismiss(&self, key: &str) {
        let dismissed = crate::jvm::with_native(|env, class| {
            let key = JObject::from(env.new_string(key)?);
            env.call_static_method(
                class,
                "onNotificationDismiss",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&key)],
            )?;
            Ok(())
        });
        if let Err(e) = dismissed {
            tracing::warn!(error = %e, "could not dismiss a notification");
        }
    }

    fn ring(&self, on: bool) {
        let rung = crate::jvm::with_native(|env, class| {
            env.call_static_method(class, "onRing", "(Z)V", &[JValue::Bool(u8::from(on))])?;
            Ok(())
        });
        if let Err(e) = rung {
            tracing::warn!(error = %e, on, "could not ring this phone");
        }
    }
}

/// What the listener read from one notification, as the mesh carries it.
#[allow(clippy::too_many_arguments)]
pub fn notification(
    key: String,
    app_id: String,
    app_name: String,
    title: String,
    text: String,
    icon: Option<Vec<u8>>,
    when_ms: i64,
    kind: &str,
    actions: Vec<(String, bool)>,
) -> Notification {
    Notification {
        key,
        app_id,
        app_name,
        title,
        text,
        icon: icon.filter(|png| !png.is_empty() && png.len() <= MAX_ICON_BYTES),
        when_ms,
        kind: match kind {
            "call" => NotifyKind::Call,
            "message" => NotifyKind::Message,
            _ => NotifyKind::Other,
        },
        actions: actions
            .into_iter()
            .map(|(title, reply)| NotifyAction { title, reply })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_icon_is_dropped_rather_than_sent() {
        let n = notification(
            "k".into(),
            "a".into(),
            "A".into(),
            "t".into(),
            "x".into(),
            Some(vec![0; MAX_ICON_BYTES + 1]),
            0,
            "message",
            vec![("Reply".into(), true)],
        );
        assert_eq!(n.icon, None);
        assert_eq!(n.kind, NotifyKind::Message);
        assert!(n.actions[0].reply);
    }
}
