//! Password-authenticated pairing and trusted-peer persistence.
//!
//! Pairing uses **SPAKE2** (symmetric mode): both sides feed in the same
//! shared password and exchange one message each. The password itself never
//! goes on the wire — only SPAKE2 messages do — and each side independently
//! derives the same secret key *iff* the passwords matched.
//!
//! Because a wrong password yields two *different* keys (rather than an
//! error), we follow the exchange with an explicit **key-confirmation** step:
//! each side sends an HMAC-like tag over the derived key, and a mismatch is
//! rejected as [`PairingError::ConfirmationFailed`]. Only after confirmation do
//! we persist the peer as trusted.
//!
//! The derived key is stored so future sessions can bootstrap an encrypted
//! channel (Noise) without re-running pairing. Wiring that channel is the next
//! milestone; this module establishes and persists the trust.

use std::collections::HashMap;
use std::path::PathBuf;

use cursio_protocol::{Message, PeerId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::{Transport, TransportError};

/// A fixed identity string mixed into the SPAKE2 exchange. Both sides must use
/// the same value; it binds the exchange to "this is a Cursio pairing".
const PAIRING_IDENTITY: &[u8] = b"cursio-pairing-v1";

/// Domain-separation tags so the confirmation MAC from each direction differs
/// even though both sides hold the same key. Initiator confirms with one tag,
/// responder with the other, preventing a reflected confirmation from passing.
const CONFIRM_TAG_INITIATOR: &[u8] = b"cursio-confirm-initiator";
const CONFIRM_TAG_RESPONDER: &[u8] = b"cursio-confirm-responder";

/// Errors that can occur during pairing.
#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("peer sent an unexpected message during pairing")]
    UnexpectedMessage,
    #[error("SPAKE2 key agreement failed: {0}")]
    KeyAgreement(String),
    #[error("key confirmation failed (wrong password?)")]
    ConfirmationFailed,
    #[error("peer rejected pairing: {0}")]
    Rejected(String),
    #[error("failed to persist trusted peer: {0}")]
    Store(String),
}

/// Outcome of a successful pairing: who we paired with and the shared secret.
#[derive(Debug, Clone)]
pub struct PairedPeer {
    pub peer_id: PeerId,
    pub display_name: String,
    /// The confirmed shared key derived via SPAKE2. Persisted for future
    /// sessions; treat as secret.
    pub shared_key: Vec<u8>,
}

/// Which side of the exchange we are. Only affects confirmation-tag ordering,
/// not the (symmetric) SPAKE2 exchange itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

impl Role {
    fn own_tag(self) -> &'static [u8] {
        match self {
            Role::Initiator => CONFIRM_TAG_INITIATOR,
            Role::Responder => CONFIRM_TAG_RESPONDER,
        }
    }
    fn peer_tag(self) -> &'static [u8] {
        match self {
            Role::Initiator => CONFIRM_TAG_RESPONDER,
            Role::Responder => CONFIRM_TAG_INITIATOR,
        }
    }
}

