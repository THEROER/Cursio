//! Who may replace the program: signatures on builds, and whom they are
//! checked against.
//!
//! Being a paired peer is not enough. Pairing grants a machine the right to
//! move the cursor and read the disks; replacing the executable every other
//! right runs through is a larger grant, and it has to be given on purpose,
//! machine by machine. So a build is accepted for what signed it, never for
//! who handed it over — a laptop relaying a build it was given is carrying
//! bytes, and the bytes vouch for themselves.
//!
//! Two kinds of key:
//!
//! - **Release keys**, whose public halves are compiled in below. They sign
//!   the `stable` channel, which is what a machine follows by default. There
//!   are none yet: public releases are phase 3, and a stable channel with no
//!   key simply accepts nothing rather than trusting a placeholder.
//! - **Builder keys**: every machine that publishes (`cargo xtask publish`)
//!   has its own, kept beside its identity. A receiving machine accepts
//!   `dev`-channel builds from it only after the user ran
//!   `cursio update trust-builder <machine>` there, which pins the key the
//!   machine had at that moment — a builder that later changes its key is
//!   refused until trusted again.

use std::path::PathBuf;

use cursio_protocol::update::{Channel, Offer, Statement};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::config::{UpdateChannel, UpdateConfig};

use super::UpdateError;

/// Public halves of the keys that sign the `stable` channel.
pub const RELEASE_KEYS: &[[u8; 32]] = &[];

/// This machine's key for publishing builds.
pub struct BuilderKey(SigningKey);

impl BuilderKey {
    /// The key beside this machine's identity, made on first use.
    pub fn load_or_create() -> Result<Self, UpdateError> {
        let path = key_path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let seed = parse_hex::<32>(text.trim()).ok_or_else(|| {
                    UpdateError::Refused(format!(
                        "{} is not a 32-byte hex key; delete it to make a new one \
                         (peers that trusted the old one will have to trust again)",
                        path.display()
                    ))
                })?;
                Ok(Self(SigningKey::from_bytes(&seed)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = SigningKey::generate(&mut rand::rngs::OsRng);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| UpdateError::io(parent, e))?;
                }
                write_private(&path, &hex(&key.to_bytes()))?;
                tracing::info!(path = %path.display(), "made this machine's build-signing key");
                Ok(Self(key))
            }
            Err(e) => Err(UpdateError::io(&path, e)),
        }
    }

    /// The key already made on this machine, if it has published before.
    pub fn existing_public() -> Option<[u8; 32]> {
        let text = std::fs::read_to_string(key_path().ok()?).ok()?;
        let seed = parse_hex::<32>(text.trim())?;
        Some(SigningKey::from_bytes(&seed).verifying_key().to_bytes())
    }

    #[doc(hidden)]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    pub fn public(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }

    pub fn sign(&self, statement: &Statement, have: Vec<String>) -> Offer {
        let bytes = statement.to_bytes();
        Offer {
            signature: self.0.sign(&bytes).to_bytes(),
            signer: self.public(),
            statement: bytes,
            have,
        }
    }
}

/// Why an offered build was not taken. Shown to the user as it is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the build's description could not be read: {0}")]
    Malformed(String),
    #[error("the signature does not match the build it came with")]
    BadSignature,
    #[error(
        "signed by a key this machine does not trust ({key}); if that is one of your \
         machines, run `cursio update trust-builder <machine>` here"
    )]
    Untrusted { key: String },
    #[error("a {got} build, and this machine follows {follows}")]
    WrongChannel { got: String, follows: String },
}

/// Check an offer's signature and whether its signer may publish to its
/// channel here, answering the statement it vouches for.
pub fn verify(offer: &Offer, config: &UpdateConfig) -> Result<Statement, Refusal> {
    let statement =
        Statement::from_bytes(&offer.statement).map_err(|e| Refusal::Malformed(e.to_string()))?;
    let key = VerifyingKey::from_bytes(&offer.signer).map_err(|_| Refusal::BadSignature)?;
    key.verify(&offer.statement, &Signature::from_bytes(&offer.signature))
        .map_err(|_| Refusal::BadSignature)?;

    if statement.channel == Channel::Dev && config.channel == UpdateChannel::Stable {
        return Err(Refusal::WrongChannel {
            got: "dev".into(),
            follows: "stable".into(),
        });
    }
    let release = RELEASE_KEYS.contains(&offer.signer);
    let builder = statement.channel == Channel::Dev
        && config
            .trusted_builders
            .iter()
            .any(|b| parse_hex::<32>(&b.key) == Some(offer.signer));
    if release || builder {
        Ok(statement)
    } else {
        Err(Refusal::Untrusted {
            key: short_key(&offer.signer),
        })
    }
}

