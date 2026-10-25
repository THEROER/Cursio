//! Mounting a peer's volume and taking it down again, including mounts a
//! crashed run left behind.

use crate::bridge::{mount_threads, PeerLink};
use crate::mount::RemoteFs;
use fuser::MountOption;

/// A live mount. Dropping it unmounts.
pub struct MountHandle {
    /// Dropping this unmounts and joins the FUSE threads — which is the whole
    /// cleanup path, since `AutoUnmount` is deliberately not used (see `mount`).
    _session: fuser::BackgroundSession,
    mountpoint: std::path::PathBuf,
}

impl MountHandle {
    pub fn mountpoint(&self) -> &std::path::Path {
        &self.mountpoint
    }
}

impl Drop for MountHandle {
    /// Remove the mount point once the mount is gone.
    ///
    /// Field order does the unmounting: `_session` is declared first, so it
    /// drops before this runs and the directory is empty by the time it is
    /// removed. Without this an unplugged disk leaves an empty folder behind
    /// under `~/Cursio/<peer>`, and a file manager shows it as a directory that
    /// is not a disk at all.
    ///
    /// `remove_dir` rather than `remove_dir_all`, and its failure ignored: it
    /// removes only an empty directory, so an unmount that did not take cannot
    /// take the peer's files with it.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.mountpoint);
    }
}

/// Unmount whatever a previous run left at this path, if anything.
///
/// Best-effort and deliberately quiet about the ordinary case: nothing mounted
/// means `fusermount3` fails, which is exactly what a clean start looks like.
/// `-z` (lazy) so a mount whose peer is gone detaches immediately rather than
/// blocking on a filesystem that will never answer — the point is to get out of
/// the way, and any process still holding it keeps its own view until it exits.
pub(super) fn release_stale_mount(mountpoint: &std::path::Path) {
    if !is_mounted(mountpoint) {
        return;
    }
    for tool in ["fusermount3", "fusermount"] {
        let ok = std::process::Command::new(tool)
            .args(["-u", "-z"])
            .arg(mountpoint)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            tracing::info!(
                mount = %mountpoint.display(),
                "released a mount left over from a previous run"
            );
            return;
        }
    }
    tracing::warn!(
        mount = %mountpoint.display(),
        "a previous mount is still in place and could not be released; \
         mounting will likely fail — clear it with `fusermount3 -u`"
    );
}

/// Whether this path is currently a mount point.
///
/// Read from `/proc/mounts` rather than by stat'ing the path: stat on a wedged
/// FUSE mount is precisely the call that blocks forever, and this runs at
/// startup where a hang would take the whole session down with it.
pub(super) fn is_mounted(mountpoint: &std::path::Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    // Paths appear with spaces and tabs escaped as octal; comparing the escaped
    // form of ours avoids decoding every line.
    let needle = mountpoint.to_string_lossy().replace(' ', "\\040");
    mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .any(|p| p == needle)
}

/// Release every Cursio mount left under `root` by a previous run.
///
/// [`release_stale_mount`] clears one known path; this clears the ones whose
/// names are no longer known. A volume's directory is named after the disk, so
/// anything that changes the name — the peer relabelling a drive, or a version
/// of Cursio that named them differently — leaves the old mount behind with
/// nothing left to look it up by. They accumulate: a file manager's sidebar
/// fills with entries for disks that answer nothing, and `~/Cursio/<peer>`
/// cannot be tidied because each one is a live mount point.
///
/// Only `fuse.cursio` mounts under this root are touched, so a filesystem the
/// user mounted there themselves is left alone.
pub fn release_stale_peer_mounts(root: &std::path::Path) {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return;
    };
    let prefix = format!("{}/", root.to_string_lossy().replace(' ', "\\040"));
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_source), Some(target), Some(fstype)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if fstype != "fuse.cursio" || !target.starts_with(&prefix) {
            continue;
        }
        // Undo the escaping `/proc/mounts` applies, so the path handed to
        // `fusermount` is the real one.
        let target = target.replace("\\040", " ");
        release_stale_mount(std::path::Path::new(&target));
    }
}

/// The session configuration the mount runs under.
///
/// Split out from [`mount`] so the settings can be asserted on without mounting
/// anything: everything here is a decision, and the concurrency ones are silent
/// when wrong — the filesystem still works, only far slower.
pub(super) fn mount_config(peer_name: &str) -> fuser::Config {
    let mut config = fuser::Config::default();

    // Every callback in this filesystem blocks on a round trip: `PeerLink::call`
    // parks the calling thread until the peer answers. On the default single
    // event loop that serialises the entire mount — one request in flight at a
    // time, each paying the full latency — so copying many small files runs at
    // one file per round trip however much bandwidth is free. The engine at the
    // far end already pipelines (`pump_requests` drains several per pass) and
    // `RemoteFs` takes `&self` behind a mutex, so filling that pipe only ever
    // needed threads to wait in parallel.
    config.n_threads = Some(mount_threads());
    // Give each worker its own `/dev/fuse` fd, so they take requests from the
    // kernel independently instead of contending on a single descriptor.
    config.clone_fd = true;

    config.mount_options = vec![
        MountOption::FSName(format!("cursio-{peer_name}")),
        MountOption::Subtype("cursio".to_string()),
        // The peer's disks are the user's own; nothing here should honour
        // set-user-id bits or device nodes that arrived over a network.
        MountOption::NoSuid,
        MountOption::NoDev,
        // No `x-gvfs-show` here, and none is needed: gvfs already picks this
        // up through its UDisks2 monitor as a `GProxyMount`, so the mount
        // appears in a file manager's sidebar with an eject button of its own.
        // The option would not have helped anyway — `x-*` options are userspace
        // ones that libmount interprets from an fstab entry, while fuser passes
        // CUSTOM straight to the kernel as a mount flag, where no gvfs monitor
        // ever sees it.
        // Deliberately NOT AutoUnmount: fuser refuses it unless the session ACL
        // is widened to `allow_other` or `allow_root`, and either would expose
        // the peer's files — and through them the peer's whole disk — to every
        // account on this machine. That is a real loss of privacy to buy
        // cleanup after a crash, which `MountHandle`'s Drop already handles on
        // every ordinary exit. A crash leaves a stale mount point, cleared with
        // `fusermount -u`; that is the better trade.
    ];
    config
}