/// Run the pairing handshake over an established transport.
///
/// Both peers call this with the same `password`. The `role` only orders the
/// key-confirmation tags; the SPAKE2 exchange is symmetric. On success the
/// caller should persist the returned [`PairedPeer`] via [`TrustStore`].
///
/// `local_id`/`local_name` identify us to the peer; `expected_peer` is the
/// identity we believe we're pairing with (from discovery), recorded in the
/// result.
pub async fn run_pairing<T: Transport + ?Sized>(
    transport: &mut T,
    password: &str,
    role: Role,
    expected_peer: (PeerId, String),
) -> Result<PairedPeer, PairingError> {
    // 1. SPAKE2 symmetric: produce our message, derive after receiving theirs.
    let (state, our_spake_msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(password.as_bytes()),
        &Identity::new(PAIRING_IDENTITY),
    );

    // Initiator speaks first; responder waits then replies. Ordering keeps the
    // exchange deterministic over a single stream.
    let their_spake_msg = match role {
        Role::Initiator => {
            transport
                .send(&Message::PairRequest {
                    spake_msg: our_spake_msg,
                })
                .await?;
            match transport.recv().await? {
                Message::PairResponse { spake_msg } => spake_msg,
                Message::PairFailed { reason } => return Err(PairingError::Rejected(reason)),
                _ => return Err(PairingError::UnexpectedMessage),
            }
        }
        Role::Responder => {
            let incoming = match transport.recv().await? {
                Message::PairRequest { spake_msg } => spake_msg,
                Message::PairFailed { reason } => return Err(PairingError::Rejected(reason)),
                _ => return Err(PairingError::UnexpectedMessage),
            };
            transport
                .send(&Message::PairResponse {
                    spake_msg: our_spake_msg,
                })
                .await?;
            incoming
        }
    };

    let shared_key = state
        .finish(&their_spake_msg)
        .map_err(|e| PairingError::KeyAgreement(format!("{e:?}")))?;

    // 2. Key confirmation: prove both sides derived the same key. If passwords
    //    differed, the keys differ and these tags won't match.
    let our_confirmation = confirmation_tag(&shared_key, role.own_tag());
    transport
        .send(&Message::PairConfirm {
            confirmation: our_confirmation,
        })
        .await?;

    let their_confirmation = match transport.recv().await? {
        Message::PairConfirm { confirmation } => confirmation,
        Message::PairFailed { reason } => return Err(PairingError::Rejected(reason)),
        _ => return Err(PairingError::UnexpectedMessage),
    };

    let expected = confirmation_tag(&shared_key, role.peer_tag());
    if !constant_time_eq(&their_confirmation, &expected) {
        // Tell the peer so their side also fails cleanly rather than hanging.
        let _ = transport
            .send(&Message::PairFailed {
                reason: "key confirmation mismatch".into(),
            })
            .await;
        return Err(PairingError::ConfirmationFailed);
    }

    Ok(PairedPeer {
        peer_id: expected_peer.0,
        display_name: expected_peer.1,
        shared_key,
    })
}

/// Compute a confirmation tag = SHA-256(tag_domain || key). Cheap MAC that both
/// sides can reproduce only if they hold the same `key`.
fn confirmation_tag(key: &[u8], domain: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(key);
    hasher.finalize().to_vec()
}

/// Compare two byte slices without early-exit, so timing doesn't leak how many
/// leading bytes matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Trusted-peer persistence
// ---------------------------------------------------------------------------

/// One persisted trust relationship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub peer_id: PeerId,
    pub display_name: String,
    /// Confirmed shared key from pairing, hex-encoded for JSON friendliness.
    pub shared_key_hex: String,
    /// Last address we successfully reached this peer at, if any. Persisted so a
    /// bounced node can be redialled even when mDNS is slow or blocked (VPN
    /// interfaces, multicast filtering): discovery normally supplies the address,
    /// but when it stays silent the registry falls back to this cached value.
    /// `Option` + `#[serde(default)]` keeps old trust files (which lack it)
    /// loadable.
    #[serde(default)]
    pub last_addr: Option<std::net::SocketAddr>,
}

impl TrustedPeer {
    /// Decode the stored shared key back into raw bytes, for bootstrapping an
    /// encrypted channel with an already-trusted peer (no re-pairing needed).
    pub fn shared_key(&self) -> Result<Vec<u8>, PairingError> {
        hex_decode(&self.shared_key_hex)
            .ok_or_else(|| PairingError::Store("corrupt shared_key_hex in trust store".into()))
    }
}

/// On-disk store of peers we've paired with, keyed by peer id.
///
/// Persisted as JSON under the platform config directory (via `directories`),
/// so a paired peer is remembered across restarts and need not re-pair.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TrustStore {
    peers: HashMap<PeerId, TrustedPeer>,
}

