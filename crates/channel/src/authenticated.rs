//! A trust for a carrier TLS already verified: who the peer is comes from
//! that TLS and a token sent in the opening exchange, judged by a verifier
//! the caller writes.
//!
//! Over the internet the peer is a workload with a certificate from a CA the
//! caller runs, or a person behind a browser that can speak only WebSocket
//! and proves itself with a token. Noise over TLS would encrypt twice and
//! still need the certificate checked, so here TLS keeps the frames and the
//! seal is the identity ([`Seal::is_identity`]); what this module adds is the
//! part TLS cannot answer — whether *this* certificate or token names a peer
//! the caller trusts, and whether it still does ([`Verifier::check`]).
//!
//! Which is why it refuses, before a byte is exchanged, any carrier that does
//! not report verified TLS ([`Carrier::tls`]): plain TCP and `ws://` carry a
//! token in the clear and frames anyone on the path may rewrite. Nothing is
//! negotiated and nothing falls back — an endpoint configured with this trust
//! speaks it or nothing. The in-memory carrier passes only behind
//! [`CarrierAuthenticated::insecure_in_memory_for_tests`], whose name is the
//! warning.
//!
//! The opening exchange, inside the TLS:
//!
//! - the side that dialled sends `OPEN`: `"CA"`, version 1, a flags byte
//!   (`0x01`: resuming), when resuming the 16-byte offer naming the session
//!   ([`crate::session::offered_tag`], bound to this connection's exporter),
//!   then `[u32 len][token]`;
//! - the side that accepted verifies it and answers `ACCEPT`: `0x00`, a
//!   16-byte session tag it drew at random, `[u32 len][token]` — or `REFUSE`:
//!   `0x01` and nothing else, the reason staying in its own log rather than
//!   going to whoever knocked;
//! - the side that dialled verifies the answer in turn.
//!
//! The tag is issued by the accepting side over the authenticated carrier,
//! so it is never derived from anything the dialler chose.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use rand::RngCore;

use crate::carrier::TlsInfo;
use crate::noise::HANDSHAKE_TIMEOUT;
use crate::session::offered_tag;
use crate::trust::{Opened, Revoked, Role, Trust, Verified};
use crate::{Carrier, ChannelError, Seal, SessionTag};

const MAGIC: [u8; 3] = [b'C', b'A', 1];
const RESUMING: u8 = 0x01;
const ACCEPT: u8 = 0x00;
const REFUSE: u8 = 0x01;
/// A token larger than this is not a token.
const MAX_TOKEN: usize = 64 * 1024;

/// What a verifier is shown of a peer.
#[derive(Debug, Clone, Copy)]
pub struct PeerContext<'a> {
    /// What the TLS under the connection verified; `None` only on the
    /// in-memory carrier of tests.
    pub tls: Option<&'a TlsInfo>,
    /// The token the peer presented, if it presented one.
    pub token: Option<&'a [u8]>,
    /// A fresh proof for a running session (`REAUTH`), not an opening.
    pub renewal: bool,
}

/// The caller's rules for who a verified TLS peer is: Leave's spiffe rules
/// for a certificate's URI SAN, a JWT validator with audience and
/// introspection for a token.
#[async_trait]
pub trait Verifier: Send + Sync + 'static {
    /// Who `peer` is, or why it is nobody this end trusts.
    async fn verify(&self, peer: PeerContext<'_>) -> Result<Verified, Revoked>;

    /// Whether `peer` is still trusted at `now`; see [`Trust::check`].
    fn check(&self, peer: &Verified, now: SystemTime) -> Result<(), Revoked>;
}

/// [`Trust`] for a carrier whose TLS verified its peer; see the module.
#[derive(Clone)]
pub struct CarrierAuthenticated {
    verifier: Arc<dyn Verifier>,
    token: Option<Vec<u8>>,
    resume: Option<SessionTag>,
    in_memory: bool,
}

impl CarrierAuthenticated {
    pub fn new(verifier: Arc<dyn Verifier>) -> Self {
        Self {
            verifier,
            token: None,
            resume: None,
            in_memory: false,
        }
    }

    /// As [`Self::new`], also accepting the in-memory carrier, which has no
    /// TLS at all. For tests only: anything it opens is protected by nothing.
    pub fn insecure_in_memory_for_tests(verifier: Arc<dyn Verifier>) -> Self {
        Self {
            in_memory: true,
            ..Self::new(verifier)
        }
    }

    /// Present `token` in the opening exchange: a person's JWT, a workload's
    /// proof where its certificate alone is not one.
    pub fn presenting(mut self, token: Vec<u8>) -> Self {
        self.token = Some(token);
        self
    }

    /// Ask, from the dialling side, to join the session known as `tag`.
    pub fn resuming(mut self, tag: SessionTag) -> Self {
        self.resume = Some(tag);
        self
    }

