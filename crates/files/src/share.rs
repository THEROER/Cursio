//! Which of this machine's files a peer may reach, and whether it may change
//! them.
//!
//! Until this existed a paired peer got everything: `/` on Linux, every drive
//! letter on Windows, all of it writable. Pairing is trust, but "this laptop may
//! read my projects folder" and "this laptop may rewrite `C:\Windows`" are
//! different amounts of it, and the second was the only one on offer.
//!
//! ## The default is what was shared before
//!
//! An empty policy offers every detected volume read-write, exactly as before,
//! so nothing changes for a machine nobody configured. Naming any root switches
//! to an allow-list: only the named roots are offered, and a volume the policy
//! does not name — including a stick plugged in later — is not.
//!
//! ## A root is a volume or a folder
//!
//! Naming a volume's own root (`/`, `C:\`) offers that volume as it was, with
//! the mode the policy gives it. Naming anything else offers that folder as a
//! volume of its own: it gets its own entry on the peer, named after the
//! folder, and nothing above it is reachable. That last part is enforced where
//! every path is already checked ([`crate::server::Namespace::resolve`]),
//! including against a symlink inside the folder that points out of it — a
//! whole disk has nothing outside it to escape to, a folder does.
//!
//! ## Why read-only is enforced here and not by the mount
//!
//! The peer's mount does mark such a volume read-only, but the peer is the
//! party the policy constrains; a check it performs on itself is a courtesy.
//! Every mutating request is refused by this machine's server, whatever the
//! peer believes about the volume.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::FilesError;

/// One root this machine offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRoot {
    /// A volume's root or a folder, as a native absolute path.
    pub path: PathBuf,
    /// Refuse every change the peer asks for under this root.
    #[serde(default)]
    pub read_only: bool,
}

impl ShareRoot {
    /// Read one `--files-share` argument: `<path>` or `<path>:ro` / `<path>:rw`.
    ///
    /// The mode is read off the *end*, because a Windows path carries a colon
    /// of its own: `C:\:ro` is the whole C drive, read-only.
    pub fn parse(arg: &str) -> Result<Self, FilesError> {
        let (path, read_only) = match (arg.strip_suffix(":ro"), arg.strip_suffix(":rw")) {
            (Some(path), _) => (path, true),
            (_, Some(path)) => (path, false),
            _ => (arg, false),
        };
        if path.is_empty() {
            return Err(FilesError::ShareRoot(format!(
                "`{arg}` names no path; use <path> or <path>:ro"
            )));
        }
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(FilesError::ShareRoot(format!(
                "`{}` is not an absolute path; a share root is resolved on this machine, \
                 not against wherever cursio was started",
                path.display()
            )));
        }
        Ok(Self { path, read_only })
    }
}

/// What this machine offers to its peers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePolicy {
    /// The offered roots. Empty offers every detected volume read-write.
    #[serde(default)]
    pub roots: Vec<ShareRoot>,
}

impl SharePolicy {
    /// Everything detected, read-write: what sharing meant before a policy.
    pub fn everything() -> Self {
        Self::default()
    }

    /// Build a policy from `--files-share` arguments.
    pub fn from_args<S: AsRef<str>>(args: &[S]) -> Result<Self, FilesError> {
        let roots = args
            .iter()
            .map(|a| ShareRoot::parse(a.as_ref()))
            .collect::<Result<_, _>>()?;
        Ok(Self { roots })
    }

    pub fn is_everything(&self) -> bool {
        self.roots.is_empty()
    }

    /// Turn the detected volumes into the ones this policy offers.
    ///
    /// A folder root that does not exist is skipped with a warning rather than
    /// failing the module: the rest of the policy is still worth serving, and
    /// the folder may be on a disk that is not mounted yet.
    pub fn apply(&self, detected: Vec<crate::SharedVolume>) -> Vec<crate::SharedVolume> {
        if self.is_everything() {
            return detected;
        }
        let mut out: Vec<crate::SharedVolume> = Vec::new();
        for root in &self.roots {
            let offered = match detected.iter().find(|v| same_path(&v.root, &root.path)) {
                Some(volume) => {
                    let mut volume = volume.clone();
                    volume.confined = false;
                    Some(volume)
                }
                None => folder_volume(&root.path),
            };
            let Some(mut volume) = offered else {
                tracing::warn!(
                    path = %root.path.display(),
                    "not sharing a root that is not a directory on this machine"
                );
                continue;
            };
            volume.info.writable = volume.info.writable && !root.read_only;
            if out.iter().any(|v| v.info.id == volume.info.id) {
                tracing::warn!(id = %volume.info.id, "a share root is named twice; keeping the first");
                continue;
            }
            out.push(volume);
        }
        out
    }
}

