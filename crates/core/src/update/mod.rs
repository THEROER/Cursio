//! Installing, switching and rolling back builds of Cursio on one machine.
//!
//! Phase 1 of `docs/update-design.md` is the local half: builds live side by
//! side under `versions/<version>/`, a `current` pointer names the one that
//! runs, `previous` names the one to fall back to, and a helper process flips
//! the pointer and relaunches the node ([`layout`], [`apply`]). Phase 2 fetches
//! them from peers: signed ([`sign`]), held and served ([`store`]), exchanged
//! over a link ([`lane`]), and switched to when policy and a quiet moment
//! allow ([`agent`], [`quiet`]). Fetching from a public release is phase 3.
//!
//! Replacing files in place, the way a single loose `cursio.exe` was updated,
//! is what this replaces: a running executable is locked on Windows, so every
//! update needed a fresh file name, and "which build is running" became a
//! question nobody could answer.

pub mod agent;
pub mod apply;
pub mod cli;
pub mod lane;
pub mod layout;
pub mod manifest;
pub mod quiet;
pub mod sign;
pub mod store;

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read the build manifest {path}: {reason}")]
    Manifest { path: String, reason: String },
    #[error("sha256 mismatch for {what}: expected {expected}, got {actual}")]
    HashMismatch {
        what: String,
        expected: String,
        actual: String,
    },
    #[error("no install directory for this platform; set CURSIO_INSTALL_ROOT")]
    NoRoot,
    #[error("version {0} is not installed; `cursio update status` lists what is")]
    NotInstalled(String),
    #[error("`{0}` is not a usable version name; use letters, digits, `.`, `-`, `+` and `_`")]
    BadVersion(String),
    #[error("{0}")]
    Refused(String),
}

/// The platform this build takes updates for: `<os>-<arch>`, and on Linux the
/// FFmpeg it was linked against (`linux-x86_64-av59`) — see `build.rs` for why
/// two Linux machines are not one platform.
pub fn platform() -> String {
    let base = cursio_protocol::update::this_platform();
    match option_env!("CURSIO_PLATFORM_ABI") {
        Some(abi) => format!("{base}-{abi}"),
        None => base,
    }
}

/// Which build this process is: the installed version it was started from,
/// or the crate version for a development build under `target/`.
pub fn running_build() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| layout::InstallLayout::for_user().ok()?.version_of(&exe))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

impl UpdateError {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}
