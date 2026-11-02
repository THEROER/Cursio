//! This phone's own shell: pairing it from the window, and asking whether it
//! is paired. See `crate::shell`.

use crate::guard;
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

/// Pair with the pairing service at `addr` using `code`; answers why it
/// failed, or `""`. Blocks on the network, so Kotlin calls it off its main
/// thread.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeShellPair<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    addr: JString<'a>,
    code: JString<'a>,
) -> JString<'a> {
    let answer = guard(&mut env, "the phone's node failed".to_string(), |env| {
        let addr: String = env.get_string(&addr)?.into();
        let code: String = env.get_string(&code)?.into();
        Ok(crate::shell::pair(&addr, &code).err().unwrap_or_default())
    });
    env.new_string(answer)
        .unwrap_or_else(|_| JString::default())
}

/// Run shell source `command` as this phone's `shell`; answers `ok\t` and
/// the output, or `error\t` and why. Blocks, a sweep for the port included.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeShellExec<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    command: JString<'a>,
) -> JString<'a> {
    let answer = guard(
        &mut env,
        "error\tthe phone's node failed".to_string(),
        |env| {
            let command: String = env.get_string(&command)?.into();
            Ok(match crate::shell::run(&command) {
                Ok(out) => format!("ok\t{out}"),
                Err(why) => format!("error\t{why}"),
            })
        },
    );
    env.new_string(answer)
        .unwrap_or_else(|_| JString::default())
}

/// A line from the shell's Kotlin half into the node's log, which computers
/// read without ADB: what mDNS saw and what pairing did happened in logcat
/// only, out of sight from a computer.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeShellLog<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    warn: jboolean,
    message: JString<'a>,
) {
    guard(&mut env, (), |env| {
        let message: String = env.get_string(&message)?.into();
        if warn != JNI_FALSE {
            tracing::warn!(target: "cursio_android::shell", "{message}");
        } else {
            tracing::info!(target: "cursio_android::shell", "{message}");
        }
        Ok(())
    })
}

/// Whether this phone's key was paired with its wireless debugging.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeShellPaired<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |_| {
        Ok(match crate::shell::paired() {
            true => JNI_TRUE,
            false => JNI_FALSE,
        })
    })
}
