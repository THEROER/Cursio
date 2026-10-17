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
/// The length of a file open on a mount, as far as its handle can vouch for.
pub mod filesize;
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
/// A mount presented by another process, answered from this one's session.
pub mod relay;
pub mod server;
/// Which roots a peer may reach, each read-only or read-write.
pub mod share;
/// Which process is walking a mounted peer volume, directory after directory.
pub mod walk;
/// Windows mounting, present only where `winfsp` can be built — see this
/// crate's `Cargo.toml` for why that means MSVC rather than Windows in general.
#[cfg(all(target_os = "windows", target_env = "msvc"))]
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

/// What a peer's volume is called where a person will read it.
///
/// [`volume_dir_name`] alone answers `C`, `V`, `Z` — which says nothing about
/// what the disk *is*. A bare letter is the peer's own private naming: it means
/// something on the machine that assigned it and nothing on the machine
/// reading it, and with two Windows peers in the mesh both offer a `C` that are
/// different disks. So the label leads and the letter follows in brackets, the
/// way a file manager writes it: `Games (V)`, `Windows (C)`.
///
/// Falls back to the bare directory name when the label adds nothing — it is
/// empty, or it already *is* the name, which is the case for a removable
/// medium whose id is its mount path and for a Linux root called "Root".
pub fn volume_display_name(id: &str, label: &str) -> String {
    let dir = volume_dir_name(id);
    let label = label.trim();
    if label.is_empty() || label.eq_ignore_ascii_case(&dir) {
        return dir;
    }
    // Only a drive letter gains the bracket: for anything else the directory
    // name is already the medium's own name, and "MY_USB (MY_USB)" helps
    // nobody.
    match dir.chars().count() == 1 && dir.chars().all(|c| c.is_ascii_alphabetic()) {
        true => format!("{label} ({dir})"),
        false => dir,
    }
}

#[cfg(test)]
mod tests {
    use super::{volume_dir_name, volume_display_name};

    /// A drive letter alone does not say what the disk is.
    ///
    /// `C`, `V`, `Z` are the peer's private naming: they mean something on the
    /// machine that assigned them and nothing on the machine reading them —
    /// and two Windows peers each offer a `C` that are different disks.
    #[test]
    fn a_drive_letter_is_shown_with_the_name_of_its_disk() {
        assert_eq!(volume_display_name("C:", "Windows"), "Windows (C)");
        assert_eq!(volume_display_name("V:", "Games"), "Games (V)");
    }

    /// A medium already named by its own label gains nothing from repeating it.
    #[test]
    fn a_name_is_not_doubled_when_it_says_the_same_thing() {
        assert_eq!(
            volume_display_name("/media/theroer/MY_USB", "MY_USB"),
            "MY_USB"
        );
        assert_eq!(volume_display_name("/", "Root"), "root");
        // Nothing to add: the label is empty.
        assert_eq!(volume_display_name("C:", ""), "C");
        assert_eq!(volume_display_name("C:", "   "), "C");
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