    fn admit_carrier(&self, carrier: &dyn Carrier) -> Result<Option<TlsInfo>, ChannelError> {
        match carrier.tls() {
            Some(tls) => Ok(Some(tls.clone())),
            None if self.in_memory && carrier.kind() == "mem" => Ok(None),
            None => Err(ChannelError::Other(format!(
                "a carrier-authenticated trust refuses a {} carrier: it needs TLS that verified its peer (wss:// or TLS over TCP)",
                carrier.kind()
            ))),
        }
    }

    async fn verify(
        &self,
        tls: Option<&TlsInfo>,
        token: Option<&[u8]>,
    ) -> Result<Verified, ChannelError> {
        self.verifier
            .verify(PeerContext {
                tls,
                token,
                renewal: false,
            })
            .await
            .map_err(|revoked| ChannelError::Other(format!("the peer was not verified: {revoked}")))
    }

    async fn initiate(
        &self,
        carrier: &mut dyn Carrier,
        tls: Option<&TlsInfo>,
    ) -> Result<Opened, ChannelError> {
        let mut open = MAGIC.to_vec();
        match &self.resume {
            Some(tag) => {
                open.push(RESUMING);
                open.extend_from_slice(&offered_tag(tag, tls.and_then(|t| t.exporter.as_ref())));
            }
            None => open.push(0),
        }
        put_token(&mut open, self.token.as_deref());
        carrier.send(open).await?;

        let answer = carrier.recv().await?;
        let (&verdict, rest) = answer.split_first().ok_or_else(truncated)?;
        if verdict == REFUSE {
            return Err(ChannelError::Other(
                "the accepting side refused this end's proof".into(),
            ));
        }
        if verdict != ACCEPT || rest.len() < 16 {
            return Err(truncated());
        }
        let session_tag: SessionTag = rest[..16].try_into().expect("sixteen bytes");
        let token = take_token(&rest[16..])?;
        let peer = self.verify(tls, token).await?;
        Ok(Opened {
            peer,
            seal: Seal::identity(),
            session_tag,
            resume: None,
        })
    }

    async fn respond(
        &self,
        carrier: &mut dyn Carrier,
        tls: Option<&TlsInfo>,
    ) -> Result<Opened, ChannelError> {
        let open = carrier.recv().await?;
        if open.get(..3) != Some(&MAGIC[..]) || open.len() < 4 {
            return Err(ChannelError::Other(
                "the dialling side did not open a carrier-authenticated exchange".into(),
            ));
        }
        let (resume, rest) = match open[3] {
            0 => (None, &open[4..]),
            RESUMING if open.len() >= 20 => {
                let offer: SessionTag = open[4..20].try_into().expect("sixteen bytes");
                (Some(offer), &open[20..])
            }
            _ => return Err(truncated()),
        };
        let token = take_token(rest)?;
        let peer = match self.verify(tls, token).await {
            Ok(peer) => peer,
            Err(error) => {
                tracing::warn!(%error, "a connection was refused at its opening");
                // The refusal is best effort: the carrier is dropped either way.
                let _ = carrier.send(vec![REFUSE]).await;
                return Err(error);
            }
        };
        let mut session_tag = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut session_tag);
        let mut accept = vec![ACCEPT];
        accept.extend_from_slice(&session_tag);
        put_token(&mut accept, self.token.as_deref());
        carrier.send(accept).await?;
        Ok(Opened {
            peer,
            seal: Seal::identity(),
            session_tag,
            resume,
        })
    }
}

fn truncated() -> ChannelError {
    ChannelError::Other("a malformed carrier-authenticated exchange".into())
}

fn put_token(out: &mut Vec<u8>, token: Option<&[u8]>) {
    let token = token.unwrap_or_default();
    out.extend_from_slice(&(token.len() as u32).to_be_bytes());
    out.extend_from_slice(token);
}

/// The token at the front of `bytes`, which must be all of them; `None` for
/// an empty one.
fn take_token(bytes: &[u8]) -> Result<Option<&[u8]>, ChannelError> {
    let len = bytes
        .get(..4)
        .map(|len| u32::from_be_bytes(len.try_into().expect("four bytes")) as usize)
        .ok_or_else(truncated)?;
    if len > MAX_TOKEN || bytes.len() != 4 + len {
        return Err(truncated());
    }
    Ok((len > 0).then(|| &bytes[4..]))
}

