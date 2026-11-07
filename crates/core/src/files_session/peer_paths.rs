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
//! of its peer's volumes are mounted, and the clipboard asks here.
//!
//! ## Mapping a path
//!
//! A volume's id is where it lives on its owner: a mount path on Linux (`/`,
//! `/mnt/data`), a drive on Windows (`C:`), a folder for a shared folder
//! (`/home/me/Projects`, `C:/Users/me/Projects`). The longest id a copied path
//! sits under names its volume, and the rest of the path is the same inside
//! the mount. A path under no mounted volume has no place here and is left
//! out — a peer that shares only a folder cannot be pasted from outside it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use cursio_protocol::Volume;

/// The engine that published, and the volumes it has mounted.
type Published = (u64, Vec<Volume>);

/// Per peer, what its engine last published.
static MOUNTED: Mutex<Option<HashMap<String, Published>>> = Mutex::new(None);

static NEXT_PUBLISHER: AtomicU64 = AtomicU64::new(1);

/// An identity for one engine's publications.
///
/// A link replaced by a new one starts its engine before the old one shuts
/// down, so withdrawing by peer name alone would take away the new engine's
/// mounts along with the old one's.
pub(super) fn publisher() -> u64 {
    NEXT_PUBLISHER.fetch_add(1, Ordering::Relaxed)
}

/// Record which of `peer`'s volumes are mounted here now.
pub(super) fn publish(peer: &str, by: u64, volumes: Vec<Volume>) {
    MOUNTED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(peer.to_string(), (by, volumes));
}

/// Forget `peer`'s mounts, if `by` is still the engine that published them.
pub(super) fn withdraw(peer: &str, by: u64) {
    if let Some(map) = MOUNTED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        if map.get(peer).is_some_and(|(owner, _)| *owner == by) {
            map.remove(peer);
        }
    }
}

/// Where `path`, spelled as `peer` spells it (`/home/me/a.png`,
/// `C:/Users/me/a.png`), can be opened here — `None` when no volume holding it
/// is mounted.
pub fn path_here(peer: &str, path: &str) -> Option<PathBuf> {
    let held = MOUNTED.lock().unwrap_or_else(PoisonError::into_inner);
    let (_, volumes) = held.as_ref()?.get(peer)?;
    let (volume, rest) = locate(volumes, path)?;
    mounted_path(peer, volume, &rest)
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

#[cfg(target_os = "linux")]
fn mounted_path(peer: &str, volume: &Volume, rest: &str) -> Option<PathBuf> {
    Some(super::mounts::volume_mount_point(peer, volume).join(rest))
}

/// The volume's UNC share rather than its drive letter: the letter is chosen
/// by the mount and may sit in a session this process cannot see (the helper
/// mounts in Explorer's), while the share is named from the volume alone and
/// reaches the same files from either (`winmount` publishes it).
#[cfg(all(target_os = "windows", target_env = "msvc"))]
fn mounted_path(peer: &str, volume: &Volume, rest: &str) -> Option<PathBuf> {
    let share = cursio_files::volume_dir_name(&volume.id);
    let mut path = format!(r"\\cursio\{peer}-{share}");
    for part in rest.split('/').filter(|p| !p.is_empty()) {
        path.push('\\');
        path.push_str(part);
    }
    Some(PathBuf::from(path))
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

    /// A replaced link's engine shuts down after the new one has published;
    /// its withdrawal must not take the new engine's mounts with it.
    #[test]
    fn only_the_engine_that_published_withdraws() {
        let (old, new) = (publisher(), publisher());
        publish("withdraw-test", old, vec![volume("/")]);
        publish("withdraw-test", new, vec![volume("/")]);
        withdraw("withdraw-test", old);
        let held = MOUNTED.lock().unwrap();
        assert!(held.as_ref().unwrap().contains_key("withdraw-test"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_copied_path_opens_inside_the_peers_mount() {
        let by = publisher();
        publish("Lightmoon-paths", by, vec![volume("C:")]);
        let here = path_here("Lightmoon-paths", "C:/Users/me/a.png").unwrap();
        assert!(
            here.ends_with("Cursio/Lightmoon-paths/Lightmoon-paths (C)/Users/me/a.png"),
            "{}",
            here.display()
        );
        withdraw("Lightmoon-paths", by);
        assert!(path_here("Lightmoon-paths", "C:/Users/me/a.png").is_none());
    }
}
