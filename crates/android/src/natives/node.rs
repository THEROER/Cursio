//! The node's lifecycle as the service drives it: where it keeps its files,
//! starting and stopping it, and what the notification says about it.

use crate::guard;
use crate::init;
use crate::is_running;
use crate::start;
use crate::status_line;
use crate::stop;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jint, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeInit(
    mut env: JNIEnv,
    _this: JObject,
    files_dir: JString,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |env| {
        let dir: String = env.get_string(&files_dir)?.into();
        init(&dir)?;
        crate::jvm::remember(env)?;
        Ok(JNI_TRUE)
    })
}

/// Android lost the default network: sessions leave their network paths at
/// once, for a Bluetooth standby where there is one.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeNetworkLost(
    mut env: JNIEnv,
    _this: JObject,
) {
    guard(&mut env, (), |_env| {
        cursio_transport::resume::network_lost();
        Ok(())
    })
}

/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    display_name: JString,
    port: jint,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |env| {
        let name: String = env.get_string(&display_name)?.into();
        let started = start(name, u16::try_from(port).unwrap_or(0))?;
        Ok(if started { JNI_TRUE } else { JNI_FALSE })
    })
}

/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeStop(
    mut env: JNIEnv,
    _class: JClass,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |_env| {
        Ok(if stop() { JNI_TRUE } else { JNI_FALSE })
    })
}

/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeIsRunning(
    mut env: JNIEnv,
    _class: JClass,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |_env| {
        Ok(if is_running() { JNI_TRUE } else { JNI_FALSE })
    })
}

/// Whether the node is moving data now — a stream, a transfer, an answer
/// owed — so the service holds the CPU and radio awake only then.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeBusy(
    mut env: JNIEnv,
    _class: JClass,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |_env| {
        Ok(if cursio_core::modules_busy() {
            JNI_TRUE
        } else {
            JNI_FALSE
        })
    })
}

/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeStatusLine<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    let line = guard(
        &mut env,
        "unavailable".to_string(),
        |_env| Ok(status_line()),
    );
    env.new_string(line).unwrap_or_else(|_| JString::default())
}

/// Hand in this phone's power and network state, which only the JVM can read.
///
/// `battery` is a percentage or -1; `power` is `battery`, `charging`, `mains`
/// or empty; `network` is `wifi`, `cellular`, `wired`, `unknown` or empty (no
/// connection); `signal_dbm` is the Wi-Fi RSSI or `Int.MIN_VALUE`. Anything
/// the caller could not read is passed as its "unknown" and arrives at peers
/// as "cannot say", never as a zero.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeReportStatus(
    mut env: JNIEnv,
    _class: JClass,
    battery: jint,
    power: JString,
    network: JString,
    signal_dbm: jint,
) -> jboolean {
    use cursio_protocol::{NetworkKind, PowerSource};
    guard(&mut env, JNI_FALSE, |env| {
        let power: String = env.get_string(&power)?.into();
        let network: String = env.get_string(&network)?.into();
        cursio_core::node_status::report(
            u8::try_from(battery).ok(),
            match power.as_str() {
                "battery" => Some(PowerSource::Battery),
                "charging" => Some(PowerSource::Charging),
                "mains" => Some(PowerSource::Mains),
                _ => None,
            },
            match network.as_str() {
                "wifi" => Some(NetworkKind::Wifi),
                "cellular" => Some(NetworkKind::Cellular),
                "wired" => Some(NetworkKind::Wired),
                "unknown" => Some(NetworkKind::Unknown),
                _ => None,
            },
            i16::try_from(signal_dbm).ok(),
        );
        Ok(JNI_TRUE)
    })
}
