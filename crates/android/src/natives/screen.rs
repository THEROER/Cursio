//! The phone's screen capture, as the encoder reports it, and the computer the
//! user chose to share it with.

use crate::guard;
use jni::objects::JObject;
use jni::sys::{jboolean, jint, JNI_FALSE};
use jni::JNIEnv;

/// The screen capture is up, at `width` x `height`, or changed size.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeScreenStarted(
    mut env: JNIEnv,
    _this: JObject,
    width: jint,
    height: jint,
) {
    guard(&mut env, (), |_| {
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            anyhow::bail!("a screen of {width} x {height} pixels is not a size");
        };
        cursio_core::phone_screen::started(width, height);
        Ok(())
    })
}

/// One unit from the screen's encoder: `size` bytes at `offset` of the
/// codec's direct output buffer, read before the buffer is released.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeScreenUnit(
    mut env: JNIEnv,
    _this: JObject,
    buffer: jni::objects::JByteBuffer,
    offset: jint,
    size: jint,
    config: jboolean,
    key: jboolean,
) {
    guard(&mut env, (), |env| {
        let base = env.get_direct_buffer_address(&buffer)?;
        let capacity = env.get_direct_buffer_capacity(&buffer)?;
        let (Ok(offset), Ok(size)) = (usize::try_from(offset), usize::try_from(size)) else {
            anyhow::bail!("an encoder unit at {offset} of {size} bytes is not a range");
        };
        if offset.checked_add(size).is_none_or(|end| end > capacity) {
            anyhow::bail!("an encoder unit ran past its buffer ({offset} + {size} > {capacity})");
        }
        // SAFETY: the range was checked against the buffer's capacity, and
        // the codec does not reuse the buffer until Kotlin releases it, which
        // is after this call returns.
        let data = unsafe { std::slice::from_raw_parts(base.add(offset), size) };
        cursio_core::phone_screen::unit(data, config != JNI_FALSE, key != JNI_FALSE);
        Ok(())
    })
}

/// The screen capture ended on the phone.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeScreenStopped(
    mut env: JNIEnv,
    _this: JObject,
) {
    guard(&mut env, (), |_| {
        cursio_core::phone_screen::stopped();
        Ok(())
    })
}

/// The phone's user shared the screen from the phone: show it on the
/// computers chosen in the window, or the one the user was at last. Answers
/// how many watch.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeScreenShareChosen(
    mut env: JNIEnv,
    _this: JObject,
) -> jint {
    guard(&mut env, 0, |_| {
        let count = cursio_core::phone_screen::share_with_chosen();
        Ok(jint::try_from(count).unwrap_or(jint::MAX))
    })
}
