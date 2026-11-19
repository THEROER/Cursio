//! File sharing on the mesh: the files module on one link, and on Windows the
//! unelevated helper process a peer's drives are mounted through.
//!
//! Policy over `cursio-files`' mechanism, split out of `cursio-core` (which
//! re-exports both modules under their old paths, behind its `files`
//! feature). `files-mount` here is the core's feature of that name: whether
//! this build links WinFsp's GPL bindings itself. See THIRD-PARTY.md.

/// File sharing. Serving this machine's volumes is portable and always built;
/// *mounting* the peer's needs FUSE on Linux or WinFsp on Windows (and the
/// latter only under MSVC, because `winfsp-sys` rejects every other target
/// outright), so on a platform with neither the module runs one-way and logs
/// why.
pub mod files_session;
/// The unelevated process that keeps a peer's drives where Explorer sees them.
pub mod mount_helper;

#[cfg(all(target_os = "windows", target_env = "msvc"))]
use cursio_core_base::elevate;
#[cfg(test)]
use cursio_core_base::test_support;
use cursio_core_base::{diagnostics, live, module, module_host, quiet};