/// Mount ONE of the peer's volumes at `mountpoint`, returning once it is
/// visible.
///
/// Called once per volume the peer offers: `volume` is the wire id (`"C:"`,
/// `"/"`, a removable medium's mount path). Each mount is its own entry in a
/// file manager's sidebar, with the capacity of that disk alone — see
/// [`RemoteFs::bound`].
pub fn mount(
    peer: PeerLink,
    mountpoint: impl AsRef<std::path::Path>,
    peer_name: &str,
    volume: &str,
) -> std::io::Result<MountHandle> {
    let mountpoint = mountpoint.as_ref().to_path_buf();
    // Clear a mount left behind by a previous run before creating anything.
    //
    // Without `AutoUnmount` (see below) a crash — or a process killed while the
    // filesystem was wedged — leaves the old mount in place, and the next start
    // fails with `EEXIST` on a point that looks like an ordinary directory. The
    // user would have to know to run `fusermount3 -u` by hand, so do it here.
    // The parent first, then the mount point itself.
    //
    // The parent matters because this path moved: a mount used to be
    // `~/Cursio/<peer>` and is now `~/Cursio/<peer>/<volume>`, so a version
    // before that change — or a crash from one — leaves a mount sitting on the
    // very directory the volumes now live in. Every path inside a wedged FUSE
    // mount answers `ENOTCONN`, so `create_dir_all` below fails and every one
    // of the peer's volumes is refused. Seen live: all six of a peer's drives
    // failed with "Transport endpoint is not connected" and none returned,
    // because nothing retried and nothing cleared the parent.
    if let Some(parent) = mountpoint.parent() {
        release_stale_mount(parent);
    }
    release_stale_mount(&mountpoint);
    std::fs::create_dir_all(&mountpoint)?;

    let config = mount_config(peer_name);
    let session = fuser::Session::new(
        RemoteFs::new(peer, volume).of_peer(peer_name),
        &mountpoint,
        &config,
    )?
    .spawn()?;
    tracing::info!(mount = %mountpoint.display(), peer = peer_name, volume, "peer volume mounted");
    Ok(MountHandle {
        _session: session,
        mountpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mount check must answer from `/proc/mounts` and never stat the path.
    ///
    /// Stat'ing a wedged FUSE mount is the exact call that blocks forever, and
    /// this runs at startup — a hang there would take the whole session with
    /// it. An ordinary directory is not a mount point, and a path that does not
    /// exist is not either; both must answer instantly.
    #[test]
    fn the_mount_check_never_touches_the_filesystem() {
        let tmp = std::env::temp_dir().join("cursio-not-a-mount-point");
        let _ = std::fs::create_dir_all(&tmp);
        let started = std::time::Instant::now();
        assert!(!is_mounted(&tmp), "a plain directory is not a mount point");
        assert!(!is_mounted(std::path::Path::new("/nonexistent/path/here")));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "the check must be a /proc read, not a filesystem call"
        );
        // The root always is one, so a true answer is reachable too.
        assert!(is_mounted(std::path::Path::new("/")));
        let _ = std::fs::remove_dir(&tmp);
    }

    /// The mount must serve requests concurrently.
    ///
    /// Every callback here blocks on a round trip, so the thread count is the
    /// number of operations that can be in flight. Left at the default of one,
    /// the filesystem still passes every other test and still works — it just
    /// serialises on network latency, which showed up as a copy of many small
    /// files crawling while the link sat idle. Nothing else fails when this
    /// regresses, so it is asserted directly.
    #[test]
    fn the_mount_serves_requests_concurrently() {
        let config = mount_config("peer");
        assert_eq!(
            config.n_threads,
            Some(mount_threads()),
            "a single event loop would serialise every blocking round trip"
        );
        assert!(
            config.clone_fd,
            "workers sharing one fd contend for kernel requests"
        );
    }

    /// The security-relevant mount options must stay on: they are what keeps a
    /// peer's disk from carrying set-user-id bits or device nodes onto this
    /// machine, and nothing observable breaks if they silently go missing.
    #[test]
    fn the_mount_refuses_setuid_and_device_nodes() {
        let config = mount_config("peer");
        assert!(config.mount_options.contains(&MountOption::NoSuid));
        assert!(config.mount_options.contains(&MountOption::NoDev));
        assert_eq!(
            config.acl,
            fuser::SessionACL::Owner,
            "widening the ACL would expose the peer's disk to every local account"
        );
    }
}
