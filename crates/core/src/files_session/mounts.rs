//! The peer's volumes here: where each is mounted, rescanning our own,
//! and keeping the mounts in step with what the peer shares.

use crate::files_session::FilesEngine;
use cursio_files::bridge::PeerLink;
use cursio_files::FileServer;
use cursio_protocol::Volume;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

/// How often the local volume set is re-read, so a medium plugged in after the
/// session started is offered without waiting for a reconnect.
///
/// Polled rather than driven by udev or udisks: the volume set is a handful of
/// lines from `/proc/self/mounts` plus one sysfs read per block device, which
/// is cheaper than holding a D-Bus subscription that would have to be
/// duplicated for Windows anyway. A second is far below what a person notices
/// between plugging a stick in and looking for it, and udisks itself takes a
/// noticeable fraction of that to mount the thing.
pub(super) const VOLUME_RESCAN: Duration = Duration::from_secs(1);

/// How long to wait before trying a failed mount again.
///
/// Long enough that a mount point which cannot be made is not hammered — each
/// attempt is filesystem work — and short enough that a disk missing because of
/// something the user is in the middle of clearing appears without them having
/// to restart the session.
#[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
pub(super) const MOUNT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Where a peer's volumes are mounted on Linux.
///
/// Under the user's runtime directory rather than `/mnt`: no privileges needed,
/// and it disappears on logout instead of leaving a stale mount point behind.
#[cfg(target_os = "linux")]
pub(super) fn mount_root(peer_name: &str) -> std::path::PathBuf {
    // Under the home directory, not `$XDG_RUNTIME_DIR`.
    //
    // The runtime directory is the tidier choice — it needs no privileges and
    // is cleared at logout — but file managers deliberately hide it, so the
    // peer's disks were mounted, readable, and invisible: not in Nautilus's
    // sidebar, not reachable by browsing, only by typing the path. A share
    // nobody can find is not shared. `~/Cursio/<peer>` shows up like any other
    // folder, and the stale-mount sweep at startup covers the cleanup the
    // runtime directory used to give for free.
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Cursio").join(peer_name)
}

/// Where one of a peer's volumes is mounted on Linux.
///
/// `~/Cursio/<peer>/<volume>`, so a peer's disks stay grouped under its name
/// while each is a mount of its own. Kept under the peer directory rather than
/// flattened to `~/Cursio/<peer>-<volume>` because the grouping is what makes
/// several peers legible, and gvfs lists each mount in the sidebar by its own
/// name regardless of where it sits.
#[cfg(target_os = "linux")]
pub(super) fn volume_mount_point(peer_name: &str, volume: &Volume) -> std::path::PathBuf {
    mount_root(peer_name).join(cursio_files::volume_display_name(&volume.id, &volume.label))
}

