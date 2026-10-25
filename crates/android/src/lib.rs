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
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jint, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

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
        jvm::remember(env)?;
        Ok(JNI_TRUE)
    })
}

/// One notification posted on this phone, for the desktop the user is at.
///
/// `icon` is a PNG or null; `kind` is `call`, `message` or anything else;
/// `action_titles` and `action_replies` are the notification's buttons in
/// order, the second saying which take text.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeNotificationPosted(
    mut env: JNIEnv,
    _this: JObject,
    key: JString,
    app_id: JString,
    app_name: JString,
    title: JString,
    text: JString,
    icon: jni::objects::JByteArray,
    when_ms: jni::sys::jlong,
    kind: JString,
    action_titles: jni::objects::JObjectArray,
    action_replies: jni::objects::JBooleanArray,
    answer: jni::sys::jint,
    decline: jni::sys::jint,
    hang_up: jni::sys::jint,
) {
    guard(&mut env, (), |env| {
        let mut read = |s: &JString| -> anyhow::Result<String> { Ok(env.get_string(s)?.into()) };
        let (key, app_id, app_name, title, text, kind) = (
            read(&key)?,
            read(&app_id)?,
            read(&app_name)?,
            read(&title)?,
            read(&text)?,
            read(&kind)?,
        );
        let icon = if icon.is_null() {
            None
        } else {
            Some(env.convert_byte_array(&icon)?)
        };
        let count = env.get_array_length(&action_titles)?;
        let mut replies = vec![0u8; usize::try_from(count).unwrap_or(0)];
        env.get_boolean_array_region(&action_replies, 0, &mut replies)?;
        let mut actions = Vec::new();
        for i in 0..count {
            let title: JString = env.get_object_array_element(&action_titles, i)?.into();
            let title: String = env.get_string(&title)?.into();
            actions.push((title, replies[i as usize] != 0));
        }
        let telephony = kind == "dialer_call";
        let notification = notify::notification(
            key, app_id, app_name, title, text, icon, when_ms, &kind, actions,
        );
        if notification.kind == cursio_protocol::notify::NotifyKind::Call {
            // -1 from Kotlin is "no such button".
            let role = |i: jni::sys::jint| u8::try_from(i).ok();
            let roles = cursio_core::notify_hub::CallRoles {
                answer: role(answer),
                decline: role(decline),
                hang_up: role(hang_up),
            };
            cursio_core::notify_hub::hub().post_call(notification, telephony, roles);
        } else {
            cursio_core::notify_hub::hub().post(notification);
        }
        Ok(())
    })
}

/// This phone's calls changed, or whether it may act on them.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeCallsChanged(
    mut env: JNIEnv,
    _this: JObject,
    available: jboolean,
    reason: JString,
    bluetooth_name: JString,
    calls: JString,
) {
    guard(&mut env, (), |env| {
        let optional = |env: &mut JNIEnv, s: &JString| -> jni::errors::Result<Option<String>> {
            if s.is_null() {
                Ok(None)
            } else {
                Ok(Some(env.get_string(s)?.into()))
            }
        };
        let reason = optional(env, &reason)?;
        let bluetooth_name = optional(env, &bluetooth_name)?;
        let json: String = env.get_string(&calls)?.into();
        let calls = calls::calls(&json).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "unreadable call report from the phone");
            Vec::new()
        });
        cursio_core::calls_hub::hub().set_phone(
            cursio_protocol::calls::CallsMsg::Status {
                available: available != JNI_FALSE,
                reason,
                bluetooth_name,
            },
            calls,
        );
        Ok(())
    })
}

/// A notification on this phone went away: dismissed, read, or withdrawn.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeNotificationRemoved(
    mut env: JNIEnv,
    _this: JObject,
    key: JString,
) {
    guard(&mut env, (), |env| {
        let key: String = env.get_string(&key)?.into();
        cursio_core::notify_hub::hub().remove(&key);
        Ok(())
    })
}

/// The phone's messages changed; every desktop reading them is told to look
/// again.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeSmsChanged(
    mut env: JNIEnv,
    _this: JObject,
) {
    guard(&mut env, (), |_env| {
        cursio_core::messages_hub::hub().changed();
        Ok(())
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
        media::changed(&json);
        Ok(())
    })
}

/// MediaStore changed — a photo taken, a screenshot, one deleted — so a peer
/// browsing this phone's gallery should ask again.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeGalleryChanged(
    mut env: JNIEnv,
    _this: JObject,
) {
    guard(&mut env, (), |_env| {
        cursio_core::gallery_hub::hub().changed();
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
        let Some(command) = media::command_from(&command, arg) else {
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

/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativeShareTargets<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    let json = guard(&mut env, "[]".to_string(), |_env| Ok(share::targets_json()));
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
        Ok(share::send(&peer, &text, &path, &name, &mime, &action)
            .err()
            .unwrap_or_default())
    });
    env.new_string(answer)
        .unwrap_or_else(|_| JString::default())
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

/// The screen came on unlocked (`in_use`), or went off or locked; see
/// `cursio_core::at_phone`.
///
/// # Safety
///
/// Called by the JVM with valid arguments; not callable from Rust.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_dev_cursio_mesh_CursioNative_nativePhoneInUse(
    mut env: JNIEnv,
    _class: JClass,
    in_use: jboolean,
) {
    guard(&mut env, (), |_env| {
        cursio_core::at_phone::set_in_use(in_use != JNI_FALSE);
        Ok(())
    })
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
