//! The phone's clipboard to a computer: where a tap sends it, and sending it.

use crate::guard;
use jni::objects::{JClass, JString};
use jni::JNIEnv;

/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeClipboardDestination<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    let json = guard(
        &mut env,
        r#"{"nowhere":"the phone's node failed"}"#.to_string(),
        |_env| Ok(crate::clipboard::destination_json()),
    );
    env.new_string(json).unwrap_or_else(|_| JString::default())
}

/// Offer `text` to `peer`; answers `"sent:<name>"`, otherwise why not.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeSendClipboard<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    peer: JString<'a>,
    text: JString<'a>,
) -> JString<'a> {
    let answer = guard(&mut env, "the phone's node failed".to_string(), |env| {
        let peer: String = env.get_string(&peer)?.into();
        let text: String = env.get_string(&text)?.into();
        Ok(match crate::clipboard::send(&peer, text) {
            Ok(name) => format!("sent:{name}"),
            Err(why) => why,
        })
    });
    env.new_string(answer)
        .unwrap_or_else(|_| JString::default())
}
