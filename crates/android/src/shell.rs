//! This phone's own `shell`, reached through its wireless debugging
//! (`docs/phone-shell-design.md`).
//!
//! The key, the pairing and every command are here; Kotlin says where the
//! phone is (its Wi-Fi address) and, when Android's mDNS shows it, where the
//! debugging port is. On HyperOS 3 / Android 16 it shows neither of the
//! phone's own services to an app, so the debugging port is found by trying
//! the ports wireless debugging picks from ([`find_debugging_port`]) and kept
//! until it stops answering — it changes only when debugging is turned on
//! again.
//!
//! The key lives in the node's data directory, which neither backup nor
//! device-to-device transfer copies: it is this phone's shell. Paired is a
//! file beside it holding the phone's answer; losing that only costs saying
//! "paired" in the window, since the phone's own Paired devices list decides.
//!
//! [`find_debugging_port`]: cursio_phone_shell::find_debugging_port

use std::path::PathBuf;
use std::sync::Mutex;

/// Where the key and the paired marker live.
fn dir() -> Result<PathBuf, String> {
    cursio_protocol::paths::data_dir()
        .map(|d| d.join("shell"))
        .ok_or_else(|| "the node's data directory is not known yet".to_owned())
}

const PAIRED: &str = "paired";

/// The name the phone lists this key under in its Paired devices.
const KEY_NAME: &str = "cursio@this-phone";

/// The debugging port last found by trying, `host:port`.
static FOUND: Mutex<Option<String>> = Mutex::new(None);

fn key() -> Result<cursio_phone_shell::AdbKey, String> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot keep the debugging key: {e}"))?;
    cursio_phone_shell::key(&dir, KEY_NAME).map_err(|e| e.to_string())
}

/// Whether this phone's key was paired with its wireless debugging.
pub fn paired() -> bool {
    dir().is_ok_and(|d| d.join(PAIRED).is_file())
}

/// Run `future` to its end on a runtime of its own: Kotlin calls in from a
/// thread of its own, which is no Tokio worker.
fn block<T>(future: impl std::future::Future<Output = T>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|rt| rt.block_on(future))
        .map_err(|e| format!("cannot start a runtime: {e}"))
}

/// Pair with the phone's pairing service at `addr` with the six-digit
/// `code` its screen shows, then say in the node's log — which every
/// computer reads — whether the shell now really answers.
pub fn pair(addr: &str, code: &str) -> Result<(), String> {
    let key = key()?;
    let guid = block(cursio_phone_shell::pair(addr, code, &key))?.map_err(|e| e.to_string())?;
    let dir = dir()?;
    std::fs::write(dir.join(PAIRED), &guid)
        .map_err(|e| format!("paired, but cannot note it: {e}"))?;
    tracing::info!(%guid, "this phone's shell is paired");
    match run("id -un") {
        Ok(user) => tracing::info!(user = %user.trim(), "this phone's shell answers"),
        Err(why) => tracing::warn!(%why, "paired, but this phone's shell does not answer"),
    }
    Ok(())
}

/// Run shell source `command` as the phone's `shell` user, finding the
/// debugging port first when it is not known or no longer answers.
pub fn run(command: &str) -> Result<String, String> {
    if !paired() {
        return Err("this phone's shell is not paired yet: pair it in Cursio's settings".into());
    }
    let mut key = key()?;
    let known = known_addr();
    if let Some(addr) = &known {
        match block(cursio_phone_shell::exec(addr, &mut key, command))? {
            Ok(out) => return Ok(String::from_utf8_lossy(&out).into_owned()),
            Err(why) => {
                tracing::debug!(%addr, %why, "the known debugging port did not answer; looking again")
            }
        }
    }
    *lock() = None;
    let host = host().ok_or("this phone has no Wi-Fi address; wireless debugging needs Wi-Fi")?;
    let port = block(cursio_phone_shell::find_debugging_port(&host, &mut key))?.ok_or(
        "wireless debugging is off, or this key is not on the phone's Paired devices: \
         turn it on in the developer settings, or pair again",
    )?;
    let addr = format!("{host}:{port}");
    tracing::info!(%addr, "found this phone's debugging port");
    *lock() = Some(addr.clone());
    let out =
        block(cursio_phone_shell::exec(&addr, &mut key, command))?.map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn lock() -> std::sync::MutexGuard<'static, Option<String>> {
    FOUND.lock().unwrap_or_else(|p| p.into_inner())
}

/// The debugging port as mDNS showed it, else as last found by trying.
fn known_addr() -> Option<String> {
    ask("shellAddr").or_else(|| lock().clone())
}

/// This phone's Wi-Fi address.
fn host() -> Option<String> {
    ask("shellHost")
}

/// A `CursioNative` method answering a string, `""` meaning none.
fn ask(method: &str) -> Option<String> {
    let answer = crate::jvm::with_native(|env, class| {
        let answer = env
            .call_static_method(class, method, "()Ljava/lang/String;", &[])?
            .l()?;
        let answer: String = env.get_string(&jni::objects::JString::from(answer))?.into();
        Ok(answer)
    })
    .map_err(|why| tracing::warn!(%method, %why, "cannot ask Kotlin about this phone's debugging"))
    .ok()?;
    (!answer.is_empty()).then_some(answer)
}
