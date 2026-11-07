//! Where a peer's file can be opened on this machine.
//!
//! A copy in a file manager is a list of paths on the machine it was made on,
//! and pasted on a peer those name nothing: a picture copied in Nautilus
//! arrived in Explorer as the text `/home/…/a.png`. The files module already
//! mounts each of the peer's volumes here, so a copied path is a path inside
//! one of those mounts — and pasting it hands the program a file it reads
//! through the mount, fetched only as it is read, folders included. Nothing
//! crosses ahead of the paste, which is the files module's own rule.
//!
//! The mounts belong to the files module and the paste to the clipboard's, and
//! the two never see each other's state. So each files engine publishes which
//! of its peer's volumes are mounted and which of this machine's it serves,
//! and the clipboard asks here.
//!
//! ## Mapping a path
//!
//! A volume's id is where it lives on its owner: a mount path on Linux (`/`,
//! `/mnt/data`), a drive on Windows (`C:`), a folder for a shared folder
//! (`/home/me/Projects`, `C:/Users/me/Projects`). The longest id a copied path
//! sits under names its volume, and the rest of the path is the same inside
//! the mount. A path under no mounted volume has no place here and is left
//! out — a peer that shares only a folder cannot be pasted from outside it.
//!
//! ## A path through a mount
//!
//! A file copied inside the peer's mount of some machine's disk is named by
//! that mount, not by the peer's own disk — and the peer refuses to serve its
//! mounts back, so mapping it like any other path would hand over a path that
//! fails. Such a path is recognised by the mount's own spelling and taken to
//! the machine whose disk it really is: this machine's own folder when it is
//! ours, our mount of a third machine when that one is mounted here too.
//!
//! - Linux: `…/Cursio/<machine>/<volume>/…`, the mount point
//!   `files_session::mounts` makes, with `<volume>` from
//!   [`cursio_files::peer_volume_name`].
//! - Windows: `//cursio/<machine>-<share>/…`, the UNC share `winmount`
//!   publishes. Explorer names a file by its drive letter instead, which says
//!   nothing about the machine, so the copying side rewrites its own Cursio
//!   letters into the share before the list leaves ([`shared_spelling`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use cursio_protocol::Volume;

#[derive(Default)]
struct State {
    /// Per peer: the engine that published, and its volumes mounted here.
    mounted: HashMap<String, (u64, Vec<Volume>)>,
    /// This machine's volumes as it serves them, with where each lives.
    own: Vec<(Volume, PathBuf)>,
    /// A Cursio drive letter (uppercase) and what it mounts: the engine that
    /// made it, the peer, and the volume id.
    drives: HashMap<char, (u64, String, String)>,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

static NEXT_PUBLISHER: AtomicU64 = AtomicU64::new(1);

/// An identity for one engine's publications.
///
/// A link replaced by a new one starts its engine before the old one shuts
/// down, so withdrawing by peer name alone would take away the new engine's
/// mounts along with the old one's.
pub(super) fn publisher() -> u64 {
    NEXT_PUBLISHER.fetch_add(1, Ordering::Relaxed)
}

/// Record which of `peer`'s volumes are mounted here now. Called only where
/// there is a mount backend.
#[cfg_attr(
    not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))),
    allow(dead_code)
)]
pub(super) fn publish(peer: &str, by: u64, volumes: Vec<Volume>) {
    state().mounted.insert(peer.to_string(), (by, volumes));
}

/// Forget what engine `by` published about `peer`, if it is still its.
pub(super) fn withdraw(peer: &str, by: u64) {
    let mut state = state();
    if state
        .mounted
        .get(peer)
        .is_some_and(|(owner, _)| *owner == by)
    {
        state.mounted.remove(peer);
    }
    state.drives.retain(|_, (owner, _, _)| *owner != by);
}

/// Record the volumes this machine serves, and where each lives.
pub(super) fn publish_own(volumes: Vec<(Volume, PathBuf)>) {
    state().own = volumes;
}

/// Record that engine `by` mounted `peer`'s `volume` on `mountpoint` (`Z:`).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn note_drive(by: u64, peer: &str, volume: &str, mountpoint: &str) {
    if let Some(letter) = drive_letter(mountpoint) {
        state()
            .drives
            .insert(letter, (by, peer.to_string(), volume.to_string()));
    }
}

