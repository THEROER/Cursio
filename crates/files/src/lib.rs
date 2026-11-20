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
/// A path's real spelling on an owner that compares names with case, for the
/// Windows mount, which promises Windows it does not.
pub mod case;
/// The drive letter each peer volume keeps on Windows, chosen and remembered
/// here so the choice can be tested off Windows.
pub mod drive_letters;
/// The length of a file open on a mount, as far as its handle can vouch for.
pub mod filesize;
mod fnv;
/// Which mount callbacks entered and never returned, the one failure a stuck
/// caller cannot report for itself.
pub mod inflight;
/// A short-lived cache of the peer's answers about paths, so a burst of
/// repeated questions costs one round trip rather than one each.
pub mod metacache;
#[cfg(target_os = "linux")]
pub mod mount;
/// A per-file read buffer that fetches ahead while access stays sequential, so
/// many small reads share one round trip.
pub mod readahead;
/// A peer's file contents kept on this machine's disk across runs, keyed by
/// the owner's version stamp.
pub mod readcache;
/// A mount presented by another process, answered from this one's session.
pub mod relay;
pub mod server;
/// Which roots a peer may reach, each read-only or read-write.
pub mod share;
/// Which process is walking a mounted peer volume, directory after directory.
pub mod walk;
/// Windows mounting, present only where `winfsp` can be built — see this
/// crate's `Cargo.toml` for why that means MSVC rather than Windows in general
/// — and only with the `mount` feature, which is what brings the GPL-3.0
/// `winfsp-sys` in.
#[cfg(all(target_os = "windows", target_env = "msvc", feature = "mount"))]
pub mod winmount;
/// Several writes in flight against one handle, without acknowledging any of
/// them before the peer has stored it.
pub mod writeback;

pub use background::Workers;
pub use server::{FileServer, Namespace, SharedVolume};
pub use share::{SharePolicy, ShareRoot};

/// What went wrong setting up file sharing on this machine.
///
/// Requests from the peer answer with `cursio_protocol::FileError`, which is
/// what crosses the wire; this is for the local side's own configuration.
#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    /// A `--files-share` root (or its config equivalent) cannot be used.
    #[error("invalid share root: {0}")]
    ShareRoot(String),
    /// The stream to the process presenting a mount broke or carried
    /// something unreadable; see [`relay`].
    #[error("mount relay: {0}")]
    Relay(String),
}

/// Directory name a volume is presented under, on either mount.
///
/// A Windows volume id is `C:` and the Linux root is `/`, and neither makes a
/// good folder name. `/` cannot be a filename at all. The colon can, on Linux
/// — but keeping it made the mount awkward to work in: a colon separates
/// entries in `PATH`, terminates the host in an `scp`/`rsync` target, and reads
/// as a drive letter to anything parsing a path the Windows way, so tools reach
/// for the wrong half of `~/Cursio/Lightmoon/C:/Projects`. Git is the plainest
/// case, refusing such a path as a revision unless it is fenced off with `--`.
/// On Windows a colon is simply illegal in a filename.
///
/// The trailing colon carries nothing the letter does not, so it goes. Only a
/// bare drive letter is shortened: two volumes reduced to the same name would
/// collide, and both mounts resolve a lookup by comparing against this exact
/// string.
///
/// A removable medium's id is its mount path (`/media/user/MY_USB`), which is
/// worse than either: the separators would nest it several directories deep
/// under a peer that offered it as one name. Only the last component carries
/// anything a person would recognise — the label the stick was formatted with —
/// so that is what shows, and it is what makes a stick appear beside the peer's
/// disks rather than buried inside one.
///
/// The volume id on the wire is untouched — this is how a volume is *shown*,
/// not what it is called. One function because it is one decision: the two
/// mounts differ in how the OS asks, not in what a drive should be called.
pub fn volume_dir_name(id: &str) -> String {
    if id == "/" {
        return "root".to_string();
    }
    // A mount path: show the medium's own name, not the machinery around it.
    if let Some((_, last)) = id.rsplit_once('/') {
        if !last.is_empty() {
            return last.to_string();
        }
    }
    match id.strip_suffix(':') {
        Some(letter)
            if letter.chars().count() == 1 && letter.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            letter.to_string()
        }
        _ => id.to_string(),
    }
}

/// What a peer's volume is called where a person will read it: a file
/// manager's sidebar, Explorer's drive list.
///
/// The sidebar mixes the peers' disks with this machine's own, with nothing
/// grouping them, so the name has to say whose disk it is — and the disk's own
/// label cannot: a phone's is "Internal storage", which every phone offers
/// alike, and a Windows system disk's is whatever the maker formatted it as
/// ("Windows-SSD"). So a volume is named after its machine: the machine's main
/// volume (a Linux root, a phone's internal storage) by the machine's name
/// alone — `POCO X7 Pro`, `Nocturne` — and every other with which of its disks
/// it is in brackets: `Lightmoon (C)`, `Nocturne (LocalDrive500)`,
/// `POCO X7 Pro (SD card)`.
pub fn peer_volume_name(peer: &str, id: &str, removable: bool) -> String {
    let dir = volume_dir_name(id);
    let drive_letter = dir.chars().count() == 1 && dir.chars().all(|c| c.is_ascii_alphabetic());
    let main = id == "/" || (!removable && !id.contains('/') && !drive_letter);
    if main {
        peer.to_string()
    } else {
        format!("{peer} ({dir})")
    }
}

