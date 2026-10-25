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
use jni::JNIEnv;

mod natives;

mod bt;
mod calls;
mod gallery;
mod jvm;
mod media;
mod messages;
mod node;
mod notify;
#[cfg(target_os = "android")]
mod phone_audio;
mod screen;
mod share;

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
    #[cfg(target_os = "android")]
    phone_audio::restore();
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
pub fn input_to(peer: &cursio_protocol::PeerId) -> Option<cursio_core::session::InputSender> {
    slot().as_ref().and_then(|node| node.input_to(peer))
}

/// Peer devices this phone's playback can be sent to.
#[cfg(target_os = "android")]
pub use cursio_audio::android_backend::{playback_targets, PlaybackTarget};
#[cfg(target_os = "android")]
pub use phone_audio::{
    awaited_playback, playback_volume, remembered_playback, resumable_playback, route_playback,
    set_playback_volume,
};

/// One line for the service notification.
pub fn status_line() -> String {
    slot()
        .as_ref()
        .map_or_else(|| "stopped".to_string(), NodeHandle::status_line)
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
        .with(tracing_subscriber::EnvFilter::new(log_filter()))
        .try_init();
}

/// The filter logcat gets: `info`, unless `debug.cursio.log` says otherwise.
///
/// A release APK is not debuggable, so its log file cannot be read and an
/// environment variable cannot be set; whatever happens below `info` — a
/// peer seen, a dial that failed — was simply invisible on the phone where it
/// mattered. A `debug.*` property is one the shell may set, so
/// `adb shell setprop debug.cursio.log debug` and a restart of the app raise
/// it without a rebuild.
#[cfg(target_os = "android")]
fn log_filter() -> String {
    let mut value = [0u8; 92];
    // SAFETY: the name is NUL-terminated and the buffer is PROP_VALUE_MAX (92)
    // bytes, the most the call writes.
    let len = unsafe {
        libc::__system_property_get(c"debug.cursio.log".as_ptr(), value.as_mut_ptr().cast())
    };
    match std::str::from_utf8(&value[..len.max(0) as usize]) {
        Ok(filter) if !filter.trim().is_empty() => filter.trim().to_string(),
        _ => "info".to_string(),
    }
}

/// No logcat off Android — the crate still builds on a desktop so its tests
/// and clippy run in the ordinary workspace check.
#[cfg(not(target_os = "android"))]
fn init_logcat() {}