/// Forget the letter engine `by` held for `volume`, which went away.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(super) fn forget_drive(by: u64, volume: &str) {
    state()
        .drives
        .retain(|_, (owner, _, id)| !(*owner == by && id == volume));
}

/// `path` (as [`ClipboardContent::copied_file_paths`] spells it) with a Cursio
/// drive letter replaced by the share it mounts, so a machine reading it can
/// tell whose disk it is. Any other path comes back unchanged.
///
/// [`ClipboardContent::copied_file_paths`]: clipboard_core::ClipboardContent::copied_file_paths
pub fn shared_spelling(path: &str) -> String {
    let Some(letter) = drive_letter(path.get(..2).unwrap_or_default()) else {
        return path.to_string();
    };
    let rest = &path[2..];
    if !(rest.is_empty() || rest.starts_with('/')) {
        return path.to_string();
    }
    match state().drives.get(&letter) {
        Some((_, peer, volume)) => {
            let share = cursio_files::volume_dir_name(volume);
            format!("//cursio/{peer}-{share}{rest}")
        }
        None => path.to_string(),
    }
}

/// Where `path`, copied on `peer` and spelled as it spells it
/// (`/home/me/a.png`, `C:/Users/me/a.png`), can be opened here — `None` when no
/// volume holding it is reachable. `me` is this machine's name as peers know it.
pub fn path_here(me: &str, peer: &str, path: &str) -> Option<PathBuf> {
    let state = state();
    if let Some(through) = through_mount(&state, me, path) {
        return through;
    }
    let (_, volumes) = state.mounted.get(peer)?;
    let (volume, rest) = locate(volumes, path)?;
    mounted_path(peer, volume, &rest)
}

/// A path inside some machine's mount, resolved to where that disk is reached
/// from here. The outer `None` is "not a path through a mount"; the inner one
/// is a mount of a machine whose disk cannot be reached from here.
fn through_mount(state: &State, me: &str, path: &str) -> Option<Option<PathBuf>> {
    let (machine, volume_name, rest) = split_mount_path(state, me, path)?;
    if machine == me {
        let (_, root) = state.own.iter().find(|(v, _)| volume_name.matches(me, v))?;
        return Some(Some(join(root, &rest)));
    }
    let resolved = state.mounted.get(&machine).and_then(|(_, volumes)| {
        let volume = volumes.iter().find(|v| volume_name.matches(&machine, v))?;
        mounted_path(&machine, volume, &rest)
    });
    Some(resolved)
}

/// How a mount names its volume.
enum MountName<'a> {
    /// The Linux mount point's last component, `peer_volume_name`.
    Dir(&'a str),
    /// The Windows share's suffix, `volume_dir_name`, compared without case.
    Share(&'a str),
}

impl MountName<'_> {
    fn matches(&self, machine: &str, volume: &Volume) -> bool {
        match self {
            MountName::Dir(name) => {
                *name == cursio_files::peer_volume_name(machine, &volume.id, volume.removable)
            }
            MountName::Share(share) => {
                cursio_files::volume_dir_name(&volume.id).eq_ignore_ascii_case(share)
            }
        }
    }
}

/// The machine, volume and path inside it that a mount path names, for a
/// machine this one knows: itself or a peer whose disks are mounted here.
fn split_mount_path<'a>(
    state: &State,
    me: &str,
    path: &'a str,
) -> Option<(String, MountName<'a>, String)> {
    let known = || std::iter::once(me).chain(state.mounted.keys().map(String::as_str));

    if let Some(after) = strip_prefix_ignore_case(path, "//cursio/") {
        let (share, rest) = after.split_once('/').unwrap_or((after, ""));
        // The longest name first: `dark` must not claim `dark-laptop-root`.
        let machine = known()
            .filter(|m| {
                share.len() > m.len()
                    && share.as_bytes()[m.len()] == b'-'
                    && share[..m.len()].eq_ignore_ascii_case(m)
            })
            .max_by_key(|m| m.len())?;
        let volume = &share[machine.len() + 1..];
        return Some((
            machine.to_string(),
            MountName::Share(volume),
            rest.to_string(),
        ));
    }

    for (at, _) in path.match_indices("/Cursio/") {
        let after = &path[at + "/Cursio/".len()..];
        let Some((machine, inner)) = after.split_once('/') else {
            continue;
        };
        if !known().any(|m| m == machine) {
            continue;
        }
        let (volume, rest) = inner.split_once('/').unwrap_or((inner, ""));
        return Some((
            machine.to_string(),
            MountName::Dir(volume),
            rest.to_string(),
        ));
    }
    None
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// The uppercase letter of a drive spelled `Z:`.
fn drive_letter(spelled: &str) -> Option<char> {
    match spelled.as_bytes() {
        [letter, b':'] if letter.is_ascii_alphabetic() => Some(letter.to_ascii_uppercase() as char),
        _ => None,
    }
}

/// The mounted volume holding `path`, and the path inside it.
fn locate<'a>(volumes: &'a [Volume], path: &str) -> Option<(&'a Volume, String)> {
    volumes
        .iter()
        .filter_map(|v| inside(&v.id, path).map(|rest| (v, rest)))
        .max_by_key(|(v, _)| v.id.len())
}

