//! A session bound to the identity that opened it.
//!
//! Without a binding a session is Cursio's: any connection that completes the
//! pair's handshake and names the tag joins, because the secret already says
//! who the peer is. Over the internet that is not enough — the tag is a value
//! a connection *offers*, and whoever saw it could offer it — so a bound
//! session asks more of every connection, and of itself, for as long as it
//! lives:
//!
//! - a connection joins only as the same canonical identity, with a proof the
//!   trust still stands behind, and offering the tag bound to its own TLS
//!   (`tls-exporter`) where it has one;
//! - the session's proofs are checked again on a timer and before each
//!   message is delivered, so revocation reaches a session that is only held;
//! - the session ends at the earliest `not_after` among its proofs, unless a
//!   `REAUTH` brings a fresh proof of the same identity, which then stands for
//!   the whole session; one of another identity ends it.
//!
//! A joining connection that fails is refused, not fatal: it may be someone
//! else holding the tag, and someone else must not be able to end a session
//! by knocking on it. Everything the session itself was proven by failing
//! ends it, held or not.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::trust::{PeerIdentity, Trust, Verified};
use crate::SessionTag;

/// How often a bound session asks its trust again when nothing else does.
pub const CHECK_EVERY: Duration = Duration::from_secs(5);

/// The most proofs a session keeps, the one it was opened by among them.
const MAX_PROOFS: usize = 8;

/// How a [`crate::ResumableLink`] is run beyond its classes.
pub struct SessionOptions {
    pub(crate) trust: Option<Arc<dyn Trust>>,
    pub(crate) check_every: Duration,
    pub(crate) hand_back: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            trust: None,
            check_every: CHECK_EVERY,
            hand_back: false,
        }
    }
}

impl SessionOptions {
    /// Bind the session to the identity its first path proved, re-checked
    /// with `trust` for as long as it lives.
    pub fn bound_to(trust: Arc<dyn Trust>) -> Self {
        Self {
            trust: Some(trust),
            ..Self::default()
        }
    }

    /// Ask the trust again this often, besides on every join and before
    /// every delivered message.
    pub fn check_every(mut self, every: Duration) -> Self {
        self.check_every = every;
        self
    }

    /// When the session ends, hand the application every reliable message
    /// the peer never acknowledged ([`crate::SessionEvent::Ended`]). Costs a
    /// copy of each reliable message until its acknowledgement.
    pub fn hand_back(mut self) -> Self {
        self.hand_back = true;
        self
    }
}

pub(super) struct Binding {
    trust: Arc<dyn Trust>,
    identity: PeerIdentity,
    /// Every proof the session stands on now: the first path's, each joined
    /// path's, or the last `REAUTH`'s alone.
    proofs: Mutex<Vec<Verified>>,
    pub(super) check_every: Duration,
}

impl Binding {
    pub(super) fn new(trust: Arc<dyn Trust>, first: Verified, check_every: Duration) -> Self {
        Self {
            trust,
            identity: first.identity.clone(),
            proofs: Mutex::new(vec![first]),
            check_every,
        }
    }

    pub(super) fn trust(&self) -> &Arc<dyn Trust> {
        &self.trust
    }

    fn proofs(&self) -> std::sync::MutexGuard<'_, Vec<Verified>> {
        self.proofs.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn stands(&self, proof: &Verified, now: SystemTime) -> Result<(), String> {
        if proof.expired_at(now) {
            return Err(format!("the proof of {} expired", self.identity));
        }
        self.trust
            .check(proof, now)
            .map_err(|revoked| format!("the trust withdrew {}: {revoked}", self.identity))
    }

    /// Whether everything the session stands on still stands at `now`.
    pub(super) fn verify(&self, now: SystemTime) -> Result<(), String> {
        let proofs = self.proofs().clone();
        proofs.iter().try_for_each(|proof| self.stands(proof, now))
    }

    /// Whether the earliest deadline among the proofs has passed, without
    /// asking the trust: cheap enough for every tick.
    pub(super) fn past_deadline(&self, now: SystemTime) -> bool {
        self.proofs().iter().any(|proof| proof.expired_at(now))
    }

    /// When the session ends unless a fresh proof comes first.
    pub(super) fn deadline(&self) -> Option<SystemTime> {
        self.proofs()
            .iter()
            .filter_map(|proof| proof.not_after)
            .min()
    }

