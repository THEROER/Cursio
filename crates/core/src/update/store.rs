//! Builds this machine can hand to its peers, and builds arriving from them.
//!
//! ```text
//! <root>/store/<version>/offer          the signed statement (lane encoding)
//! <root>/store/<version>/<platform>/…   that platform's files
//! <root>/store/<version>/<platform>/.complete
//! ```
//!
//! Separate from `versions/`, which holds only what this machine can run: a
//! Linux machine that published a Windows build keeps the Windows files here
//! and serves them, and a Windows machine that received one keeps a copy here
//! after installing it, so the next peer can be served by whichever machine
//! is up.
//!
//! **Names arrive off a socket.** A version, platform or file name in a
//! request is a string another process chose, so each is checked to be one
//! plain path segment ([`layout::valid_version`]'s rule) before it is joined
//! to anything. And nothing is served but what the signed statement lists,
//! from a platform marked complete — so a request can name only a file this
//! machine already vouched for.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cursio_protocol::update::{Artifact, Offer, Statement, UpdateMsg};
use sha2::{Digest, Sha256};

use super::layout::{self, InstallLayout};
use super::manifest::{Manifest, MANIFEST_FILE};
use super::sign::BuilderKey;
use super::UpdateError;

const OFFER_FILE: &str = "offer";
const COMPLETE_MARK: &str = ".complete";
const SEEN_SIGNERS: &str = "seen-signers.json";

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// A build held here: what was signed, and which platforms are here in full.
#[derive(Debug, Clone)]
pub struct Held {
    pub offer: Offer,
    pub statement: Statement,
}

