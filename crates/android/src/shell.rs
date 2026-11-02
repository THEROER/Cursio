//! This phone's own `shell`, reached through its wireless debugging
//! (`docs/phone-shell-design.md`).
//!
//! Kotlin finds the phone's pairing and debugging services by mDNS — Android's
//! own `NsdManager` sees them, where a library browsing the network may not —
//! and hands the addresses down; the key, the pairing and every command are
//! here. The key lives in the node's data directory, which neither backup nor
//! device-to-device transfer copies: it is this phone's shell.
//!
//! Paired is a file beside the key holding the phone's answer. Losing it only
//! costs saying "paired" in the window; the phone's own Paired devices list
//! is what decides, and an unpaired key is refused there with a reason.

use std::path::PathBuf;

/// Where the key and the paired marker live.
fn dir() -> Result<PathBuf, String> {
    cursio_protocol::paths::data_dir()
        .map(|d| d.join("shell"))
        .ok_or_else(|| "the node's data directory is not known yet".to_owned())
}

const PAIRED: &str = "paired";

/// The name the phone lists this key under in its Paired devices.
fn key_name() -> String {
    "cursio@this-phone".to_owned()
}

fn key() -> Result<cursio_phone_shell::AdbKey, String> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot keep the debugging key: {e}"))?;
    cursio_phone_shell::key(&dir, &key_name()).map_err(|e| e.to_string())
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
/// `code` its screen shows.
pub fn pair(addr: &str, code: &str) -> Result<(), String> {
    let key = key()?;
    let guid = block(cursio_phone_shell::pair(addr, code, &key))?.map_err(|e| e.to_string())?;
    let dir = dir()?;
    std::fs::write(dir.join(PAIRED), &guid)
        .map_err(|e| format!("paired, but cannot note it: {e}"))?;
    tracing::info!(%guid, "this phone's shell is paired");
    // Said in the node's log, which every computer reads: whether the key
    // now really reaches the shell, not only whether pairing answered.
    match connect_addr() {
        Some(addr) => match exec(&addr, "id -un") {
            Ok(user) => tracing::info!(user = %user.trim(), %addr, "this phone's shell answers"),
            Err(why) => {
                tracing::warn!(%why, %addr, "paired, but this phone's shell does not answer")
            }
        },
        None => {
            tracing::info!("paired; the debugging port is not seen yet, so the shell is not tried")
        }
    }
    Ok(())
}

/// This phone's debugging port while wireless debugging is on, as Kotlin's
/// mDNS sees it.
pub fn connect_addr() -> Option<String> {
    let addr = crate::jvm::with_native(|env, class| {
        let addr = env
            .call_static_method(class, "shellAddr", "()Ljava/lang/String;", &[])?
            .l()?;
        let addr: String = env.get_string(&jni::objects::JString::from(addr))?.into();
        Ok(addr)
    })
    .map_err(|why| tracing::warn!(%why, "cannot ask where this phone's debugging port is"))
    .ok()?;
    (!addr.is_empty()).then_some(addr)
}

/// Run shell source `command` as the phone's `shell` user through its
/// debugging port at `addr`.
pub fn exec(addr: &str, command: &str) -> Result<String, String> {
    if !paired() {
        return Err("this phone's shell is not paired yet: pair it in Cursio's window".into());
    }
    let key = key()?;
    let out = block(cursio_phone_shell::exec(addr, key, command))?.map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}
