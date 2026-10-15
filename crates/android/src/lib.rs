//! The JVM's way into the mesh core.
//!
//! Android cannot run a daemon; it runs an *app*, and the app is Kotlin. This
//! crate is the seam: a `cdylib` the JVM opens with `System.loadLibrary`, whose
//! exported functions are the only thing Kotlin ever calls. Everything above
//! this file is Android — a foreground service, permissions, a notification.
//! Everything below it is the same `cursio-core` the desktop runs.
//!
//! ## The node lives here, not in Kotlin
//!
//! The alternative — Kotlin owning a socket and calling down per message —
//! would put the session loop on the JVM side, where it would be subject to
//! garbage collection pauses mid-handshake and would have to reimplement
//! everything `mesh.rs` already does. Instead the whole node runs on a Rust
//! thread with its own Tokio runtime, and Kotlin only starts it, stops it, and
//! asks what it can see. That keeps the phone running the *same* mesh code as
//! every other peer, which is the entire point of the port.
//!
//! ## Why every entry point catches its own errors
//!
//! A panic unwinding into JVM frames is undefined behaviour, not an exception —
//! the process dies with no Java stack trace and nothing in logcat that names
//! the cause. Each `extern "system"` function below therefore catches unwinds
//! at the boundary and turns a failure into a Java exception or a sentinel the
//! caller can read. This is the one place in the codebase where catching a
//! panic is correct rather than a smell.
//!
//! ## Threading
//!
//! Sessions are `!Send` by design (see `mesh.rs`), so the node runs on a
//! current-thread runtime inside a `LocalSet` on one dedicated thread. Kotlin
//! calls arrive on arbitrary JVM threads and only ever touch the handle, never
//! the session state.

use std::sync::Mutex;

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jfloat, jint, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

mod node;

use node::NodeHandle;

/// The running node, if `nativeStart` has been called and `nativeStop` has not.
///
/// A single global rather than a handle returned to Kotlin: Android will
/// happily start a service twice, and a second node would bind a second
/// listener and announce a second copy of this machine on the LAN. One slot
/// makes the second start a no-op that says so.
static NODE: Mutex<Option<NodeHandle>> = Mutex::new(None);

/// The mic gate the app last chose, kept across a stopped node so the next
/// start applies it — the app restores its saved choice before or after
/// starting, depending on how the service came up.
static MIC_GATE: Mutex<String> = Mutex::new(String::new());

/// Point Cursio at the directory Android gave this app, and start logging.
///
/// Must be the first call: identity, the trust store and the log directory are
/// all read relative to this, and none of them can be derived on Android — the
/// writable path is chosen by the system per install and reaches the app only
/// through `Context.getFilesDir()`.
///
/// Safe to call more than once with the same directory; a second call with a
/// different one is refused rather than splitting state in two.
///
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
        cursio_protocol::paths::set_root(&dir)?;
        // Logs beside the rest, and mirrored into logcat: a file under the
        // app's private directory is not something the user can reach without
        // a debugger, and during bring-up "no output" and "never started" look
        // identical.
        cursio_log::set_log_dir(std::path::Path::new(&dir).join("data").join("logs"));
        init_logcat();
        tracing::info!(dir = %dir, "cursio native initialised");
        Ok(JNI_TRUE)
    })
}

/// Start the node: listen, announce, and dial known peers.
///
/// Returns false when a node is already running, which is the ordinary case
/// when Android restarts a service that was never really gone.
///
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
        let mut slot = NODE.lock().expect("node lock");
        if slot.is_some() {
            tracing::info!("start ignored: a node is already running");
            return Ok(JNI_FALSE);
        }
        let port = u16::try_from(port).unwrap_or(0);
        let gate = MIC_GATE.lock().expect("mic gate lock").clone();
        let gate = if gate.is_empty() {
            "auto".to_owned()
        } else {
            gate
        };
        *slot = Some(NodeHandle::start(name, port, &gate)?);
        Ok(JNI_TRUE)
    })
}

/// Stop the node and wait for its thread to finish.
///
/// Blocking is deliberate: the caller is a service being torn down, and
/// returning before the listener is closed would let Android start the next
/// instance onto a port this one still holds.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeStop(
    mut env: JNIEnv,
    _class: JClass,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |_env| {
        let handle = NODE.lock().expect("node lock").take();
        match handle {
            Some(node) => {
                node.stop();
                Ok(JNI_TRUE)
            }
            None => Ok(JNI_FALSE),
        }
    })
}

