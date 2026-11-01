//! `cursio-mount-helper --parent <pid>`: holds a peer's drives on Windows for
//! the node with that process id, in the session Explorer runs in.
//!
//! The same helper `cursio mount-helper` was, as an executable of its own. It
//! is the one Cursio executable that links WinFsp, whose Rust bindings are
//! GPL-3.0; the node starts it beside itself and talks to it over a pipe
//! (`mount_helper.rs`), so a node built without `files-mount` — or a closed
//! product embedding Cursio — mounts a peer's disks without carrying that
//! code. See THIRD-PARTY.md.

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let parent = match (args.next().as_deref(), args.next()) {
        (Some("--parent"), Some(pid)) => pid
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("`{pid}` is not a process id"))?,
        _ => {
            anyhow::bail!("usage: cursio-mount-helper --parent <pid>; the node starts this itself")
        }
    };
    let _log = cursio_core::logging::init_with_verbosity("cursio-mount-helper", 0);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("cannot start the async runtime: {e}"))?
        .block_on(cursio_core::mount_helper::run_helper(parent))
}
