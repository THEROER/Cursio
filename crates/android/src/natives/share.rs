//! Sharing from the phone's share sheet: which computers can receive, and
//! sending to one.

use crate::guard;
use jni::objects::{JClass, JString};
use jni::JNIEnv;

/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeShareTargets<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    let json = guard(&mut env, "[]".to_string(), |_env| {
        Ok(crate::share::targets_json())
    });
    env.new_string(json).unwrap_or_else(|_| JString::default())
}

/// Send what the share sheet handed over; answers `""` when it is on its way,
/// otherwise why not.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeShare<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    peer: JString<'a>,
    text: JString<'a>,
    path: JString<'a>,
    name: JString<'a>,
    mime: JString<'a>,
    action: JString<'a>,
) -> JString<'a> {
    let answer = guard(&mut env, "the phone's node failed".to_string(), |env| {
        let mut read = |s: &JString| -> anyhow::Result<String> { Ok(env.get_string(s)?.into()) };
        let (peer, text, path, name, mime, action) = (
            read(&peer)?,
            read(&text)?,
            read(&path)?,
            read(&name)?,
            read(&mime)?,
            read(&action)?,
        );
        Ok(
            crate::share::send(&peer, &text, &path, &name, &mime, &action)
                .err()
                .unwrap_or_default(),
        )
    });
    env.new_string(answer)
        .unwrap_or_else(|_| JString::default())
}
