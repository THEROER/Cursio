//! The phone's media sessions: their list, commands from a computer, covers,
//! and where the phone's own sound plays.

use crate::guard;
use jni::objects::{JClass, JObject, JString};
#[cfg(target_os = "android")]
use jni::sys::jint;
use jni::JNIEnv;

/// The phone's media sessions changed; `json` is `MediaBridge`'s list.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeMediaChanged(
    mut env: JNIEnv,
    _this: JObject,
    json: JString,
) {
    guard(&mut env, (), |env| {
        let json: String = env.get_string(&json)?.into();
        crate::media::changed(&json);
        Ok(())
    })
}

/// A button on a computer's player, pressed in its notification here.
/// Answers why it went nowhere, or `""`.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeMediaRemoteCommand<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    peer: JString<'a>,
    player: JString<'a>,
    command: JString<'a>,
    arg: jni::sys::jlong,
) -> JString<'a> {
    let answer = guard(&mut env, "the phone's node failed".to_string(), |env| {
        let peer: String = env.get_string(&peer)?.into();
        let player: String = env.get_string(&player)?.into();
        let command: String = env.get_string(&command)?.into();
        let Ok(peer) = peer.parse::<cursio_protocol::PeerId>() else {
            return Ok(format!("{peer} is not a machine id"));
        };
        let Some(command) = crate::media::command_from(&command, arg) else {
            return Ok(format!("{command} is not a player button"));
        };
        Ok(cursio_core::media_hub::hub()
            .command(&peer, &player, command)
            .err()
            .unwrap_or_default())
    });
    env.new_string(answer)
        .unwrap_or_else(|_| JString::default())
}

/// "Disconnect" on a computer's player: hide it until it plays something else.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeMediaDismiss(
    mut env: JNIEnv,
    _this: JObject,
    peer: JString,
    player: JString,
) {
    guard(&mut env, (), |env| {
        let peer: String = env.get_string(&peer)?.into();
        let player: String = env.get_string(&player)?.into();
        if let Ok(peer) = peer.parse::<cursio_protocol::PeerId>() {
            cursio_core::media_hub::hub().dismiss(&peer, &player);
        }
        Ok(())
    })
}

/// A computer player's cover by the decimal hash its list named, or an empty
/// array before it has arrived.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeMediaArt<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    art: JString<'a>,
) -> jni::objects::JByteArray<'a> {
    let bytes = guard(&mut env, Vec::new(), |env| {
        let art: String = env.get_string(&art)?.into();
        Ok(art
            .parse::<u64>()
            .ok()
            .and_then(|a| cursio_core::media_hub::hub().art(a))
            .map(|data| data.1.clone())
            .unwrap_or_default())
    });
    env.byte_array_from_slice(&bytes)
        .unwrap_or_else(|_| jni::objects::JByteArray::default())
}

/// Hand in `len` samples of what this phone is playing — interleaved stereo
/// f32 at 48 kHz from AudioPlaybackCapture — for the peer device it is routed
/// to.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativePushPlayback(
    mut env: JNIEnv,
    _class: JClass,
    samples: jni::objects::JFloatArray,
    len: jint,
) {
    guard(&mut env, (), |env| {
        let len = usize::try_from(len).unwrap_or(0);
        let mut buf = vec![0f32; len];
        env.get_float_array_region(&samples, 0, &mut buf)?;
        cursio_audio::android_backend::push_playback(&buf);
        Ok(())
    })
}
