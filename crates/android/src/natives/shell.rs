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

/// Run shell source `command` through the debugging port at `addr`; answers
/// `ok\t` and the output, or `error\t` and why.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeShellExec<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    addr: JString<'a>,
    command: JString<'a>,
) -> JString<'a> {
    let answer = guard(
        &mut env,
        "error\tthe phone's node failed".to_string(),
        |env| {
            let addr: String = env.get_string(&addr)?.into();
            let command: String = env.get_string(&command)?.into();
            Ok(match crate::shell::exec(&addr, &command) {
                Ok(out) => format!("ok\t{out}"),
                Err(why) => format!("error\t{why}"),
            })
        },
    );
    env.new_string(answer)
        .unwrap_or_else(|_| JString::default())
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