impl Store {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn of(layout: &InstallLayout) -> Self {
        Self::at(layout.root().join("store"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn version_dir(&self, version: &str) -> Result<PathBuf, UpdateError> {
        check_name(version)?;
        Ok(self.root.join(version))
    }

    pub fn platform_dir(&self, version: &str, platform: &str) -> Result<PathBuf, UpdateError> {
        check_name(platform)?;
        Ok(self.version_dir(version)?.join(platform))
    }

    /// Every build held with at least one platform complete, newest first,
    /// with [`Offer::have`] filled from the disk.
    pub fn held(&self) -> Vec<Held> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<Held> = entries
            .filter_map(Result::ok)
            .filter_map(|e| self.load(&e.file_name().into_string().ok()?))
            .filter(|h| !h.offer.have.is_empty())
            .collect();
        out.sort_by_key(|h| std::cmp::Reverse(h.statement.published_at));
        out
    }

    /// The build `version`, if its signed statement is here.
    pub fn load(&self, version: &str) -> Option<Held> {
        let dir = self.version_dir(version).ok()?;
        let bytes = fs::read(dir.join(OFFER_FILE)).ok()?;
        let Ok(Some(UpdateMsg::Offer(mut offer))) = UpdateMsg::decode(&bytes) else {
            return None;
        };
        let statement = Statement::from_bytes(&offer.statement).ok()?;
        if statement.version != version {
            return None;
        }
        let mut have: Vec<String> = statement
            .artifacts
            .iter()
            .map(|a| a.platform.clone())
            .collect();
        have.sort();
        have.dedup();
        have.retain(|p| dir.join(p).join(COMPLETE_MARK).is_file());
        offer.have = have;
        Some(Held { offer, statement })
    }

    pub fn is_complete(&self, version: &str, platform: &str) -> bool {
        self.platform_dir(version, platform)
            .is_ok_and(|d| d.join(COMPLETE_MARK).is_file())
    }

    /// Put builds staged by `cargo xtask stage` here, signed as one version.
    ///
    /// Every directory must carry the same version: a statement vouches for
    /// one build, whatever platforms it was compiled for.
    pub fn publish(
        &self,
        key: &BuilderKey,
        channel: cursio_protocol::update::Channel,
        builds: &[(String, PathBuf)],
        published_at: u64,
    ) -> Result<Statement, UpdateError> {
        let mut version: Option<String> = None;
        let mut artifacts = Vec::new();
        for (platform, dir) in builds {
            check_name(platform)?;
            let manifest = Manifest::read(dir)?;
            manifest.verify(dir)?;
            match &version {
                Some(v) if *v != manifest.version => {
                    return Err(UpdateError::Refused(format!(
                        "{} is build {} and another is {v}; publish one build at a time",
                        dir.display(),
                        manifest.version
                    )))
                }
                _ => version = Some(manifest.version.clone()),
            }
            let mut names: Vec<String> = manifest
                .components
                .values()
                .map(|c| c.path.clone())
                .collect();
            names.push(MANIFEST_FILE.into());
            let target = self.platform_dir(&manifest.version, platform)?;
            fs::create_dir_all(&target).map_err(|e| UpdateError::io(&target, e))?;
            for name in names {
                check_name(&name)?;
                let from = dir.join(&name);
                let to = target.join(&name);
                fs::copy(&from, &to).map_err(|e| UpdateError::io(&to, e))?;
                let (size, sha256) = measure(&to)?;
                artifacts.push(Artifact {
                    platform: platform.clone(),
                    name,
                    size,
                    sha256,
                });
            }
            mark_complete(&target)?;
        }
        let version = version.ok_or_else(|| UpdateError::Refused("nothing to publish".into()))?;
        let statement = Statement {
            version: version.clone(),
            channel,
            published_at,
            artifacts,
        };
        self.remember(&key.sign(&statement, Vec::new()))?;
        Ok(statement)
    }

    /// Keep an offer's signed statement, before any of its files arrive.
    pub fn remember(&self, offer: &Offer) -> Result<(), UpdateError> {
        let statement = Statement::from_bytes(&offer.statement)
            .map_err(|e| UpdateError::Refused(e.to_string()))?;
        let dir = self.version_dir(&statement.version)?;
        fs::create_dir_all(&dir).map_err(|e| UpdateError::io(&dir, e))?;
        let bare = Offer {
            have: Vec::new(),
            ..offer.clone()
        };
        let path = dir.join(OFFER_FILE);
        fs::write(&path, UpdateMsg::Offer(bare).encode()).map_err(|e| UpdateError::io(&path, e))
    }

    /// Up to `length` bytes of one file of a complete platform, from `offset`.
    pub fn read(
        &self,
        version: &str,
        platform: &str,
        name: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, UpdateError> {
        check_name(name)?;
        let held = self
            .load(version)
            .ok_or_else(|| UpdateError::Refused(format!("build {version} is not held here")))?;
        if !held.offer.have.iter().any(|p| p == platform)
            || !held
                .statement
                .for_platform(platform)
                .any(|a| a.name == name)
        {
            return Err(UpdateError::Refused(format!(
                "{platform}/{name} of {version} is not held here"
            )));
        }
        let path = self.platform_dir(version, platform)?.join(name);
        let mut file = fs::File::open(&path).map_err(|e| UpdateError::io(&path, e))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| UpdateError::io(&path, e))?;
        let mut out = Vec::with_capacity(length);
        file.take(length as u64)
            .read_to_end(&mut out)
            .map_err(|e| UpdateError::io(&path, e))?;
        Ok(out)
    }

    /// Write received bytes into the partial copy of a file.
    pub fn write(
        &self,
        version: &str,
        platform: &str,
        name: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<(), UpdateError> {
        check_name(name)?;
        let dir = self.platform_dir(version, platform)?;
        fs::create_dir_all(&dir).map_err(|e| UpdateError::io(&dir, e))?;
        let path = dir.join(format!("{name}.partial"));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(offset == 0)
            .open(&path)
            .map_err(|e| UpdateError::io(&path, e))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| UpdateError::io(&path, e))?;
        file.write_all(data).map_err(|e| UpdateError::io(&path, e))
    }

    /// Check a received file against the statement and move it into place.
    pub fn finish_file(&self, version: &str, artifact: &Artifact) -> Result<(), UpdateError> {
        check_name(&artifact.name)?;
        let dir = self.platform_dir(version, &artifact.platform)?;
        let partial = dir.join(format!("{}.partial", artifact.name));
        let (size, sha256) = measure(&partial)?;
        if size != artifact.size || sha256 != artifact.sha256 {
            let _ = fs::remove_file(&partial);
            return Err(UpdateError::HashMismatch {
                what: format!("{}/{} of {version}", artifact.platform, artifact.name),
                expected: super::sign::hex(&artifact.sha256),
                actual: super::sign::hex(&sha256),
            });
        }
        let target = dir.join(&artifact.name);
        fs::rename(&partial, &target).map_err(|e| UpdateError::io(&target, e))
    }

    /// Mark a platform whose every file has been checked as servable.
    pub fn finish_platform(&self, version: &str, platform: &str) -> Result<PathBuf, UpdateError> {
        let dir = self.platform_dir(version, platform)?;
        mark_complete(&dir)?;
        Ok(dir)
    }

    /// Remember which key a machine signed an offered build with, so trusting
    /// it later needs no key copied by hand. Only ever a suggestion: the key
    /// is pinned when the user says so, not when it is seen.
    pub fn note_signer(&self, machine: &str, key: &[u8; 32]) {
        let mut seen = self.seen_signers();
        if seen.get(machine).map(String::as_str) == Some(super::sign::hex(key).as_str()) {
            return;
        }
        seen.insert(machine.to_owned(), super::sign::hex(key));
        let path = self.root.join(SEEN_SIGNERS);
        let written = fs::create_dir_all(&self.root)
            .and_then(|()| fs::write(&path, serde_json::to_vec_pretty(&seen).unwrap_or_default()));
        if let Err(e) = written {
            tracing::debug!(error = %e, "could not remember a builder's key");
        }
    }

    pub fn seen_signer(&self, machine: &str) -> Option<String> {
        self.seen_signers().remove(machine)
    }

    fn seen_signers(&self) -> std::collections::BTreeMap<String, String> {
        fs::read(self.root.join(SEEN_SIGNERS))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Delete held builds past the newest `keep`.
    pub fn prune(&self, keep: usize) -> Vec<String> {
        let mut gone = Vec::new();
        for held in self.held().into_iter().skip(keep) {
            if let Ok(dir) = self.version_dir(&held.statement.version) {
                if fs::remove_dir_all(&dir).is_ok() {
                    gone.push(held.statement.version);
                }
            }
        }
        gone
    }
}

fn check_name(name: &str) -> Result<(), UpdateError> {
    match layout::valid_version(name) {
        true => Ok(()),
        false => Err(UpdateError::Refused(format!(
            "`{name}` is not a plain file or directory name"
        ))),
    }
}

fn mark_complete(dir: &Path) -> Result<(), UpdateError> {
    let path = dir.join(COMPLETE_MARK);
    fs::write(&path, b"").map_err(|e| UpdateError::io(&path, e))
}

fn measure(path: &Path) -> Result<(u64, [u8; 32]), UpdateError> {
    let mut file = fs::File::open(path).map_err(|e| UpdateError::io(path, e))?;
    let mut hasher = Sha256::new();
    let size = std::io::copy(&mut file, &mut hasher).map_err(|e| UpdateError::io(path, e))?;
    Ok((size, hasher.finalize().into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::update::Channel;

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cursio-store-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn staged(dir: &Path, version: &str, exe: &[u8]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("cursio"), exe).unwrap();
        Manifest::describe(dir, version, &["cursio"])
            .unwrap()
            .write(dir)
            .unwrap();
        dir.to_owned()
    }

    #[test]
    fn a_published_build_is_held_and_served_only_by_the_files_it_signed() {
        let root = scratch("publish");
        let store = Store::at(root.join("store"));
        let build = staged(&root.join("stage"), "0.1.0+a", b"node-bytes");
        let key = BuilderKey::from_seed([4; 32]);
        let statement = store
            .publish(&key, Channel::Dev, &[("linux-x86_64".into(), build)], 5)
            .unwrap();
        assert_eq!(statement.artifacts.len(), 2);

        let held = store.held();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].offer.have, vec!["linux-x86_64".to_owned()]);
        assert_eq!(
            store
                .read("0.1.0+a", "linux-x86_64", "cursio", 5, 100)
                .unwrap(),
            b"bytes"
        );
        assert!(store
            .read("0.1.0+a", "linux-x86_64", "offer", 0, 10)
            .is_err());
        assert!(store.read("0.1.0+a", "..", "cursio", 0, 10).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_key_a_machine_last_signed_with_is_remembered_by_its_name() {
        let root = scratch("seen");
        let store = Store::at(root.join("store"));
        assert_eq!(store.seen_signer("nocturne"), None);
        store.note_signer("nocturne", &[1; 32]);
        store.note_signer("nocturne", &[2; 32]);
        assert_eq!(
            store.seen_signer("nocturne"),
            Some(crate::update::sign::hex(&[2; 32]))
        );
        assert!(store.held().is_empty(), "the record is not taken for a build");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_received_file_that_does_not_match_its_statement_is_discarded() {
        let root = scratch("tamper");
        let store = Store::at(root.join("store"));
        let artifact = Artifact {
            platform: "p".into(),
            name: "cursio".into(),
            size: 3,
            sha256: Sha256::digest(b"abc").into(),
        };
        store.write("v", "p", "cursio", 0, b"abd").unwrap();
        assert!(store.finish_file("v", &artifact).is_err());
        assert!(!store
            .platform_dir("v", "p")
            .unwrap()
            .join("cursio")
            .exists());

        store.write("v", "p", "cursio", 0, b"ab").unwrap();
        store.write("v", "p", "cursio", 2, b"c").unwrap();
        store.finish_file("v", &artifact).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}
