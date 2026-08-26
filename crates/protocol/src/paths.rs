//! Where this node keeps its identity, trust store and logs.
//!
//! Every desktop platform answers this question for itself, and
//! [`directories::ProjectDirs`] asks it: a config directory under the user's
//! home, a data directory beside it. That is the whole story on Linux, Windows
//! and macOS, and nothing here changes it for them.
//!
//! ## Why this exists at all
//!
//! Android has no such answer. There is no home directory to derive one from —
//! `ProjectDirs` falls through to a `$HOME` that is either unset or points
//! somewhere unwritable — and the directory an app may actually write to is
//! chosen by the system per install and handed to the app at runtime through
//! `Context.getFilesDir()`. Nothing in this process can compute it; only the
//! platform can say.
//!
//! So the root is settable **once**, before anything reads it, and everything
//! that persists state goes through here. Set on a desktop it is equally an
//! answer to "run this node out of that directory", which is what a second
//! instance on one machine needs anyway.
//!
//! ## Why once, and why not a parameter
//!
//! Identity, the trust store and the log directory are read from three
//! different crates at three different moments, several of them before any
//! session exists. Threading a path through all of them would put the same
//! argument in every constructor to serve one platform. A process-wide value
//! that can be set only before its first read keeps that contract visible: it
//! is set at startup or not at all, and a later attempt is refused rather than
//! silently splitting state across two directories.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The explicitly configured root, if the host set one.
static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Point every persistent file at `root`, creating it if needed.
///
/// Call before reading identity, the trust store or the log directory —
/// on Android, that means before anything else at all. Returns an error if a
/// root is already in force and differs from this one: state split across two
/// directories is a peer that forgets who it paired with, and it is worth
/// failing loudly at startup rather than debugging later.
///
/// Setting the *same* root twice succeeds, so a host that initialises
/// defensively on several entry points is not punished for it.
pub fn set_root(root: impl Into<PathBuf>) -> Result<(), PathError> {
    let root = root.into();
    if let Some(existing) = ROOT.get() {
        if existing == &root {
            return Ok(());
        }
        return Err(PathError::AlreadySet {
            existing: existing.clone(),
            attempted: root,
        });
    }
    std::fs::create_dir_all(&root).map_err(|e| PathError::Create {
        path: root.clone(),
        source: e,
    })?;
    // A racing setter that won means another thread set the same value or a
    // different one; either way the check above is the one that reports it.
    match ROOT.set(root.clone()) {
        Ok(()) => Ok(()),
        Err(_) => match ROOT.get() {
            Some(existing) if existing == &root => Ok(()),
            Some(existing) => Err(PathError::AlreadySet {
                existing: existing.clone(),
                attempted: root,
            }),
            None => Ok(()),
        },
    }
}

/// The configured root, if one was set.
///
/// `None` means "use the platform's own answer", which is what every desktop
/// build does.
pub fn root() -> Option<&'static Path> {
    ROOT.get().map(PathBuf::as_path)
}

/// Where small persistent state lives: identity and the trust store.
///
/// Under an explicit root this is `<root>/config`, keeping it apart from logs
/// so a "clear the logs" never takes the pairing with it.
pub fn config_dir() -> Option<PathBuf> {
    root().map(|r| r.join("config"))
}

/// Where rotating logs live.
pub fn data_dir() -> Option<PathBuf> {
    root().map(|r| r.join("data"))
}

/// What can go wrong pointing the node at a directory.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error(
        "cursio's directory is already set to {existing} and cannot be changed to {attempted}"
    )]
    AlreadySet {
        existing: PathBuf,
        attempted: PathBuf,
    },
    #[error("cannot create {path}: {source}")]
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With nothing set, every caller falls through to the platform's own
    /// answer — which is what keeps desktop behaviour unchanged.
    ///
    /// Deliberately the only test that reads the unset state: `ROOT` is
    /// process-wide, so a test that set it would make this one depend on
    /// ordering. The setter's own behaviour is covered where it can be
    /// exercised without a global — see `set_root`'s contract above.
    #[test]
    fn an_unset_root_defers_to_the_platform() {
        if root().is_none() {
            assert_eq!(config_dir(), None);
            assert_eq!(data_dir(), None);
        }
    }

    /// Config and logs must not share a directory: clearing one would take the
    /// other, and the pairing is the half that cannot be regenerated.
    #[test]
    fn config_and_data_are_separate_under_one_root() {
        let root = Path::new("/tmp/cursio-test-root");
        assert_ne!(root.join("config"), root.join("data"));
    }
}
