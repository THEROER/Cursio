//! Who the other end is: a value a verifier produced from what it checked,
//! never something a caller asserts.
//!
//! A [`Trust`] runs the opening exchange over a fresh carrier and answers it
//! with an [`Opened`]: the seal, the session's tag, and the [`Verified`] peer.
//! The identity is typed ([`PeerIdentity`]) so a service cannot be taken for a
//! user, and it compares on its canonical form only: a certificate rotated to
//! a new serial names the same peer, so a session it proved lives on.
//!
//! Trust is continuous rather than once. Every proof carries the instant it
//! stops proving anything (`not_after`: a certificate's, a token's `exp`), and
//! the trust is asked again — [`Trust::check`] — whenever a connection joins
//! a session, on a timer, and before each message is delivered, so a serial
//! put on a deny list ends a session that is only being held for a path that
//! may never come back. A session nearing its deadline is extended only by a
//! fresh proof of the same identity ([`Trust::reverify`], carried by a
//! `REAUTH` frame); a proof of another identity ends it.

use std::time::SystemTime;

use async_trait::async_trait;

use crate::carrier::TlsInfo;
use crate::{Carrier, ChannelError, Seal, SessionTag};

/// Which side of the opening exchange this end is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The side that dialled.
    Initiator,
    /// The side that accepted.
    Responder,
}

/// Who a verifier found at the other end.
#[derive(Debug, Clone)]
pub enum PeerIdentity {
    /// A workload, from the one URI SAN of a client certificate with
    /// clientAuth (`spiffe://<domain>/service/<id>`), never a CN or a header.
    Service { id: String },
    /// A machine.
    Host { id: String },
    /// A person, from a token whose audience was checked.
    User { sub: String, aud: String },
    /// A peer known by a secret both ends hold, after pairing.
    Paired { id: [u8; 16] },
}

impl PeerIdentity {
    /// The form two proofs of one peer share. Identifiers compare exactly:
    /// folding case or trimming would let `Admin` prove `admin`, and an
    /// issuer that means them alike must say so itself.
    pub fn canonical(&self) -> String {
        match self {
            Self::Service { id } => format!("service:{id}"),
            Self::Host { id } => format!("host:{id}"),
            Self::User { sub, aud } => format!("user:{}:{sub}:{aud}", sub.len()),
            Self::Paired { id } => {
                let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
                format!("paired:{hex}")
            }
        }
    }
}

impl PartialEq for PeerIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.canonical() == other.canonical()
    }
}

impl Eq for PeerIdentity {}

impl std::hash::Hash for PeerIdentity {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.canonical().hash(state);
    }
}

impl std::fmt::Display for PeerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.canonical())
    }
}

/// One proof of a peer's identity, as a verifier accepted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub identity: PeerIdentity,
    /// What names this proof to whoever may revoke it: a certificate's serial,
    /// a token's `jti`. `None` for a proof nothing can revoke but the trust
    /// itself (a shared secret).
    pub serial: Option<String>,
    /// When the proof stops proving anything; `None` for one that does not
    /// expire.
    pub not_after: Option<SystemTime>,
}

impl Verified {
    /// A proof that never expires and names no serial.
    pub fn lasting(identity: PeerIdentity) -> Self {
        Self {
            identity,
            serial: None,
            not_after: None,
        }
    }

    /// Whether this proof still stands at `now` as far as its own deadline
    /// says; [`Trust::check`] answers the rest.
    pub fn expired_at(&self, now: SystemTime) -> bool {
        self.not_after.is_some_and(|at| now >= at)
    }
}

/// Why a trust no longer stands behind a proof.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Revoked(pub String);

/// What an opening exchange established.
pub struct Opened {
    /// Who the other end proved to be.
    pub peer: Verified,
    pub seal: Seal,
    /// The same on both ends and unique to this connection: what names a
    /// session for resumption. Never logged.
    pub session_tag: SessionTag,
    /// The session this connection asks to join, as it named it; see
    /// [`crate::ResumeHandle::named_by`]. Only a trust whose opening exchange
    /// carries the request sets it.
    pub resume: Option<SessionTag>,
}

/// Who the other end is and the keys to talk to it with.
#[async_trait]
pub trait Trust: Send + Sync {
    /// The opening exchange over a fresh carrier, as the side that dialled
    /// or the side that accepted.
    async fn open(&self, carrier: &mut dyn Carrier, role: Role) -> Result<Opened, ChannelError>;

    /// Whether `peer` is still trusted at `now`: past its deadline, on a deny
    /// list, refused by an issuer's introspection. `Err` ends every session
    /// it proved, held or not.
    fn check(&self, peer: &Verified, now: SystemTime) -> Result<(), Revoked>;

    /// A fresh proof the peer sent in a `REAUTH` frame over a connection
    /// whose TLS, if any, is `tls`. A trust without fresh proofs refuses it,
    /// which ends the session: a proof nobody can verify proves nothing.
    async fn reverify(
        &self,
        _proof: &[u8],
        _tls: Option<&TlsInfo>,
    ) -> Result<Verified, ChannelError> {
        Err(ChannelError::Other(
            "this trust takes no fresh proof of identity".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(sub: &str, aud: &str) -> PeerIdentity {
        PeerIdentity::User {
            sub: sub.into(),
            aud: aud.into(),
        }
    }

    #[test]
    fn identities_of_different_kinds_never_compare_equal() {
        assert_ne!(
            PeerIdentity::Service { id: "a".into() },
            PeerIdentity::Host { id: "a".into() }
        );
        assert_eq!(user("a", "panel"), user("a", "panel"));
        assert_ne!(user("a", "panel"), user("a", "agent"));
    }

    #[test]
    fn a_subject_cannot_borrow_its_audience_to_look_like_another() {
        assert_ne!(user("a:b", "c"), user("a", "b:c"));
    }

    #[test]
    fn a_rotated_proof_of_one_peer_names_the_same_identity() {
        let old = Verified {
            identity: PeerIdentity::Service { id: "agent".into() },
            serial: Some("01".into()),
            not_after: Some(SystemTime::UNIX_EPOCH),
        };
        let new = Verified {
            serial: Some("02".into()),
            not_after: None,
            ..old.clone()
        };
        assert_ne!(old, new);
        assert_eq!(old.identity, new.identity);
        assert!(old.expired_at(SystemTime::now()));
        assert!(!new.expired_at(SystemTime::now()));
    }
}
