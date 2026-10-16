//! The phone's mesh node, and the JVM's way into it.
//!
//! Android cannot run a daemon; it runs an *app*. The Cursio app on a phone is
//! the same Tauri app as on a desktop, built for Android, and this crate is
//! what it runs in place of a separate `cursio mesh` process: a node in the
//! app's own process, which the app's UI reaches through [`control`] — the
//! very [`NodeState`] a desktop's control socket serves.
//!
//! What remains of the JVM's part is keeping that process alive. A foreground
//! service (Kotlin, in the app's Android plugin) holds it, and the service can
//! start without any window — after a reboot, after an update — so it starts
//! the node through the `extern "system"` functions below rather than waiting
//! for the UI.
//!
//! ## The node lives here, not in Kotlin
//!
//! The alternative — Kotlin owning a socket and calling down per message —
//! would put the session loop on the JVM side, where it would be subject to
//! garbage collection pauses mid-handshake and would have to reimplement
//! everything `mesh.rs` already does. Instead the whole node runs on a Rust
//! thread with its own Tokio runtime, which keeps the phone running the *same*
//! mesh code as every other peer.
//!
//! ## Why every entry point catches its own errors
//!
//! A panic unwinding into JVM frames is undefined behaviour, not an exception —
//! the process dies with no Java stack trace and nothing in logcat that names
//! the cause. Each `extern "system"` function below therefore catches unwinds
//! at the boundary and turns a failure into a Java exception or a sentinel the
//! caller can read. This is the one place in the codebase where catching a
//! panic is correct rather than a smell.

use std::sync::Mutex;

use cursio_core::control::NodeState;
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

mod node;

use node::NodeHandle;

/// The running node, if one was started and not stopped.
///
/// A single global rather than a handle passed around: Android will happily
/// start a service twice, and the UI may come up before or after it, and a
/// second node would bind a second listener and announce a second copy of
/// this machine on the LAN. One slot makes the second start a no-op.
static NODE: Mutex<Option<NodeHandle>> = Mutex::new(None);

fn slot() -> std::sync::MutexGuard<'static, Option<NodeHandle>> {
    NODE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Point Cursio at the directory Android gave this app, and start logging.
///
/// Must come first: identity, the trust store, the config and the logs all
/// resolve against it, and none of them can be derived on Android — the
/// writable path is chosen by the system per install and reaches the app only
/// through `Context.getFilesDir()`. Safe to repeat with the same directory; a
/// different one is refused rather than splitting state in two.
pub fn init(files_dir: &str) -> anyhow::Result<()> {
    cursio_protocol::paths::set_root(files_dir)?;
    // Logs beside the rest, and mirrored into logcat: a file under the app's
    // private directory is not something the user can reach without a
    // debugger, and during bring-up "no output" and "never started" look
    // identical.
    cursio_log::set_log_dir(std::path::Path::new(files_dir).join("data").join("logs"));
    init_logcat();
    tracing::info!(dir = %files_dir, "cursio native initialised");
    Ok(())
}

/// Start the node, unless one runs. True when this call started it.
pub fn start(display_name: String, port: u16) -> anyhow::Result<bool> {
    let mut slot = slot();
    if slot.is_some() {
        tracing::info!("start ignored: a node is already running");
        return Ok(false);
    }
    *slot = Some(NodeHandle::start(display_name, port)?);
    Ok(true)
}

/// Stop the node and wait for its thread. True when one was running.
///
/// Blocking is deliberate: returning before the listener is closed would let
/// Android start the next instance onto a port this one still holds.
pub fn stop() -> bool {
    let handle = slot().take();
    match handle {
        Some(node) => {
            node.stop();
            true
        }
        None => false,
    }
}

pub fn is_running() -> bool {
    slot().is_some()
}

/// The running node's control state, for the app to send commands to.
pub fn control() -> Option<NodeState> {
    slot().as_ref().map(NodeHandle::control)
}

/// The peers the touchpad can drive: every live link, by id and name.
pub fn input_targets() -> Vec<(cursio_protocol::PeerId, String)> {
    slot()
        .as_ref()
        .map(NodeHandle::input_targets)
        .unwrap_or_default()
}

/// A channel into one peer's session for the touchpad's events.
pub fn input_to(
    peer: &cursio_protocol::PeerId,
) -> Option<tokio::sync::mpsc::UnboundedSender<cursio_core::session::CapturedInput>> {
    slot().as_ref().and_then(|node| node.input_to(peer))
}

/// One line for the service notification.
pub fn status_line() -> String {
    slot()
        .as_ref()
        .map_or_else(|| "stopped".to_string(), NodeHandle::status_line)
}

/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeInit(
    mut env: JNIEnv,
    _class: JClass,
    files_dir: JString,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |env| {
        let dir: String = env.get_string(&files_dir)?.into();
        init(&dir)?;
        Ok(JNI_TRUE)
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

/// Run `body`, turning any failure into a Java exception and `fallback`.
///
/// Both halves matter. An `Err` becomes a `RuntimeException` Kotlin can catch
/// and show; a *panic* is caught here because letting one unwind into JVM
/// frames is undefined behaviour — the process dies with no Java stack trace
/// and nothing in logcat naming the cause, which is the worst possible failure
/// mode during bring-up.
fn guard<T>(
    env: &mut JNIEnv,
    fallback: T,
    body: impl FnOnce(&mut JNIEnv) -> anyhow::Result<T>,
) -> T {
    // The borrow is re-taken inside rather than moved in, so `env` is still
    // available afterwards to throw with. `AssertUnwindSafe` is sound here for
    // the same reason: a panic in `body` leaves no half-written Rust state,
    // and reading arguments or throwing afterwards is fine.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(env)));
    match result {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "native call failed");
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            fallback
        }
        Err(_) => {
            tracing::error!("native call panicked");
            let _ = env.throw_new("java/lang/RuntimeException", "cursio native panicked");
            fallback
        }
    }
}

/// Send `tracing` output to logcat as well as the log file.
///
/// Idempotent: a second `init` is an error the subscriber reports, and a
/// service restarted by Android will call this again.
#[cfg(target_os = "android")]
fn init_logcat() {
    use tracing_subscriber::prelude::*;
    let layer = match tracing_android::layer("cursio") {
        Ok(layer) => layer,
        Err(_) => return,
    };
    let _ = tracing_subscriber::registry()
        .with(layer)
        .with(tracing_subscriber::EnvFilter::new("info"))
        .try_init();
}

/// No logcat off Android — the crate still builds on a desktop so its tests
/// and clippy run in the ordinary workspace check.
#[cfg(not(target_os = "android"))]
fn init_logcat() {}