/// `path` relative to the volume `id`, if it lies under it.
fn inside(id: &str, path: &str) -> Option<String> {
    // A drive, or a folder on one, compares without case, as Windows does.
    let drive = id.as_bytes().get(1) == Some(&b':');
    let root = id.trim_end_matches('/');
    let head = path.get(..root.len())?;
    let same = if drive {
        head.eq_ignore_ascii_case(root)
    } else {
        head == root
    };
    let rest = &path[root.len()..];
    // On a component boundary: `/mnt/data` does not hold `/mnt/database`.
    if !same || !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    Some(rest.trim_start_matches('/').to_string())
}

/// `rest`, `/`-separated, under `root`, in this OS's separators.
fn join(root: &Path, rest: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in rest.split('/').filter(|p| !p.is_empty()) {
        path.push(part);
    }
    path
}

#[cfg(target_os = "linux")]
fn mounted_path(peer: &str, volume: &Volume, rest: &str) -> Option<PathBuf> {
    Some(join(&super::mounts::volume_mount_point(peer, volume), rest))
}

/// The volume's UNC share rather than its drive letter: the letter is chosen
/// by the mount and may sit in a session this process cannot see (the helper
/// mounts in Explorer's), while the share is named from the volume alone and
/// reaches the same files from either (`winmount` publishes it).
#[cfg(all(target_os = "windows", target_env = "msvc"))]
fn mounted_path(peer: &str, volume: &Volume, rest: &str) -> Option<PathBuf> {
    let share = cursio_files::volume_dir_name(&volume.id);
    Some(join(Path::new(&format!(r"\\cursio\{peer}-{share}\")), rest))
}

/// Nothing is mounted where there is no mount backend.
#[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
fn mounted_path(_peer: &str, _volume: &Volume, _rest: &str) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(id: &str) -> Volume {
        Volume {
            id: id.to_string(),
            label: id.to_string(),
            writable: true,
            total_bytes: 0,
            free_bytes: 0,
            removable: false,
        }
    }

    fn with(mounted: &[(&str, &[&str])], own: &[(&str, &str)]) -> State {
        State {
            mounted: mounted
                .iter()
                .map(|(peer, ids)| {
                    (
                        peer.to_string(),
                        (1, ids.iter().map(|id| volume(id)).collect()),
                    )
                })
                .collect(),
            own: own
                .iter()
                .map(|(id, root)| (volume(id), PathBuf::from(root)))
                .collect(),
            drives: HashMap::new(),
        }
    }

    #[test]
    fn the_deepest_volume_holding_a_path_is_the_one_it_is_on() {
        let volumes = [volume("/"), volume("/mnt/data")];
        let (v, rest) = locate(&volumes, "/mnt/data/photos/a.png").unwrap();
        assert_eq!(
            (v.id.as_str(), rest.as_str()),
            ("/mnt/data", "photos/a.png")
        );
        let (v, rest) = locate(&volumes, "/mnt/database/x").unwrap();
        assert_eq!((v.id.as_str(), rest.as_str()), ("/", "mnt/database/x"));
    }

    #[test]
    fn a_drive_matches_without_case_and_a_linux_path_does_not() {
        let volumes = [volume("C:"), volume("/home/me/Projects")];
        let (v, rest) = locate(&volumes, "c:/Users/me/a b.jpg").unwrap();
        assert_eq!((v.id.as_str(), rest.as_str()), ("C:", "Users/me/a b.jpg"));
        assert!(locate(&volumes, "/home/me/projects/x").is_none());
        let (_, rest) = locate(&volumes, "/home/me/Projects").unwrap();
        assert_eq!(rest, "", "the shared folder itself");
    }

    #[test]
    fn a_path_on_no_mounted_volume_has_no_place_here() {
        assert!(locate(&[volume("C:")], "D:/x").is_none());
        assert!(locate(&[volume("C:")], "/home/me/x").is_none());
        assert!(locate(&[], "/x").is_none());
    }

    /// Copied in Nautilus inside the peer's mount of this machine's `C:`, and
    /// pasted here: the file is ours, and the peer will not serve it back.
    #[test]
    fn a_file_copied_through_the_peers_mount_of_our_disk_comes_home() {
        let state = with(&[("Nocturne", &["/"])], &[("C:", "C:/")]);
        let got = through_mount(
            &state,
            "Lightmoon",
            "/home/me/Cursio/Lightmoon/Lightmoon (C)/Users/me/a.png",
        );
        assert_eq!(got, Some(Some(join(Path::new("C:/"), "Users/me/a.png"))));
    }

    /// Copied in Explorer from the share of our root, the Windows spelling.
    #[test]
    fn a_file_copied_through_a_share_of_our_disk_comes_home() {
        let state = with(&[("Lightmoon", &["C:"])], &[("/", "/")]);
        let got = through_mount(&state, "Nocturne", "//CURSIO/nocturne-root/home/me/a.png");
        assert_eq!(got, Some(Some(PathBuf::from("/home/me/a.png"))));
    }

    /// A machine whose name starts like another's is not mistaken for it.
    #[test]
    fn the_longest_machine_name_owns_a_share() {
        let state = with(&[("dark", &["/"]), ("dark-laptop", &["/"])], &[]);
        let (machine, _, rest) =
            split_mount_path(&state, "me", "//cursio/dark-laptop-root/x").unwrap();
        assert_eq!((machine.as_str(), rest.as_str()), ("dark-laptop", "x"));
    }

    /// A folder that is merely called `Cursio` names no machine.
    #[test]
    fn a_cursio_folder_naming_no_known_machine_is_an_ordinary_path() {
        let state = with(&[("Lightmoon", &["C:"])], &[]);
        assert!(through_mount(&state, "Nocturne", "/home/me/Cursio/notes/x.txt").is_none());
    }

    #[test]
    fn only_the_engine_that_published_withdraws() {
        let (old, new) = (publisher(), publisher());
        publish("withdraw-test", old, vec![volume("/")]);
        publish("withdraw-test", new, vec![volume("/")]);
        withdraw("withdraw-test", old);
        assert!(state().mounted.contains_key("withdraw-test"));
        withdraw("withdraw-test", new);
    }

    /// Explorer names a file on a Cursio drive by its letter; the list leaves
    /// with the share instead, which names the machine.
    #[test]
    fn a_cursio_drive_letter_is_sent_as_its_share() {
        let by = publisher();
        note_drive(by, "Nocturne-letter", "/", "Y:");
        assert_eq!(
            shared_spelling("y:/home/me/a.png"),
            "//cursio/Nocturne-letter-root/home/me/a.png"
        );
        assert_eq!(shared_spelling("Y:"), "//cursio/Nocturne-letter-root");
        assert_eq!(shared_spelling("C:/Users/a.png"), "C:/Users/a.png");
        forget_drive(by, "/");
        assert_eq!(shared_spelling("Y:/home/me/a.png"), "Y:/home/me/a.png");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_copied_path_opens_inside_the_peers_mount() {
        let by = publisher();
        publish("Lightmoon-paths", by, vec![volume("C:")]);
        let here = path_here("me", "Lightmoon-paths", "C:/Users/me/a.png").unwrap();
        assert!(
            here.ends_with("Cursio/Lightmoon-paths/Lightmoon-paths (C)/Users/me/a.png"),
            "{}",
            here.display()
        );
        withdraw("Lightmoon-paths", by);
        assert!(path_here("me", "Lightmoon-paths", "C:/Users/me/a.png").is_none());
    }
}
