//! Presenting the peer's volumes on Windows, via WinFsp.
//!
//! The Linux counterpart of this is [`crate::mount`]. Both drive the same
//! [`PeerLink`](crate::mount::bridge::PeerLink) and answer to the same wire
//! protocol; only the OS-facing half differs, and it differs more than the
//! shared vocabulary suggests:
//!
//! | | FUSE | WinFsp |
//! |---|---|---|
//! | addressing | inode numbers, after one `lookup` | the full path, every call |
//! | paths | UTF-8, `/` | UTF-16, `\` |
//! | time | Unix seconds | FILETIME: 100 ns ticks since 1601 |
//! | mount point | any empty directory | a drive letter |
//!
//! Because WinFsp passes the path on every operation there is no inode table
//! here — the bookkeeping that [`crate::mount::inode`] exists for simply has no
//! counterpart, which removes a whole class of lifetime bugs on this side.
//!
//! ## One drive letter per peer
//!
//! A Linux peer offers a single volume (`/`), so it becomes one drive whose root
//! is the peer's filesystem root. A peer offering several volumes gets one
//! directory per volume under that drive, mirroring what the Linux mount does
//! with `C:` and `D:`.

mod convert;
mod fs;

pub use fs::{mount, MountHandle};
