//! Calling up into the app's Kotlin from the node's own threads.
//!
//! Several things only the JVM can do — keep a shared file in Downloads, press
//! a notification's button — are asked for from threads the JVM did not
//! start. Those need the JVM itself and `CursioNative`'s class, both taken in
//! `nativeInit`: on such a thread `FindClass` searches the system class
//! loader, which has never heard of this app's classes.
//!
//! The class is looked up by name rather than taken from `nativeInit`'s
//! second argument: `CursioNative` is a Kotlin `object`, so its non-`@JvmStatic`
//! natives receive the `INSTANCE`, not the class. Handing that instance to
//! `GetMethodID` as a `jclass` is a segfault on the first call up.
//!
//! Every method called this way is reachable only from here, so R8 keeps it
//! only because `consumer-rules.pro` names it.

use std::sync::OnceLock;

use jni::objects::{GlobalRef, JClass, JString};
use jni::{JNIEnv, JavaVM};

static JVM: OnceLock<(JavaVM, GlobalRef)> = OnceLock::new();

const NATIVE_CLASS: &str = "dev/cursio/mesh/CursioNative";

/// Keep the JVM and `CursioNative`'s class for calls from the node's threads.
/// Must run on a thread inside a native call from the app, whose class loader
/// is the one `FindClass` then uses.
pub(crate) fn remember(env: &mut JNIEnv) -> jni::errors::Result<()> {
    if JVM.get().is_none() {
        let vm = env.get_java_vm()?;
        let class = env.find_class(NATIVE_CLASS)?;
        let class = env.new_global_ref(class)?;
        let _ = JVM.set((vm, class));
    }
    Ok(())
}

/// Run `call` with this thread attached and `CursioNative`'s class, answering
/// what went wrong in words: a Java exception is cleared and described rather
/// than left pending.
pub(crate) fn with_native<T>(
    call: impl FnOnce(&mut JNIEnv, &JClass) -> jni::errors::Result<T>,
) -> Result<T, String> {
    let Some((vm, class)) = JVM.get() else {
        return Err("the phone's app is not initialised".into());
    };
    let result = (|| {
        let mut env = vm.attach_current_thread_as_daemon()?;
        call(&mut env, class.as_obj().into())
    })();
    match result {
        Ok(value) => Ok(value),
        Err(jni::errors::Error::JavaException) => Err(take_exception(vm)),
        Err(e) => Err(e.to_string()),
    }
}

/// Clear the pending exception and say what it was. Left pending, it makes the
/// next JNI call on this thread abort the process.
fn take_exception(vm: &JavaVM) -> String {
    let Ok(mut env) = vm.attach_current_thread_as_daemon() else {
        return "a Java exception".into();
    };
    let Ok(thrown) = env.exception_occurred() else {
        return "a Java exception".into();
    };
    let _ = env.exception_clear();
    let described = env
        .call_method(&thrown, "toString", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .and_then(|s| Ok(String::from(env.get_string(&JString::from(s))?)));
    let _ = env.exception_clear();
    described.unwrap_or_else(|_| "a Java exception".into())
}