/// Whether two native paths name the same place, ignoring a trailing
/// separator — `C:\` and `C:`, `/` and `//` are one root to a user.
fn same_path(a: &Path, b: &Path) -> bool {
    let trim = |p: &Path| {
        let s = p.to_string_lossy();
        let trimmed = s.trim_end_matches(['/', '\\']);
        match trimmed.is_empty() {
            true => "/".to_string(),
            false => trimmed.to_string(),
        }
    };
    trim(a) == trim(b)
}

/// A folder offered as a volume of its own.
///
/// Its id is the path with `/` separators, which is what the mount side's
/// naming reads the last component from — so a peer sees `Projects`, not a
/// path it cannot use.
fn folder_volume(path: &Path) -> Option<crate::SharedVolume> {
    if !path.is_dir() {
        return None;
    }
    let id = path.to_string_lossy().replace('\\', "/");
    let id = id.trim_end_matches('/').to_string();
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| id.clone());
    let (total_bytes, free_bytes) = crate::server::capacity_of(path);
    Some(crate::SharedVolume {
        info: cursio_protocol::Volume {
            id,
            label,
            writable: true,
            total_bytes,
            free_bytes,
            removable: false,
        },
        root: path.to_path_buf(),
        confined: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_argument_reads_its_mode_off_the_end() {
        let ro = ShareRoot::parse("/home/me/Projects:ro").expect("parses");
        assert_eq!(ro.path, PathBuf::from("/home/me/Projects"));
        assert!(ro.read_only);

        let rw = ShareRoot::parse("/srv/media:rw").expect("parses");
        assert!(!rw.read_only);

        let plain = ShareRoot::parse("/").expect("parses");
        assert_eq!(plain.path, PathBuf::from("/"));
        assert!(!plain.read_only, "unmarked means read-write, as before");
    }

    /// A Windows path has a colon of its own, and it must not be read as the
    /// mode separator.
    #[cfg(windows)]
    #[test]
    fn a_windows_drive_keeps_its_colon() {
        let ro = ShareRoot::parse(r"C:\:ro").expect("parses");
        assert_eq!(ro.path, PathBuf::from(r"C:\"));
        assert!(ro.read_only);
    }

    #[test]
    fn a_relative_or_empty_root_is_refused_with_the_reason() {
        let relative = ShareRoot::parse("Projects:ro").expect_err("relative");
        assert!(relative.to_string().contains("absolute"), "{relative}");
        assert!(ShareRoot::parse(":ro").is_err());
    }

    /// A policy is carried by the config another component writes; a missing
    /// `read_only` must mean what it means on the command line.
    #[test]
    fn a_policy_round_trips_through_json_and_defaults_to_read_write() {
        let policy: SharePolicy =
            serde_json::from_str(r#"{"roots":[{"path":"/data"}]}"#).expect("parses");
        assert!(!policy.roots[0].read_only);
        let empty: SharePolicy = serde_json::from_str("{}").expect("parses");
        assert!(empty.is_everything());
        let json = serde_json::to_string(&policy).expect("serialises");
        assert_eq!(
            serde_json::from_str::<SharePolicy>(&json).expect("back"),
            policy
        );
    }

    fn volume(id: &str, root: &Path) -> crate::SharedVolume {
        crate::SharedVolume {
            info: cursio_protocol::Volume {
                id: id.into(),
                label: id.into(),
                writable: true,
                total_bytes: 1,
                free_bytes: 1,
                removable: false,
            },
            root: root.to_path_buf(),
            confined: false,
        }
    }

    #[test]
    fn an_empty_policy_offers_everything_as_before() {
        let detected = vec![volume("/", Path::new("/"))];
        let offered = SharePolicy::everything().apply(detected.clone());
        assert_eq!(offered.len(), 1);
        assert!(offered[0].info.writable);
    }

    #[test]
    fn naming_roots_offers_only_those_with_their_modes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let projects = dir.path().join("Projects");
        std::fs::create_dir(&projects).expect("mkdir");
        let detected = vec![
            volume("/", Path::new("/")),
            volume("/media/me/STICK", Path::new("/media/me/STICK")),
        ];
        let policy =
            SharePolicy::from_args(&["/:ro".to_string(), format!("{}:rw", projects.display())])
                .expect("parses");

        let offered = policy.apply(detected);
        let ids: Vec<&str> = offered.iter().map(|v| v.info.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "the stick was not named: {ids:?}");
        assert_eq!(offered[0].info.id, "/");
        assert!(!offered[0].info.writable, "named read-only");
        assert!(
            !offered[0].confined,
            "a whole volume has nothing to escape to"
        );

        assert_eq!(crate::volume_dir_name(&offered[1].info.id), "Projects");
        assert!(offered[1].info.writable);
        assert!(offered[1].confined, "a folder must be confined to itself");
    }

    #[test]
    fn a_missing_folder_is_skipped_and_the_rest_still_offered() {
        let policy = SharePolicy::from_args(&["/definitely/not/here", "/"]).expect("parses");
        let offered = policy.apply(vec![volume("/", Path::new("/"))]);
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].info.id, "/");
    }
}