/// The file system name a peer's volume reports on Windows, `cursio-<peer>`
/// cut to what WinFsp can hold.
///
/// `FSP_FSCTL_VOLUME_PARAMS::FileSystemName` is 16 UTF-16 units read as a
/// terminated string, and the `winfsp` crate copies whatever it is given into
/// it, bounded at 192 — so a name past 16 panicked the mount. A phone called
/// "POCO X7 Pro" makes `cursio-POCO X7 Pro`, 18, and its storage never
/// mounted, retried every second. The name only fills the "File system" line
/// of a drive's properties; the label and the share carry the peer's full
/// name.
pub fn winfsp_filesystem_name(peer_name: &str) -> String {
    const UNITS: usize = 15;
    let mut name = String::new();
    let mut units = 0;
    for c in format!("cursio-{peer_name}").chars() {
        units += c.len_utf16();
        if units > UNITS {
            break;
        }
        name.push(c);
    }
    name
}

/// Now, as seconds since the epoch — for a file just created here, whose
/// timestamp the peer has not yet told us.
#[cfg(any(
    target_os = "linux",
    all(target_os = "windows", target_env = "msvc", feature = "mount")
))]
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{peer_volume_name, volume_dir_name, winfsp_filesystem_name};

    #[test]
    fn a_long_peer_name_still_fits_the_file_system_name_winfsp_holds() {
        assert_eq!(winfsp_filesystem_name("Nocturne"), "cursio-Nocturne");
        let long = winfsp_filesystem_name("POCO X7 Pro");
        assert_eq!(long, "cursio-POCO X7 ");
        assert!(long.encode_utf16().count() <= 15);
        // A character outside the BMP is two units and is never split.
        let wide = winfsp_filesystem_name("Ноут😀😀😀😀");
        assert!(wide.encode_utf16().count() <= 15);
        assert!(wide.chars().all(|c| c != char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn a_machines_main_volume_is_called_by_the_machines_name() {
        assert_eq!(
            peer_volume_name("POCO X7 Pro", "Internal storage", false),
            "POCO X7 Pro"
        );
        assert_eq!(peer_volume_name("Nocturne", "/", false), "Nocturne");
    }

    #[test]
    fn every_other_volume_says_which_of_the_machines_disks_it_is() {
        assert_eq!(peer_volume_name("Lightmoon", "C:", false), "Lightmoon (C)");
        assert_eq!(peer_volume_name("Lightmoon", "E:", true), "Lightmoon (E)");
        assert_eq!(
            peer_volume_name("Nocturne", "/mnt/LocalDrive500", false),
            "Nocturne (LocalDrive500)"
        );
        assert_eq!(
            peer_volume_name("Nocturne", "/media/theroer/MY_USB", true),
            "Nocturne (MY_USB)"
        );
        assert_eq!(
            peer_volume_name("POCO X7 Pro", "SD card", true),
            "POCO X7 Pro (SD card)"
        );
    }

    /// A removable medium is offered under its mount path, and only the last
    /// component names the stick. Showing the whole path would nest it several
    /// directories deep under the peer, which is exactly the burial that
    /// offering it as its own volume was meant to undo.
    #[test]
    fn a_removable_medium_shows_its_own_name() {
        assert_eq!(volume_dir_name("/media/theroer/MY_USB"), "MY_USB");
        assert_eq!(volume_dir_name("/run/media/theroer/KINGSTON"), "KINGSTON");
    }

    /// A label with a space survives: udisks names the mount after the
    /// filesystem label, and plenty of sticks ship with one.
    #[test]
    fn a_name_with_a_space_is_kept_whole() {
        assert_eq!(volume_dir_name("/media/theroer/MY DRIVE"), "MY DRIVE");
    }

    /// The root has to become a legal name, and a drive letter one that is
    /// comfortable to type and to hand to a tool.
    #[test]
    fn volume_names_are_legal_directory_names() {
        assert_eq!(volume_dir_name("/"), "root");
        assert_eq!(volume_dir_name("C:"), "C");
        assert_eq!(volume_dir_name("D:"), "D");
        for id in ["/", "C:", "D:"] {
            let name = volume_dir_name(id);
            assert!(!name.contains(':'), "{id} kept a colon");
            assert!(!name.contains('/'), "{id} kept a separator");
        }
    }

    /// A volume the rule does not fit keeps its name whole, rather than being
    /// quietly cut short into something another volume might also become.
    #[test]
    fn only_a_bare_drive_letter_loses_its_colon() {
        assert_eq!(volume_dir_name("Backup:"), "Backup:");
        assert_eq!(volume_dir_name("C::"), "C::");
        assert_eq!(volume_dir_name(":"), ":");
        assert_eq!(volume_dir_name("media"), "media");
    }
}
