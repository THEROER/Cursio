//! Who the other end is, and the keys: the opening exchange over a fresh
//! carrier.
//!
//! A [`Trust`] answers it. [`NoisePsk`] is the one shipped here: a Noise
//! `NNpsk0` handshake keyed by a secret both ends already hold — Cursio's,
//! byte for byte, whose SPAKE2 pairing produces the secret. `NNpsk0` mixes it
//! in as a pre-shared key, giving a confidential, integrity-protected,
//! mutually authenticated channel without a second identity exchange, which
//! an `XX`-style pattern would add redundantly once the peer is known by the
//! secret.
//!
//! Noise needs a 32-byte key, and a shared secret is not guaranteed to be
//! one, so [`NoisePsk::from_secret`] derives it as `SHA-256(domain ||
//! secret)`: fixed length, and separated by the caller's domain tag from any
//! other use of the same secret.
//!
//! Each handshake message travels inside its carrier frame behind a length
//! prefix of its own, `[u32 len][message]`. That prefix carries nothing a
//! frame does not already say; it is kept because Cursio has always sent its
//! handshake that way, and a peer built before this crate must still answer.

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use snow::HandshakeState;

use crate::carrier::{decode_frame, encode_frame};
use crate::seal::{noise_error, Seal};
use crate::{Carrier, ChannelError, SessionTag};

/// `NNpsk0`: no static keys, the PSK mixed at position 0; X25519, ChaChaPoly,
/// BLAKE2s.
const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// A half-open handshake must not reserve a peer's session indefinitely.
pub const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Which side of the opening exchange this end is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The side that dialled.
    Initiator,
    /// The side that accepted.
    Responder,
}

/// What an opening exchange established.
pub struct Opened {
    pub seal: Seal,
    /// The same on both ends and unique to this connection: what names a
    /// session for resumption.
    pub session_tag: SessionTag,
}

/// Who the other end is and the keys to talk to it with.
#[async_trait]
pub trait Trust: Send + Sync {
    /// The opening exchange over a fresh carrier, as the side that dialled
    /// or the side that accepted.
    async fn open(&self, carrier: &mut dyn Carrier, role: Role) -> Result<Opened, ChannelError>;
}

/// `Noise_NNpsk0` keyed by a secret both ends hold.
#[derive(Clone)]
pub struct NoisePsk {
    psk: [u8; 32],
}

impl NoisePsk {
    /// Key the handshake with `SHA-256(domain || secret)`.
    pub fn from_secret(domain: &[u8], secret: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain);
        hasher.update(secret);
        Self {
            psk: hasher.finalize().into(),
        }
    }

    async fn initiate(
        carrier: &mut dyn Carrier,
        mut hs: HandshakeState,
    ) -> Result<HandshakeState, ChannelError> {
        // -> e
        let mut buf = vec![0u8; 1024];
        let len = hs.write_message(&[], &mut buf).map_err(noise_error)?;
        carrier.send(encode_frame(&buf[..len])?).await?;
        // Each step said, because a handshake that stalls says nothing else:
        // a Bluetooth join was seen to stop here for minutes.
        tracing::debug!(
            kind = carrier.kind(),
            "noise: first message sent, waiting for the answer"
        );
        // <- e, ee, psk
        let answer = recv_message(carrier).await?;
        tracing::debug!(kind = carrier.kind(), "noise: answer received");
        let mut scratch = vec![0u8; 1024];
        hs.read_message(&answer, &mut scratch)
            .map_err(noise_error)?;
        Ok(hs)
    }

    async fn respond(
        carrier: &mut dyn Carrier,
        mut hs: HandshakeState,
    ) -> Result<HandshakeState, ChannelError> {
        // -> e
        tracing::debug!(
            kind = carrier.kind(),
            "noise: waiting for the first message"
        );
        let first = recv_message(carrier).await?;
        tracing::debug!(
            kind = carrier.kind(),
            "noise: first message received, answering"
        );
        let mut scratch = vec![0u8; 1024];
        hs.read_message(&first, &mut scratch).map_err(noise_error)?;
        // <- e, ee, psk
        let mut buf = vec![0u8; 1024];
        let len = hs.write_message(&[], &mut buf).map_err(noise_error)?;
        carrier.send(encode_frame(&buf[..len])?).await?;
        Ok(hs)
    }
}

