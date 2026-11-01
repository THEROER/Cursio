//! Where the node keeps what is neither config nor logs: the applications'
//! registry, the phone's screenshots.
//!
//! `cursio_protocol::paths::data_dir` answers only under an explicit root —
//! on Android, where the system hands the app its directory at runtime — and
//! is `None` on a desktop. Code that took it as the answer everywhere kept the
//! applications' registry in memory only and could not save a screenshot at
//! all on a computer; this is the desktop half beside it.

use std::path::PathBuf;

/// The node's data directory: the explicit root's when there is one, else
/// the platform's (`~/.local/share/cursio`, `%APPDATA%\cursio\cursio\data`).
pub fn data_dir() -> Option<PathBuf> {
    cursio_protocol::paths::data_dir().or_else(|| {
        directories::ProjectDirs::from("dev", "cursio", "cursio")
            .map(|dirs| dirs.data_dir().to_path_buf())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_desktop_has_a_data_directory_without_an_explicit_root() {
        assert!(super::data_dir().is_some());
    }
}
