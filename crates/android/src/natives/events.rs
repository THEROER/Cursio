//! What happens on the phone that its computers hear about: notifications,
//! calls, messages, the gallery, and whether the phone is in the user's hand.

use crate::guard;
#[cfg(target_os = "android")]
use jni::objects::JClass;
use jni::objects::{JObject, JString};
use jni::sys::{jboolean, JNI_FALSE};
use jni::JNIEnv;

/// One notification posted on this phone, for the desktop the user is at.
///
/// `icon` is a PNG or null; `kind` is `call`, `message` or anything else;
/// `action_titles` and `action_replies` are the notification's buttons in
/// order, the second saying which take text.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeNotificationPosted(
    mut env: JNIEnv,
    _this: JObject,
    key: JString,
    app_id: JString,
    app_name: JString,
    title: JString,
    text: JString,
    icon: jni::objects::JByteArray,
    when_ms: jni::sys::jlong,
    kind: JString,
    action_titles: jni::objects::JObjectArray,
    action_replies: jni::objects::JBooleanArray,
    answer: jni::sys::jint,
    decline: jni::sys::jint,
    hang_up: jni::sys::jint,
) {
    guard(&mut env, (), |env| {
        let mut read = |s: &JString| -> anyhow::Result<String> { Ok(env.get_string(s)?.into()) };
        let (key, app_id, app_name, title, text, kind) = (
            read(&key)?,
            read(&app_id)?,
            read(&app_name)?,
            read(&title)?,
            read(&text)?,
            read(&kind)?,
        );
        let icon = if icon.is_null() {
            None
        } else {
            Some(env.convert_byte_array(&icon)?)
        };
        let count = env.get_array_length(&action_titles)?;
        let mut replies = vec![0u8; usize::try_from(count).unwrap_or(0)];
        env.get_boolean_array_region(&action_replies, 0, &mut replies)?;
        let mut actions = Vec::new();
        for i in 0..count {
            let title: JString = env.get_object_array_element(&action_titles, i)?.into();
            let title: String = env.get_string(&title)?.into();
            actions.push((title, replies[i as usize] != 0));
        }
        let telephony = kind == "dialer_call";
        let notification = crate::notify::notification(
            key, app_id, app_name, title, text, icon, when_ms, &kind, actions,
        );
        if notification.kind == cursio_protocol::notify::NotifyKind::Call {
            // -1 from Kotlin is "no such button".
            let role = |i: jni::sys::jint| u8::try_from(i).ok();
            let roles = cursio_core::notify_hub::CallRoles {
                answer: role(answer),
                decline: role(decline),
                hang_up: role(hang_up),
            };
            cursio_core::notify_hub::hub().post_call(notification, telephony, roles);
        } else {
            cursio_core::notify_hub::hub().post(notification);
        }
        Ok(())
    })
}

/// This phone's calls changed, or whether it may act on them.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeCallsChanged(
    mut env: JNIEnv,
    _this: JObject,
    available: jboolean,
    reason: JString,
    bluetooth_name: JString,
    calls: JString,
) {
    guard(&mut env, (), |env| {
        let optional = |env: &mut JNIEnv, s: &JString| -> jni::errors::Result<Option<String>> {
            if s.is_null() {
                Ok(None)
            } else {
                Ok(Some(env.get_string(s)?.into()))
            }
        };
        let reason = optional(env, &reason)?;
        let bluetooth_name = optional(env, &bluetooth_name)?;
        let json: String = env.get_string(&calls)?.into();
        let calls = crate::calls::calls(&json).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "unreadable call report from the phone");
            Vec::new()
        });
        cursio_core::calls_hub::hub().set_phone(
            cursio_protocol::calls::CallsMsg::Status {
                available: available != JNI_FALSE,
                reason,
                bluetooth_name,
            },
            calls,
        );
        Ok(())
    })
}

/// A notification on this phone went away: dismissed, read, or withdrawn.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeNotificationRemoved(
    mut env: JNIEnv,
    _this: JObject,
    key: JString,
) {
    guard(&mut env, (), |env| {
        let key: String = env.get_string(&key)?.into();
        cursio_core::notify_hub::hub().remove(&key);
        Ok(())
    })
}

/// The phone's messages changed; every desktop reading them is told to look
/// again.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeSmsChanged(
    mut env: JNIEnv,
    _this: JObject,
) {
    guard(&mut env, (), |_env| {
        cursio_core::messages_hub::hub().changed();
        Ok(())
    })
}

/// MediaStore changed — a photo taken, a screenshot, one deleted — so a peer
/// browsing this phone's gallery should ask again.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeGalleryChanged(
    mut env: JNIEnv,
    _this: JObject,
) {
    guard(&mut env, (), |_env| {
        cursio_core::gallery_hub::hub().changed();
        Ok(())
    })
}

/// The screen came on unlocked (`in_use`), or went off or locked; see
/// `cursio_core::at_phone`.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativePhoneInUse(
    mut env: JNIEnv,
    _class: JClass,
    in_use: jboolean,
) {
    guard(&mut env, (), |_env| {
        cursio_core::at_phone::set_in_use(in_use != JNI_FALSE);
        Ok(())
    })
}