async fn recv_message(carrier: &mut dyn Carrier) -> Result<Vec<u8>, ChannelError> {
    let frame = carrier.recv().await?;
    decode_frame(&frame)?
        .map(|(payload, _)| payload)
        .ok_or_else(|| ChannelError::Noise("truncated handshake frame".into()))
}

#[async_trait]
impl Trust for NoisePsk {
    async fn open(&self, carrier: &mut dyn Carrier, role: Role) -> Result<Opened, ChannelError> {
        let params = NOISE_PARAMS
            .parse()
            .map_err(|e| ChannelError::Noise(format!("bad params: {e:?}")))?;
        let builder = snow::Builder::new(params).psk(0, &self.psk);
        let hs = match role {
            Role::Initiator => builder.build_initiator(),
            Role::Responder => builder.build_responder(),
        }
        .map_err(noise_error)?;

        let kind = carrier.kind();
        let exchange = async {
            match role {
                Role::Initiator => Self::initiate(carrier, hs).await,
                Role::Responder => Self::respond(carrier, hs).await,
            }
        };
        let hs = tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange)
            .await
            .map_err(|_| {
                ChannelError::Noise(format!(
                    "Noise handshake over {kind} timed out after {} seconds",
                    HANDSHAKE_TIMEOUT.as_secs()
                ))
            })??;
        let mut session_tag = [0u8; 16];
        session_tag.copy_from_slice(&Sha256::digest(hs.get_handshake_hash())[..16]);
        Ok(Opened {
            seal: Seal::new(hs.into_transport_mode().map_err(noise_error)?),
            session_tag,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::MemCarrier;

    #[test]
    fn psk_derivation_is_deterministic_and_separated_by_secret() {
        let a = NoisePsk::from_secret(b"domain", b"some-shared-secret");
        let b = NoisePsk::from_secret(b"domain", b"some-shared-secret");
        assert_eq!(a.psk, b.psk);
        assert_ne!(a.psk, NoisePsk::from_secret(b"domain", b"secret-b").psk);
        assert_ne!(
            a.psk,
            NoisePsk::from_secret(b"other", b"some-shared-secret").psk
        );
    }

    async fn open_pair(a_key: &[u8], b_key: &[u8]) -> [Result<Opened, ChannelError>; 2] {
        let (mut a, mut b) = MemCarrier::pair();
        let (ta, tb) = (
            NoisePsk::from_secret(b"t", a_key),
            NoisePsk::from_secret(b"t", b_key),
        );
        let (ra, rb) = tokio::join!(
            ta.open(&mut a, Role::Initiator),
            tb.open(&mut b, Role::Responder)
        );
        [ra, rb]
    }

    #[tokio::test]
    async fn both_ends_seal_for_each_other_and_name_the_session_alike() {
        let [a, b] = open_pair(b"k", b"k").await;
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.session_tag, b.session_tag);
        let sealed = a.seal.seal(b"TOPSECRET").unwrap();
        assert!(!sealed.windows(9).any(|w| w == b"TOPSECRET"));
        assert_eq!(b.seal.open(&sealed).unwrap(), b"TOPSECRET");
        assert_eq!(
            a.seal.open(&b.seal.seal(b"back").unwrap()).unwrap(),
            b"back"
        );
    }

    #[tokio::test]
    async fn mismatched_secrets_fail_to_communicate() {
        let [a, b] = open_pair(b"key-A", b"key-B").await;
        assert!(a.is_err() || b.is_err());
    }

    #[tokio::test]
    async fn the_handshake_frames_carry_a_length_prefix_of_their_own() {
        let (mut a, mut far) = MemCarrier::pair();
        let opening = tokio::spawn(async move {
            NoisePsk::from_secret(b"t", b"k")
                .open(&mut a, Role::Initiator)
                .await
        });
        let first = far.recv().await.unwrap();
        assert_eq!(
            u32::from_be_bytes(first[..4].try_into().unwrap()) as usize,
            first.len() - 4
        );
        drop(far);
        assert!(opening.await.unwrap().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn an_unresponsive_peer_times_out_in_both_roles() {
        for role in [Role::Initiator, Role::Responder] {
            let (mut local, _remote) = MemCarrier::pair();
            let started = tokio::time::Instant::now();
            let result = NoisePsk::from_secret(b"t", b"k")
                .open(&mut local, role)
                .await;
            assert!(
                matches!(result, Err(ChannelError::Noise(message)) if message.contains("timed out"))
            );
            assert_eq!(started.elapsed(), HANDSHAKE_TIMEOUT);
        }
    }
}