/// Whether a node is running right now.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeIsRunning(
    mut env: JNIEnv,
    _class: JClass,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |_env| {
        let running = NODE.lock().expect("node lock").is_some();
        Ok(if running { JNI_TRUE } else { JNI_FALSE })
    })
}

/// This node's peer id, for showing the user which machine this is.
///
/// Returns an empty string on failure rather than throwing: the caller is a
/// UI label, and an exception there would crash a screen over a cosmetic value.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativePeerId<'a>(
    env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    let id = std::panic::catch_unwind(|| {
        cursio_core::NodeIdentity::load_or_create()
            .map(|i| i.peer_id.to_string())
            .unwrap_or_default()
    })
    .unwrap_or_default();
    env.new_string(id).unwrap_or_else(|_| JString::default())
}

/// A one-line summary of what this node can see, for the service notification.
///
/// Deliberately a string rather than a structured object: the JNI cost of
/// building a Java list of peer objects is real, and the only consumer today is
/// a notification line. When Kotlin needs the peers themselves, this becomes a
/// proper call rather than a parsed string.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeStatusLine<'a>(
    env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    let line = std::panic::catch_unwind(|| match NODE.lock().expect("node lock").as_ref() {
        Some(node) => node.status_line(),
        None => "stopped".to_string(),
    })
    .unwrap_or_else(|_| "unavailable".to_string());
    env.new_string(line).unwrap_or_else(|_| JString::default())
}

/// Open the pairing window with a code shown on another machine.
///
/// Returns "" on success, else the reason, for the app to show beside the
/// field — a typo in a code is not an exception.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeEnterCode<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    code: JString<'a>,
) -> JString<'a> {
    let reply = guard(&mut env, String::new(), |env| {
        let code: String = env.get_string(&code)?.into();
        Ok(match NODE.lock().expect("node lock").as_ref() {
            Some(node) => match node.enter_code(&code) {
                Ok(()) => String::new(),
                Err(e) => e.to_string(),
            },
            None => "Cursio is not running".to_owned(),
        })
    });
    new_string(&env, reply)
}

/// Open the pairing window with a fresh code for this phone to show; "" when
/// no node is running.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeShowCode<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    let code = guard(&mut env, String::new(), |_env| {
        Ok(NODE
            .lock()
            .expect("node lock")
            .as_ref()
            .map(|node| node.show_code())
            .unwrap_or_default())
    });
    new_string(&env, code)
}

/// Close the pairing window.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeCancelPairing(
    mut env: JNIEnv,
    _class: JClass,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |_env| {
        Ok(match NODE.lock().expect("node lock").as_ref() {
            Some(node) => {
                node.cancel_pairing();
                JNI_TRUE
            }
            None => JNI_FALSE,
        })
    })
}

/// Everything the app shows, as JSON; `{"running":false}` when stopped.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeStatusJson<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    let json = guard(&mut env, String::from("{\"running\":false}"), |_env| {
        Ok(match NODE.lock().expect("node lock").as_ref() {
            Some(node) => node.status_json(),
            None => "{\"running\":false}".to_owned(),
        })
    });
    new_string(&env, json)
}

/// Set one incoming stream's volume (0.0–1.5) and mute on this phone.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeSetStreamVolume(
    mut env: JNIEnv,
    _class: JClass,
    id: jint,
    volume: jfloat,
    muted: jboolean,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |_env| {
        let set = NODE
            .lock()
            .expect("node lock")
            .as_ref()
            .is_some_and(|node| node.set_stream_volume(id as u32, volume, muted != JNI_FALSE));
        Ok(if set { JNI_TRUE } else { JNI_FALSE })
    })
}

/// Set how this phone's microphone is gated: "auto", "off" or a dBFS level.
/// Returns "" on success, else why the value was refused.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeSetMicGate<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    value: JString<'a>,
) -> JString<'a> {
    let reply = guard(&mut env, String::new(), |env| {
        let value: String = env.get_string(&value)?.into();
        if let Err(e) = value.parse::<cursio_core::audio_gate::MicGate>() {
            return Ok(e.to_string());
        }
        *MIC_GATE.lock().expect("mic gate lock") = value.clone();
        Ok(match NODE.lock().expect("node lock").as_ref() {
            Some(node) => node
                .set_mic_gate(&value)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default(),
            None => String::new(),
        })
    });
    new_string(&env, reply)
}

fn new_string<'a>(env: &JNIEnv<'a>, value: String) -> JString<'a> {
    env.new_string(value).unwrap_or_else(|_| JString::default())
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