#[async_trait]
impl Trust for CarrierAuthenticated {
    async fn open(&self, carrier: &mut dyn Carrier, role: Role) -> Result<Opened, ChannelError> {
        let tls = self.admit_carrier(carrier)?;
        let exchange = async {
            match role {
                Role::Initiator => self.initiate(carrier, tls.as_ref()).await,
                Role::Responder => self.respond(carrier, tls.as_ref()).await,
            }
        };
        tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange)
            .await
            .map_err(|_| {
                ChannelError::Other(format!(
                    "the carrier-authenticated opening timed out after {} seconds",
                    HANDSHAKE_TIMEOUT.as_secs()
                ))
            })?
    }

    fn check(&self, peer: &Verified, now: SystemTime) -> Result<(), Revoked> {
        self.verifier.check(peer, now)
    }

    async fn reverify(
        &self,
        proof: &[u8],
        tls: Option<&TlsInfo>,
    ) -> Result<Verified, ChannelError> {
        if tls.is_none() && !self.in_memory {
            return Err(ChannelError::Other(
                "a fresh proof arrived over a connection with no TLS".into(),
            ));
        }
        self.verifier
            .verify(PeerContext {
                tls,
                token: Some(proof),
                renewal: true,
            })
            .await
            .map_err(|revoked| {
                ChannelError::Other(format!("the fresh proof was refused: {revoked}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::MemCarrier;
    use crate::trust::PeerIdentity;

    /// Tokens are `user:<sub>`; a peer presenting none is the service.
    struct Tokens;

    #[async_trait]
    impl Verifier for Tokens {
        async fn verify(&self, peer: PeerContext<'_>) -> Result<Verified, Revoked> {
            let identity = match peer.token {
                None => PeerIdentity::Service { id: "panel".into() },
                Some(token) => match token.strip_prefix(b"user:") {
                    Some(sub) => PeerIdentity::User {
                        sub: String::from_utf8_lossy(sub).into(),
                        aud: "panel".into(),
                    },
                    None => return Err(Revoked("not a token this end issued".into())),
                },
            };
            Ok(Verified::lasting(identity))
        }

        fn check(&self, _: &Verified, _: SystemTime) -> Result<(), Revoked> {
            Ok(())
        }
    }

    fn trust() -> CarrierAuthenticated {
        CarrierAuthenticated::insecure_in_memory_for_tests(Arc::new(Tokens))
    }

    async fn open(
        client: CarrierAuthenticated,
        server: CarrierAuthenticated,
    ) -> (Result<Opened, ChannelError>, Result<Opened, ChannelError>) {
        let (mut a, mut b) = MemCarrier::pair();
        tokio::join!(
            client.open(&mut a, Role::Initiator),
            server.open(&mut b, Role::Responder)
        )
    }

    #[tokio::test]
    async fn both_ends_learn_who_the_other_is_and_share_the_tag_the_acceptor_drew() {
        let (client, server) = open(trust().presenting(b"user:ann".to_vec()), trust()).await;
        let (client, server) = (client.unwrap(), server.unwrap());
        assert_eq!(client.session_tag, server.session_tag);
        assert_ne!(client.session_tag, [0; 16]);
        assert_eq!(
            server.peer.identity,
            PeerIdentity::User {
                sub: "ann".into(),
                aud: "panel".into()
            }
        );
        assert_eq!(
            client.peer.identity,
            PeerIdentity::Service { id: "panel".into() }
        );
        assert!(client.seal.is_identity() && server.seal.is_identity());
        assert_eq!(server.resume, None);
    }

    #[tokio::test]
    async fn a_token_the_verifier_rejects_is_refused_without_its_reason() {
        let (client, server) = open(trust().presenting(b"forged".to_vec()), trust()).await;
        let refused = client
            .err()
            .expect("the dialler hears a refusal")
            .to_string();
        assert!(!refused.contains("issued"), "{refused}");
        assert!(server.is_err());
    }

    #[tokio::test]
    async fn a_resuming_dialler_names_its_session_and_gets_a_fresh_tag() {
        let (client, server) = open(trust().resuming([4; 16]), trust()).await;
        let (client, server) = (client.unwrap(), server.unwrap());
        assert_eq!(server.resume, Some([4; 16]), "no exporter in memory");
        assert_ne!(client.session_tag, [4; 16]);
    }

    #[tokio::test]
    async fn without_the_test_constructor_the_in_memory_carrier_is_refused() {
        let strict = CarrierAuthenticated::new(Arc::new(Tokens));
        let (mut a, _b) = MemCarrier::pair();
        let refused = strict.open(&mut a, Role::Initiator).await.err().unwrap();
        assert!(refused.to_string().contains("needs TLS"), "{refused}");
    }

    #[test]
    fn a_token_must_fill_exactly_what_its_length_says() {
        assert_eq!(take_token(&[0, 0, 0, 0]).unwrap(), None);
        assert_eq!(take_token(&[0, 0, 0, 1, 9]).unwrap(), Some(&[9][..]));
        assert!(take_token(&[0, 0, 0, 2, 9]).is_err());
        assert!(take_token(&[0, 0, 0, 1, 9, 9]).is_err());
    }
}
