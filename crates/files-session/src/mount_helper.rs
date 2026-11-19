//! The unelevated process that holds a peer's drives on Windows.
//!
//! The node elevates so injected input reaches elevated windows
//! ([`crate::elevate`]). A drive letter, though, belongs to the logon session
//! that defined it, and UAC splits an administrator into two: the elevated one
//! the node now runs in, and the filtered one Explorer runs in. A peer's disk
//! mounted by the node was there — reachable by its UNC path, readable from an
//! elevated prompt — and missing from Explorer, which is where anyone looks.
//!
//! So the node starts a copy of itself, `cursio mount-helper`, in Explorer's
//! session, and the drives live there. Only the mount moves (see
//! `cursio_files::relay` for why the engine cannot): each volume is one pipe
//! connection from the node to the helper, carrying the mount's operations one
//! way and the session's answers the other, and closing it unmounts.
//!
//! **Started unelevated from an elevated process.** Windows offers no flag for
//! that; the child of an elevated process inherits its token. The token is
//! taken from the shell instead — the desktop window's process is Explorer,
//! running as the same user, unelevated — and the helper started with it.
//! An administrator holds the impersonation privilege `CreateProcessWithTokenW`
//! needs, so this works exactly when the node is elevated, which is the only
//! time it is wanted.
//!
//! **One helper per node, gone with it.** The pipe is named after the node's
//! process id and the helper exits when that process does, so a restarted or
//! updated node starts a helper of its own version rather than talking to one
//! left behind. The node checks that the pipe it reached is served by the very
//! process it started: anyone running as this user could create a pipe of that
//! name first and be handed the peer's files.

//!
//! **Its own executable, `cursio-mount-helper`.** The helper is the one side
//! that needs WinFsp, whose Rust bindings are GPL-3.0; the node's side is a
//! pipe and a relay. So the helper is also built apart (`files-mount`), and a
//! node built without `files-mount` — or a closed product embedding Cursio —
//! mounts a peer's disks through that executable beside it, linking none of
//! WinFsp, and mounts through it when unelevated too, where it would
//! otherwise have mounted in-process. A node built with `files-mount` keeps
//! running a copy of itself as `cursio mount-helper`: certainly the same
//! build, wherever it was started from.

/// The subcommand a node built with `files-mount` runs a copy of itself as.
pub const HELPER_COMMAND: &str = "mount-helper";

/// The helper's own executable, looked for beside the node's.
pub const HELPER_EXE: &str = "cursio-mount-helper";

/// Run as the helper for the node with process id `parent`, until it exits.
#[cfg(all(target_os = "windows", target_env = "msvc", feature = "files-mount"))]
pub async fn run_helper(parent: u32) -> anyhow::Result<()> {
    imp::run_helper(parent).await
}

#[cfg(not(all(target_os = "windows", target_env = "msvc", feature = "files-mount")))]
pub async fn run_helper(_parent: u32) -> anyhow::Result<()> {
    anyhow::bail!(
        "the mount helper exists only in a Windows (MSVC) build with `files-mount`: elsewhere \
         the node mounts a peer's volumes itself, and a node without it starts \
         `{HELPER_EXE}` instead"
    )
}

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub use imp::{mount, RelayedMount};

#[cfg(all(target_os = "windows", target_env = "msvc"))]
mod imp;
