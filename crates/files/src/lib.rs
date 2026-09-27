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
/// Which mount callbacks entered and never returned, the one failure a stuck
/// caller cannot report for itself.
pub mod inflight;
#[cfg(target_os = "linux")]
pub mod mount;
/// A per-file read buffer that fetches ahead while access stays sequential, so
/// many small reads share one round trip.
pub mod readahead;
pub mod server;
/// Several writes in flight against one handle, without acknowledging any of
/// them before the peer has stored it.
pub mod writeback;
/// Windows mounting, present only where `winfsp` can be built — see this
/// crate's `Cargo.toml` for why that means MSVC rather than Windows in general.
#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub mod winmount;

pub use background::Workers;
pub use server::{FileServer, SharedVolume};

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

#[cfg(test)]
mod tests {
    use super::volume_dir_name;

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
