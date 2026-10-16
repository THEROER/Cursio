//! Stable node identity.
//!
//! Each machine needs a [`PeerId`] that survives restarts so peers recognise it
//! across sessions and the trust store stays valid. We generate one on first
//! run and persist it (alongside a display name) as JSON in the platform config
//! directory.

use std::path::PathBuf;

use cursio_protocol::PeerId;
use serde::{Deserialize, Serialize};

/// This node's persistent identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    /// Stable id, generated once and reused forever.
    pub peer_id: PeerId,
    /// Human-friendly name shown to peers (defaults to the hostname).
    pub display_name: String,
}

impl NodeIdentity {
    /// Load the identity from disk, generating and persisting a new one on
    /// first run.
    pub fn load_or_create() -> anyhow::Result<Self> {
        let path = Self::path()?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self {
                    peer_id: PeerId::new_v4(),
                    display_name: default_display_name(),
                };
                identity.save()?;
                Ok(identity)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Change the name peers show for this machine and keep it.
    ///
    /// Peers learn a name from the greeting, so links already up keep the old
    /// one until they are brought up again.
    pub fn rename(&mut self, name: &str) -> anyhow::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("a machine's name cannot be empty");
        }
        if name.chars().count() > MAX_NAME_CHARS {
            anyhow::bail!("a machine's name is at most {MAX_NAME_CHARS} characters");
        }
        self.display_name = name.to_owned();
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    fn path() -> anyhow::Result<PathBuf> {
        // An explicitly configured root wins: on Android it is the only answer,
        // since the writable directory is chosen by the system per install and
        // handed to the app at runtime. See `cursio_protocol::paths`.
        if let Some(dir) = cursio_protocol::paths::config_dir() {
            return Ok(dir.join("identity.json"));
        }
        let dirs = directories::ProjectDirs::from("dev", "cursio", "cursio")
            .ok_or_else(|| anyhow::anyhow!("cannot resolve config directory"))?;
        Ok(dirs.config_dir().join("identity.json"))
    }
}

/// Longest name a machine may take. mDNS labels stop at 63 bytes, and a name
/// longer than this is one no peer list or notification shows whole anyway.
const MAX_NAME_CHARS: usize = 40;

/// Whether this name is one the node chose for itself rather than the user —
/// what a phone replaces with its own name, since Android's hostname is a
/// meaningless `localhost`.
pub fn is_default_name(name: &str) -> bool {
    name == default_display_name() || name == "localhost" || name == "cursio-node"
}

/// Best-effort hostname, falling back to a generic label.
fn default_display_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "cursio-node".to_string())
}
