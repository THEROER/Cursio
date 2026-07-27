//! Cursio core: node identity and session orchestration.
//!
//! Ties the protocol and transport crates together into the two flows the CLI
//! exposes:
//! - a persistent [`identity::NodeIdentity`] (stable [`PeerId`] + display name),
//! - a [`session`] handshake + heartbeat loop over any [`Transport`].
//!
//! Capture, input proxying, and the Tauri UI build on top of this later; for
//! now this is the layer that proves two machines can find, pair, and hold a
//! live session.

pub mod audio_session;
pub mod clipboard_session;
/// Windows self-elevation, so injected input isn't blocked by UIPI.
pub mod elevate;
/// File sharing. Present where a mount implementation exists: FUSE on Linux,
/// WinFsp on Windows — and the latter builds only under MSVC, because
/// `winfsp-sys` rejects every other target outright.
#[cfg(any(
    target_os = "linux",
    all(target_os = "windows", target_env = "msvc")
))]
pub mod files_session;
pub mod identity;
pub mod logging;
pub mod mesh;
pub mod module;
pub mod window_session;
pub mod registry;
pub mod session;

pub use identity::NodeIdentity;
