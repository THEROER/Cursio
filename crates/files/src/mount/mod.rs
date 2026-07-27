//! Presenting the peer's volumes here as ordinary folders.
//!
//! The mount point holds one directory per volume the peer offers — `C:`, `D:`,
//! or `/` — and below that its real contents. Any application opens a file
//! across the link without knowing it is remote, which is the whole point: no
//! special client, no copy step.

pub mod attr;
mod fs;
pub mod inode;

// Re-exported so existing callers keep naming these through `mount`, though the
// bridge itself now lives at the crate root: Windows needs it too.
pub use crate::bridge::{Op, PeerLink, OP_TIMEOUT};
pub use fs::{mount, MountHandle, RemoteFs};
