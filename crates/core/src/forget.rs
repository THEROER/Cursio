//! Removing a machine from the mesh.
//!
//! Pairing is the only way in, so this is the only way out, and it has to undo
//! everything pairing made durable: the shared key in the trust store (without
//! it the machine can no longer complete a handshake here, which is what
//! actually keeps it out), the address cached beside that key, and builder
//! trust for updates, which names the machine by id. Those live on disk, so
//! this part runs the same whether a node is running or not — the CLI uses it
//! directly when no node answers.
//!
//! What a *running* node also holds — the registry entry, a live link, the
//! machine's problems in the diagnostics list — is dropped by the node itself:
//! the control command calls [`forget_on_disk`] and then tells the mesh through
//! [`LiveConfig::forget`](crate::live::LiveConfig::forget), whose watch ends
//! the link from inside its own session so modules stop and disks unmount the
//! ordinary way.
//!
//! The other machine still trusts this one and keeps dialling. Its handshake
//! fails here for want of a key, as any stranger's does; [`note_refused_call`]
//! keeps that to one line in the log rather than one per sweep, and a pairing
//! from either side lets it back in.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use cursio_protocol::PeerId;
use cursio_transport::pairing::TrustStore;

use crate::config::Config;

/// What removing a machine undid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forgotten {
    pub peer: PeerId,
    pub name: String,
    /// It had been trusted to sign builds, and no longer is.
    pub was_builder: bool,
}

/// A paired machine by the name or id `cursio peers` and the app show.
pub fn resolve(machine: &str) -> anyhow::Result<(PeerId, String)> {
    let store = TrustStore::load()?;
    let found = store
        .peers()
        .find(|p| p.display_name == machine || p.peer_id.to_string() == machine)
        .map(|p| (p.peer_id, p.display_name.clone()));
    found.ok_or_else(|| {
        anyhow::anyhow!("{machine} is not paired with this machine; `cursio status` lists who is")
    })
}

/// Undo the durable half of a pairing: the trust-store entry with its key and
/// cached address, and builder trust in `config_path`.
pub fn forget_on_disk(peer: PeerId, config_path: &Path) -> anyhow::Result<Forgotten> {
    let mut store = TrustStore::load()?;
    let removed = store
        .remove(&peer)?
        .ok_or_else(|| anyhow::anyhow!("{peer} is not paired with this machine"))?;
    let was_builder = untrust_builder(peer, config_path)?;
    clear_problems(peer, &removed.display_name);
    tracing::info!(peer = %removed.display_name, id = %peer, "removed from this mesh");
    Ok(Forgotten {
        peer,
        name: removed.display_name,
        was_builder,
    })
}

/// Drop `peer` from the builders whose updates are accepted. A missing config
/// file has no builders to drop, and is not written just to say so.
fn untrust_builder(peer: PeerId, config_path: &Path) -> anyhow::Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }
    let mut config = Config::load(config_path)?;
    let before = config.update.trusted_builders.len();
    config.update.trusted_builders.retain(|b| b.peer != peer);
    if config.update.trusted_builders.len() == before {
        return Ok(false);
    }
    config.save(config_path)?;
    Ok(true)
}

/// The machine's problems describe a link that is not coming back; left up,
/// they would ask the user to fix something they just chose to end.
fn clear_problems(peer: PeerId, name: &str) {
    for problem in crate::diagnostics::snapshot() {
        if is_about(&problem.id, peer, name) {
            crate::diagnostics::clear(&problem.id);
        }
    }
}

/// Whether a problem id names this machine: the link's `peer.<id>.` family,
/// or a module's `module.<module>.<name>.` one.
fn is_about(id: &str, peer: PeerId, name: &str) -> bool {
    id.starts_with(&format!("peer.{peer}."))
        || (id.starts_with("module.") && id.contains(&format!(".{name}.")))
}

static REFUSED: Mutex<Option<HashSet<PeerId>>> = Mutex::new(None);

/// A removed machine called again. True the first time for each, so the
/// caller can say so once; it keeps calling every sweep until it is paired
/// again or removes this machine too.
pub fn note_refused_call(peer: PeerId) -> bool {
    REFUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(HashSet::new)
        .insert(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_problem_about_the_machine_is_recognised_by_id_or_name() {
        let peer = PeerId::from_u128(7);
        assert!(is_about(&format!("peer.{peer}.down"), peer, "Lightmoon"));
        assert!(is_about(
            "module.files.Lightmoon.stalled",
            peer,
            "Lightmoon"
        ));
        assert!(!is_about(
            "module.files.Nocturne.stalled",
            peer,
            "Lightmoon"
        ));
        assert!(!is_about("machine.io", peer, "Lightmoon"));
        assert!(!is_about(
            &format!("peer.{}.down", PeerId::from_u128(8)),
            peer,
            "Lightmoon"
        ));
    }

    #[test]
    fn a_refused_call_is_reported_once_per_machine() {
        let a = PeerId::new_v4();
        let b = PeerId::new_v4();
        assert!(note_refused_call(a));
        assert!(!note_refused_call(a));
        assert!(note_refused_call(b));
    }

    #[test]
    fn dropping_builder_trust_leaves_other_builders_and_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("cursio-forget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let (gone, kept) = (PeerId::new_v4(), PeerId::new_v4());
        let mut config = Config {
            port: 47000,
            ..Config::default()
        };
        for (peer, name) in [(gone, "gone"), (kept, "kept")] {
            config
                .update
                .trusted_builders
                .push(crate::config::TrustedBuilder {
                    peer,
                    name: name.into(),
                    key: "00".repeat(32),
                });
        }
        config.save(&path).unwrap();

        assert!(untrust_builder(gone, &path).unwrap());
        assert!(!untrust_builder(gone, &path).unwrap());

        let after = Config::load(&path).unwrap();
        assert_eq!(after.port, 47000);
        assert_eq!(
            after
                .update
                .trusted_builders
                .iter()
                .map(|b| b.peer)
                .collect::<Vec<_>>(),
            vec![kept]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_config_file_is_not_created_to_drop_nothing() {
        let path = std::env::temp_dir()
            .join(format!("cursio-forget-{}", uuid::Uuid::new_v4()))
            .join("config.toml");
        assert!(!untrust_builder(PeerId::new_v4(), &path).unwrap());
        assert!(!path.exists());
    }
}