/// The first bytes of a key, enough to tell two apart in a message.
pub fn short_key(key: &[u8; 32]) -> String {
    hex(&key[..6])
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn key_path() -> Result<PathBuf, UpdateError> {
    let config = crate::config::default_path().map_err(|e| UpdateError::Refused(e.to_string()))?;
    let dir = config.parent().ok_or(UpdateError::NoRoot)?;
    Ok(dir.join("update-signing.key"))
}

/// Write a secret readable by this user alone where the platform lets us say so.
fn write_private(path: &std::path::Path, text: &str) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| UpdateError::io(path, e))?;
        file.write_all(text.as_bytes())
            .map_err(|e| UpdateError::io(path, e))
    }
    // The per-user profile directory already denies other users on Windows.
    #[cfg(not(unix))]
    {
        std::fs::write(path, text).map_err(|e| UpdateError::io(path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TrustedBuilder;
    use cursio_protocol::update::Artifact;

    fn statement(channel: Channel) -> Statement {
        Statement {
            version: "0.1.0+abc".into(),
            channel,
            published_at: 10,
            artifacts: vec![Artifact {
                platform: "linux-x86_64".into(),
                name: "cursio".into(),
                size: 3,
                sha256: [9; 32],
            }],
        }
    }

    fn trusting(key: &BuilderKey) -> UpdateConfig {
        UpdateConfig {
            channel: UpdateChannel::Dev,
            trusted_builders: vec![TrustedBuilder {
                peer: cursio_protocol::PeerId::nil(),
                name: "nocturne".into(),
                key: hex(&key.public()),
            }],
            ..UpdateConfig::default()
        }
    }

    #[test]
    fn a_trusted_builders_dev_build_is_accepted() {
        let key = BuilderKey::from_seed([1; 32]);
        let offer = key.sign(&statement(Channel::Dev), vec![]);
        assert_eq!(
            verify(&offer, &trusting(&key)).unwrap(),
            statement(Channel::Dev)
        );
    }

    #[test]
    fn an_untrusted_signer_is_refused_by_its_key() {
        let key = BuilderKey::from_seed([1; 32]);
        let stranger = BuilderKey::from_seed([2; 32]);
        let offer = stranger.sign(&statement(Channel::Dev), vec![]);
        assert!(matches!(
            verify(&offer, &trusting(&key)),
            Err(Refusal::Untrusted { .. })
        ));
    }

    #[test]
    fn a_statement_changed_after_signing_is_refused() {
        let key = BuilderKey::from_seed([1; 32]);
        let mut offer = key.sign(&statement(Channel::Dev), vec![]);
        let last = offer.statement.len() - 1;
        offer.statement[last] ^= 1;
        assert_eq!(verify(&offer, &trusting(&key)), Err(Refusal::BadSignature));
    }

    #[test]
    fn a_builder_cannot_publish_to_stable() {
        let key = BuilderKey::from_seed([1; 32]);
        let offer = key.sign(&statement(Channel::Stable), vec![]);
        assert!(matches!(
            verify(&offer, &trusting(&key)),
            Err(Refusal::Untrusted { .. })
        ));
    }

    #[test]
    fn a_stable_machine_refuses_dev_builds_even_from_a_trusted_builder() {
        let key = BuilderKey::from_seed([1; 32]);
        let config = UpdateConfig {
            channel: UpdateChannel::Stable,
            ..trusting(&key)
        };
        let offer = key.sign(&statement(Channel::Dev), vec![]);
        assert!(matches!(
            verify(&offer, &config),
            Err(Refusal::WrongChannel { .. })
        ));
    }

    #[test]
    fn hex_round_trips_and_rejects_the_wrong_length() {
        let bytes = [0xab; 32];
        assert_eq!(parse_hex::<32>(&hex(&bytes)), Some(bytes));
        assert_eq!(parse_hex::<32>("abcd"), None);
    }
}