    /// Let a joining connection's proof stand beside the session's, or say
    /// why it may not join.
    pub(super) fn admit(&self, proof: Option<&Verified>, now: SystemTime) -> Result<(), String> {
        let proof = proof.ok_or("the connection proved no identity")?;
        if proof.identity != self.identity {
            return Err(format!(
                "the connection proved {}, not the session's {}",
                proof.identity, self.identity
            ));
        }
        self.stands(proof, now)?;
        let mut proofs = self.proofs();
        // A session that reconnects all day must not grow a proof per
        // reconnect: the same proof again replaces itself, and past a few the
        // oldest joined one goes, never the one the session was opened by.
        proofs.retain(|kept| kept != proof);
        if proofs.len() >= MAX_PROOFS {
            proofs.remove(1);
        }
        proofs.push(proof.clone());
        Ok(())
    }

    /// Take a fresh proof from `REAUTH` as the session's whole footing, or
    /// say why it ends the session.
    pub(super) fn renew(&self, proof: Verified, now: SystemTime) -> Result<(), String> {
        if proof.identity != self.identity {
            return Err(format!(
                "a fresh proof named {}, not the session's {}",
                proof.identity, self.identity
            ));
        }
        self.stands(&proof, now)?;
        *self.proofs() = vec![proof];
        Ok(())
    }
}

/// What a connection offers to name a session: the tag itself over a
/// connection with no channel binding (Cursio's wire), and over TLS a value
/// bound to that connection's `tls-exporter`, so an offer seen on one
/// connection names nothing on another.
pub fn offered_tag(tag: &SessionTag, exporter: Option<&[u8; 32]>) -> SessionTag {
    let Some(exporter) = exporter else {
        return *tag;
    };
    let mut hasher = Sha256::new();
    hasher.update(b"cursio-channel resume");
    hasher.update(tag);
    hasher.update(exporter);
    let mut offered = [0u8; 16];
    offered.copy_from_slice(&hasher.finalize()[..16]);
    offered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::Revoked;

    struct DenySerial(&'static str);

    #[async_trait::async_trait]
    impl Trust for DenySerial {
        async fn open(
            &self,
            _: &mut dyn crate::Carrier,
            _: crate::Role,
        ) -> Result<crate::Opened, crate::ChannelError> {
            Err(crate::ChannelError::Closed)
        }

        fn check(&self, peer: &Verified, _: SystemTime) -> Result<(), Revoked> {
            match peer.serial.as_deref() {
                Some(serial) if serial == self.0 => Err(Revoked("denied".into())),
                _ => Ok(()),
            }
        }
    }

    fn proof(id: &str, serial: &str, for_secs: u64) -> Verified {
        Verified {
            identity: PeerIdentity::Host { id: id.into() },
            serial: Some(serial.into()),
            not_after: Some(SystemTime::now() + Duration::from_secs(for_secs)),
        }
    }

    #[test]
    fn the_deadline_is_the_earliest_proof_and_a_renewal_replaces_them_all() {
        let binding = Binding::new(Arc::new(DenySerial("x")), proof("a", "1", 60), CHECK_EVERY);
        let now = SystemTime::now();
        binding.admit(Some(&proof("a", "2", 10)), now).unwrap();
        assert!(binding.deadline().unwrap() < now + Duration::from_secs(11));
        binding.renew(proof("a", "3", 600), now).unwrap();
        assert!(binding.deadline().unwrap() > now + Duration::from_secs(500));
    }

    #[test]
    fn another_identity_or_a_denied_serial_is_not_admitted() {
        let binding = Binding::new(Arc::new(DenySerial("x")), proof("a", "1", 60), CHECK_EVERY);
        let now = SystemTime::now();
        assert!(binding.admit(Some(&proof("b", "2", 60)), now).is_err());
        assert!(binding.admit(Some(&proof("a", "x", 60)), now).is_err());
        assert!(binding.admit(None, now).is_err());
        assert!(binding.renew(proof("b", "4", 60), now).is_err());
        assert!(binding.verify(now).is_ok());
    }

    #[test]
    fn a_tag_offered_over_tls_differs_per_connection_and_is_bare_without() {
        let tag = [9; 16];
        assert_eq!(offered_tag(&tag, None), tag);
        let one = offered_tag(&tag, Some(&[1; 32]));
        assert_ne!(one, tag);
        assert_ne!(one, offered_tag(&tag, Some(&[2; 32])));
        assert_eq!(one, offered_tag(&tag, Some(&[1; 32])));
    }
}
