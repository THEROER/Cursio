//! What a build says about itself: its version and the sha256 of every file.
//!
//! The shape is the LeavePulse launcher's update manifest (`components` plus
//! `deltas` keyed by source version), defined here rather than taken from its
//! SDK, which is a platform client Cursio has no other use for. Keeping the
//! same shape keeps the two updaters readable side by side.
//!
//! A manifest vouches for files; it does not vouch for itself. A build from
//! the mesh arrives with a signed statement that covers the manifest too
//! ([`super::sign`]); a directory handed to `cursio update install` is trusted
//! as far as whoever handed it over — the developer's own `xtask stage`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::UpdateError;

/// The file every build directory carries beside its executables.
pub const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The directory name this build installs under, e.g. `0.1.0+c9798e7`.
    pub version: String,
    /// Keyed by component name (`cursio`, `cursio-app`).
    pub components: BTreeMap<String, Component>,
    /// Binary patches keyed by source version, then component name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deltas: BTreeMap<String, BTreeMap<String, Delta>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    /// File name relative to the build directory.
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    /// Patch file relative to the directory the deltas were handed over in.
    pub patch_path: String,
    pub patch_sha256: String,
    /// What the patched file must hash to.
    pub result_sha256: String,
}

impl Manifest {
    pub fn read(dir: &Path) -> Result<Self, UpdateError> {
        let path = dir.join(MANIFEST_FILE);
        let bytes = fs::read(&path).map_err(|e| UpdateError::io(&path, e))?;
        serde_json::from_slice(&bytes).map_err(|e| UpdateError::Manifest {
            path: path.display().to_string(),
            reason: e.to_string(),
        })
    }

    pub fn write(&self, dir: &Path) -> Result<(), UpdateError> {
        let path = dir.join(MANIFEST_FILE);
        let text = serde_json::to_vec_pretty(self).map_err(|e| UpdateError::Manifest {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        fs::write(&path, text).map_err(|e| UpdateError::io(&path, e))
    }

    /// Describe the executables in `dir` as a build of `version`.
    pub fn describe(dir: &Path, version: &str, files: &[&str]) -> Result<Self, UpdateError> {
        let mut components = BTreeMap::new();
        for file in files {
            let name = Path::new(file)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(file)
                .to_owned();
            components.insert(
                name,
                Component {
                    path: (*file).to_owned(),
                    sha256: sha256_file(&dir.join(file))?,
                },
            );
        }
        Ok(Self {
            version: version.to_owned(),
            components,
            deltas: BTreeMap::new(),
        })
    }

    /// Confirm every component is present in `dir` with the hash this manifest
    /// vouches for, before anything installed is touched.
    pub fn verify(&self, dir: &Path) -> Result<(), UpdateError> {
        for (name, component) in &self.components {
            let actual = sha256_file(&dir.join(&component.path))?;
            if !actual.eq_ignore_ascii_case(component.sha256.trim()) {
                return Err(UpdateError::HashMismatch {
                    what: format!("component {name} ({})", component.path),
                    expected: component.sha256.trim().to_owned(),
                    actual,
                });
            }
        }
        Ok(())
    }
}

pub fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = fs::File::open(path).map_err(|e| UpdateError::io(path, e))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| UpdateError::io(path, e))?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cursio-manifest-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_described_build_verifies_and_survives_a_round_trip() {
        let dir = scratch("roundtrip");
        fs::write(dir.join("cursio"), b"node").unwrap();
        fs::write(dir.join("cursio-app"), b"app").unwrap();
        let manifest = Manifest::describe(&dir, "0.1.0+abc", &["cursio", "cursio-app"]).unwrap();
        manifest.write(&dir).unwrap();
        let read = Manifest::read(&dir).unwrap();
        assert_eq!(read, manifest);
        read.verify(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_changed_component_is_refused_by_name() {
        let dir = scratch("tamper");
        fs::write(dir.join("cursio.exe"), b"node").unwrap();
        let manifest = Manifest::describe(&dir, "v", &["cursio.exe"]).unwrap();
        fs::write(dir.join("cursio.exe"), b"other").unwrap();
        let err = manifest.verify(&dir).unwrap_err().to_string();
        assert!(err.contains("component cursio"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