/// Paths this machine must not serve to the peer.
///
/// Where the peer's own volumes are mounted here. Without this the two machines
/// serve each other their own disks: Linux offers `/`, the peer's drives hang
/// off it under `~/Cursio/<peer>/`, and the peer mounts them again as drives of
/// its own — a machine with two disks announcing six, four of which are the
/// asking machine's own files taking a round trip across the network to come
/// home under a different letter.
///
/// Windows needs none: its volumes are drive letters, and a peer's mount gets a
/// letter of its own rather than appearing inside one this machine offers.
#[cfg(target_os = "linux")]
pub(super) fn excluded_paths(peer_name: &str) -> Vec<std::path::PathBuf> {
    // The whole `~/Cursio` tree, not just this peer's directory: every peer's
    // mounts live under it, and a third machine's disks are no more ours to
    // serve than this one's.
    let root = mount_root(peer_name);
    match root.parent() {
        Some(cursio) => vec![cursio.to_path_buf()],
        None => vec![root],
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn excluded_paths(_peer_name: &str) -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// A live mount of the peer's volumes, whichever OS is presenting them.
#[cfg(target_os = "linux")]
pub(super) type Mount = cursio_files::mount::MountHandle;
/// On Windows a drive is mounted here only when the node runs unelevated;
/// elevated, the helper holds it, in the session Explorer runs in (see
/// [`crate::mount_helper`]).
#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub(super) enum Mount {
    Here {
        _drive: cursio_files::winmount::MountHandle,
    },
    Helper(crate::mount_helper::RelayedMount),
}

/// Whether a mount is still standing. Only a relayed one can fall on its own
/// — its helper exits, or refuses — and is then dropped so the retry mounts it
/// again.
#[cfg(target_os = "linux")]
pub(super) fn mount_is_alive(_mount: &Mount) -> bool {
    true
}

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub(super) fn mount_is_alive(mount: &Mount) -> bool {
    match mount {
        Mount::Here { .. } => true,
        Mount::Helper(relayed) => relayed.is_alive(),
    }
}

/// Uninhabited where no mount implementation exists, so `Option<Mount>` is
/// always `None` and every path that reads it stays compiled and honest.
#[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
pub(super) enum Mount {}

/// Mount one of the peer's volumes with whatever this platform provides.
///
/// The two implementations differ in everything the OS cares about — a
/// directory versus a drive letter, FUSE versus WinFsp — and in nothing this
/// module does: both take the same [`PeerLink`] and answer the same protocol.
///
/// One volume at a time on both, and driven by the peer's `Volumes`
/// announcement rather than by startup — see [`FilesEngine::sync_peer_mounts`].
/// A drive letter forced the shape on Windows (the letter *is* the volume, and
/// none can be added to a mount already made); Linux followed because the
/// alternative it allowed — every volume a folder under one mount — could
/// report only one capacity for all of them and put a single entry in a file
/// manager's sidebar. A peer's disk should look here the way it looks on the
/// machine that owns it.
#[cfg(target_os = "linux")]
pub(super) fn mount_one_volume(
    peer: PeerLink,
    peer_name: &str,
    volume: &Volume,
) -> anyhow::Result<Mount> {
    Ok(cursio_files::mount::mount(
        peer,
        volume_mount_point(peer_name, volume),
        peer_name,
        &volume.id,
    )?)
}

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub(super) fn mount_one_volume(
    peer: PeerLink,
    peer_name: &str,
    volume: &Volume,
) -> anyhow::Result<Mount> {
    if crate::elevate::is_elevated() {
        match crate::mount_helper::mount(peer.clone(), peer_name, &volume.id, &volume.label) {
            Ok(relayed) => return Ok(Mount::Helper(relayed)),
            Err(e) => tracing::warn!(
                peer = %peer_name,
                volume = %volume.id,
                error = %format!("{e:#}"),
                "cannot start the drive helper; mounting here, where Explorer will not show the \
                 drive (it stays reachable by its \\\\cursio\\ path)"
            ),
        }
    }
    cursio_files::winmount::mount(peer, peer_name, &volume.id, &volume.label)
        .map(|drive| Mount::Here { _drive: drive })
}

/// What makes a volume *this* volume, for deciding whether the offered set has
/// changed.
///
/// Everything a peer addresses or displays, and nothing that moves on its own:
/// a disk filling up is not a new disk. See [`FilesEngine::rescan_volumes`] for
/// what including the byte counts here cost.
pub(super) fn volume_identity(volumes: &[Volume]) -> Vec<(&str, &str, bool, bool)> {
    volumes
        .iter()
        .map(|v| (v.id.as_str(), v.label.as_str(), v.writable, v.removable))
        .collect()
}

impl FilesEngine {
    /// Re-read the local volumes, and schedule an announcement if the *set*
    /// changed.
    ///
    /// Compared as a whole rather than watched for arrivals: a medium going away
    /// matters as much as one appearing, and the peer is sent the entire set in
    /// either case, so there is nothing a delta would buy.
    ///
    /// Compared on identity only, deliberately excluding `free_bytes` and
    /// `total_bytes`. Free space on a live disk changes constantly — a browser
    /// cache write is enough — so comparing whole `Volume`s made every rescan
    /// report a change, once a second, forever: an announcement to the peer and
    /// a "local volumes changed was=5 now=5" line each time. The set was
    /// identical every time; only the statistics had moved. Capacity still
    /// reaches the peer, it just no longer *triggers* the announcement.
    pub(super) fn rescan_volumes(&mut self) {
        // Take delivery of a scan that finished since the last pump. The worker
        // has already put it into the server; see [`FilesEngine::server`] for
        // why that is not done here.
        while let Ok(changed) = self.rescan_rx.try_recv() {
            self.rescanning = false;
            if let Some(volumes) = changed {
                self.local_volumes = volumes;
                self.announce_volumes = true;
            }
        }

        if self.rescanning || self.last_rescan.elapsed() < VOLUME_RESCAN {
            return;
        }
        self.last_rescan = Instant::now();
        self.rescanning = true;

        // Scanned on a worker, never inline.
        //
        // Reading the volume set is not the cheap directory read it looks like.
        // Windows probes every drive letter, and a letter whose medium is gone
        // — an empty card reader, a disconnected USB disk — does not fail fast:
        // the request goes to the device and waits out its timeout. Measured on
        // a real machine the moment a disk dropped out: 3048 ms per scan, once
        // a second, which stopped input, audio and the file mount alike and
        // left the peer timing out file operations in a loop. Linux is cheap by
        // comparison but not free, and the cost is the same kind: a stale NFS
        // or autofs mount blocks in `statfs` exactly the same way.
        let tx = self.rescan_tx.clone();
        let server = std::sync::Arc::clone(&self.server);
        let peer = self.peer_name.clone();
        let share = self.share.clone();
        self.workers.spawn(move || {
            let after = FileServer::detect_shared(&share);
            let changed = {
                let mut server = server.lock().unwrap_or_else(PoisonError::into_inner);
                let before = server.volumes();
                let now = after.volumes();
                if volume_identity(&now) == volume_identity(&before) {
                    None
                } else {
                    tracing::info!(
                        peer = %peer,
                        was = before.len(),
                        now = now.len(),
                        "local volumes changed"
                    );
                    // Only the volume set, never the open handles: see
                    // [`FileServer::adopt_volumes`] for what replacing the whole
                    // server did to a peer that had files open when a disk was
                    // unplugged.
                    server.adopt_volumes(after);
                    Some(now)
                }
            };
            // The receiver only closes when the session is ending, in which case
            // the scan is moot.
            let _ = tx.send(changed);
        });
    }

    /// Bring the set of mounted drives in line with what the peer now offers.
    ///
    /// Driven by the announcement rather than by startup: the peer's volume set
    /// is not known when the session begins, and it changes while the session
    /// runs — a stick goes in, a disk is unplugged. Each volume owns a mount of
    /// its own, so following the set means mounting what arrived and dropping
    /// what left; anything already mounted is untouched, so a peer
    /// re-announcing an unchanged set costs nothing.
    #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
    pub(super) fn sync_peer_mounts(&mut self) {
        // Gone: drop the handle, which unmounts and frees the letter or the
        // mount point.
        let live: std::collections::HashSet<&str> =
            self.peer_volumes.iter().map(|v| v.id.as_str()).collect();
        self.volume_mounts.retain(|id, mount| {
            let keep = live.contains(id.as_str());
            if !keep {
                tracing::info!(peer = %self.peer_name, volume = %id, "peer volume went away; unmounting");
            }
            keep && mount_is_alive(mount)
        });

        // Arrived: one mount each. Anything still missing is retried, since a
        // failure here is often temporary — see [`FilesEngine::mount_retry_at`].
        if self.mount_retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        let mut failed = false;
        for volume in &self.peer_volumes {
            if self.volume_mounts.contains_key(&volume.id) {
                continue;
            }
            match mount_one_volume(self.mount_link.clone(), &self.peer_name, volume) {
                Ok(m) => {
                    self.volume_mounts.insert(volume.id.clone(), m);
                }
                // One volume failing must not cost the others: a machine out of
                // drive letters, or one whose mount point cannot be made,
                // should still show the disks it had room for.
                Err(e) => {
                    failed = true;
                    tracing::warn!(
                        peer = %self.peer_name,
                        volume = %volume.id,
                        error = %e,
                        "cannot mount this peer volume; will retry"
                    );
                }
            }
        }
        // Spaced out rather than attempted every pump: a mount that cannot be
        // made usually cannot be made a millisecond later either, and each
        // attempt touches the filesystem.
        self.mount_retry_at = failed.then(|| Instant::now() + MOUNT_RETRY_INTERVAL);
    }

    /// No-op where this platform has no mount backend at all.
    ///
    /// Serving is portable; mounting is not. macOS has no mount backend here
    /// (FUSE needs a kernel extension the user must install and approve), and a
    /// MinGW build has none either — `winfsp-sys` refuses that target. Only
    /// this half is missing, so a Mac still serves its volumes to a peer that
    /// can mount them; `start` says plainly why the reverse direction is.
    #[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
    pub(super) fn sync_peer_mounts(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files_session::test_support::engine;

    /// A scan that has not come back yet does not get a second one queued
    /// behind it.
    ///
    /// The timer fires once a second, but a scan takes as long as the slowest
    /// drive answers — 3 s on a Windows machine with a medium that has gone
    /// away. Without this guard each tick would start another scan of the same
    /// stuck device, and the pool would fill with scans that all block on it.
    #[test]
    fn a_rescan_does_not_start_while_one_is_still_running() {
        let (mut engine, _ops) = engine();

        // First pass: nothing in flight, so a scan starts.
        engine.last_rescan = Instant::now() - VOLUME_RESCAN * 2;
        engine.rescan_volumes();
        assert!(engine.rescanning, "a scan should have started");

        // Second pass, well past the interval: the first has not answered, so
        // no new scan may begin.
        engine.last_rescan = Instant::now() - VOLUME_RESCAN * 2;
        let before = engine.last_rescan;
        engine.rescan_volumes();
        assert!(
            engine.rescanning,
            "the in-flight scan should still be the only one"
        );
        assert_eq!(
            engine.last_rescan, before,
            "a skipped tick must not count as a scan"
        );
    }

    /// A scan that finds a different volume set leaves the server already
    /// serving it, and hands the loop only the list to announce.
    #[test]
    fn a_rescan_that_finds_a_new_set_is_adopted_off_the_loop() {
        let (mut engine, _ops) = engine();
        *engine.server.lock().expect("server") = FileServer::new(Vec::new());
        engine.last_rescan = Instant::now() - VOLUME_RESCAN * 2;
        engine.rescan_volumes();

        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.rescanning && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            engine.rescan_volumes();
        }
        assert!(!engine.rescanning, "the scan came back");
        let detected = FileServer::detect().volumes();
        assert!(!detected.is_empty(), "this machine offers a volume");
        assert_eq!(
            volume_identity(&engine.local_volumes),
            volume_identity(&detected),
            "the loop has the new set to announce"
        );
        assert!(engine.announce_volumes, "and announces it");
        assert_eq!(
            volume_identity(&engine.server.lock().expect("server").volumes()),
            volume_identity(&detected),
            "the server already serves it"
        );
    }
}
