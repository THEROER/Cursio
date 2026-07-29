//! Filesystem sharing: each machine's volumes, mounted on the other.
//!
//! Split the way the other cursio crates are: this crate is the *mechanism*
//! (serving local files, mounting remote ones), and `cursio-core`'s session
//! module is the *policy* that drives it over the encrypted channel.
//!
//! - [`server`] answers the peer's requests against this machine's disks.
//! - [`mount`] presents the peer's volumes here as ordinary folders.

/// Low-priority threads that carry out file work without crowding out the rest
/// of the machine.
pub mod background;
/// The link between a mount's blocking callbacks and the async session. Shared
/// by both mount implementations — the platform differences are all in how the
/// OS asks, not in how the answer is fetched.
pub mod bridge;
#[cfg(target_os = "linux")]
pub mod mount;
pub mod server;
/// Windows mounting, present only where `winfsp` can be built — see this
/// crate's `Cargo.toml` for why that means MSVC rather than Windows in general.
#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub mod winmount;

pub use background::Workers;
pub use server::{FileServer, SharedVolume};