impl TrustStore {
    /// Load the store from disk, or return an empty one if none exists yet.
    pub fn load() -> Result<Self, PairingError> {
        let path = Self::path()?;
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| PairingError::Store(e.to_string()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PairingError::Store(e.to_string())),
        }
    }

    /// Record a freshly paired peer and persist immediately.
    pub fn add(&mut self, paired: &PairedPeer) -> Result<(), PairingError> {
        self.peers.insert(
            paired.peer_id,
            TrustedPeer {
                peer_id: paired.peer_id,
                display_name: paired.display_name.clone(),
                shared_key_hex: hex_encode(&paired.shared_key),
                last_addr: None,
            },
        );
        self.save()
    }

    /// Remember the address a peer was last reached at, and persist it. Called
    /// after a session connects so the next start can redial this peer even if
    /// mDNS never announces it. A no-op (no write) when the address is unchanged,
    /// so a steady session doesn't rewrite the file. Unknown peers are ignored.
    pub fn remember_addr(
        &mut self,
        peer_id: &PeerId,
        addr: std::net::SocketAddr,
    ) -> Result<(), PairingError> {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            if peer.last_addr != Some(addr) {
                peer.last_addr = Some(addr);
                return self.save();
            }
        }
        Ok(())
    }

    /// Look up a previously paired peer.
    pub fn get(&self, peer_id: &PeerId) -> Option<&TrustedPeer> {
        self.peers.get(peer_id)
    }

    /// All trusted peers.
    pub fn peers(&self) -> impl Iterator<Item = &TrustedPeer> {
        self.peers.values()
    }

    fn save(&self) -> Result<(), PairingError> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PairingError::Store(e.to_string()))?;
        }
        let json =
            serde_json::to_vec_pretty(self).map_err(|e| PairingError::Store(e.to_string()))?;
        std::fs::write(&path, json).map_err(|e| PairingError::Store(e.to_string()))
    }

    /// Path to the trust-store file in the platform config directory.
    fn path() -> Result<PathBuf, PairingError> {
        let dirs = directories::ProjectDirs::from("dev", "cursio", "cursio")
            .ok_or_else(|| PairingError::Store("cannot resolve config directory".into()))?;
        Ok(dirs.config_dir().join("trusted_peers.json"))
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode a hex string back to bytes. Returns `None` on odd length or a
/// non-hex digit.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_tags_differ_by_domain() {
        let key = b"same-key";
        let a = confirmation_tag(key, CONFIRM_TAG_INITIATOR);
        let b = confirmation_tag(key, CONFIRM_TAG_RESPONDER);
        assert_ne!(a, b, "tags must be domain-separated");
    }

    #[test]
    fn hex_roundtrips() {
        let bytes = vec![0x00, 0x9a, 0xff, 0x10, 0x42];
        let encoded = hex_encode(&bytes);
        assert_eq!(encoded, "009aff1042");
        assert_eq!(hex_decode(&encoded), Some(bytes));
    }

    #[test]
    fn hex_decode_rejects_malformed() {
        assert_eq!(hex_decode("abc"), None, "odd length");
        assert_eq!(hex_decode("zz"), None, "non-hex digit");
    }

    #[test]
    fn trusted_peer_recovers_shared_key() {
        let peer = TrustedPeer {
            peer_id: PeerId::nil(),
            display_name: "x".into(),
            shared_key_hex: "0a0b0c".into(),
            last_addr: None,
        };
        assert_eq!(peer.shared_key().unwrap(), vec![0x0a, 0x0b, 0x0c]);
    }

    #[test]
    fn confirmation_tag_is_deterministic() {
        let key = b"k";
        assert_eq!(
            confirmation_tag(key, CONFIRM_TAG_INITIATOR),
            confirmation_tag(key, CONFIRM_TAG_INITIATOR)
        );
    }

    #[test]
    fn constant_time_eq_matches_semantics_of_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn different_passwords_yield_different_keys() {
        // Simulate both sides of the symmetric exchange for each password and
        // confirm mismatched passwords produce mismatched keys — the property
        // key-confirmation relies on.
        let run = |pw_a: &str, pw_b: &str| {
            let (sa, msg_a) = Spake2::<Ed25519Group>::start_symmetric(
                &Password::new(pw_a.as_bytes()),
                &Identity::new(PAIRING_IDENTITY),
            );
            let (sb, msg_b) = Spake2::<Ed25519Group>::start_symmetric(
                &Password::new(pw_b.as_bytes()),
                &Identity::new(PAIRING_IDENTITY),
            );
            let ka = sa.finish(&msg_b).unwrap();
            let kb = sb.finish(&msg_a).unwrap();
            (ka, kb)
        };

        let (ka, kb) = run("hunter2", "hunter2");
        assert_eq!(ka, kb, "matching passwords must agree on a key");

        let (ka, kb) = run("hunter2", "wrong");
        assert_ne!(ka, kb, "mismatched passwords must not agree");
    }
}
