//! Encrypted transport via the Noise protocol.
//!
//! After pairing, both peers hold the same SPAKE2-derived secret. This module
//! wraps any [`Transport`] in an [`EncryptedTransport`] that runs the
//! channel's [`NoisePsk`] handshake keyed by that secret, then encrypts and
//! decrypts every [`Message`]. Layers above (`session::handshake`,
//! `run_heartbeat`) keep working against the [`Transport`] trait, unaware that
//! bytes are now ciphertext.
//!
//! What crosses the inner transport is the channel's frames, each as a
//! [`Message::Opaque`]: the handshake, the tagged chunks and the session's own
//! frames alike (`OpaqueCarrier`). That is the shape every Cursio build has
//! put on the wire, so a peer from before the channel was extracted still
//! forms a session with this one.
//!
//! ## Why `NNpsk0`
//!
//! We already authenticated the peer by password (SPAKE2) and hold a shared
//! secret. `NNpsk0` mixes that secret in as a pre-shared key, giving a
//! confidential, integrity-protected, mutually-authenticated channel without a
//! second identity exchange (which `XX`-style patterns would add redundantly).
//!
//! ## Key derivation
//!
//! Noise requires a 32-byte PSK. The SPAKE2 output isn't guaranteed to be
//! exactly that, so we derive the PSK as `SHA-256("cursio-noise-psk-v1" ||
//! shared_key)` — fixed length, with domain separation so this key can't be
//! confused with the pairing confirmation tags.

use async_trait::async_trait;
use cursio_channel::seal::{chunk, Reassembler, SealedRx, SealedTx};
use cursio_channel::{Carrier, ChannelError, NoisePsk, Seal, Trust};
use cursio_protocol::Message;

use crate::pairing::Role;
use crate::{
    OpaqueCarrier, OpaqueRx, OpaqueTx, Transport, TransportError, TransportRx, TransportTx,
};

/// Domain tag for deriving the Noise PSK from the SPAKE2 secret.
const PSK_DERIVATION_TAG: &[u8] = b"cursio-noise-psk-v1";

/// Errors specific to the encrypted transport.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("noise error: {0}")]
    Noise(String),
    #[error("plaintext message too large for a single Noise frame: {0} bytes")]
    MessageTooLarge(usize),
}

impl From<ChannelError> for CryptoError {
    fn from(e: ChannelError) -> Self {
        match e {
            ChannelError::Noise(message) => CryptoError::Noise(message),
            ChannelError::MessageTooLarge(len) => CryptoError::MessageTooLarge(len),
            other => CryptoError::Transport(other.into()),
        }
    }
}

impl From<CryptoError> for TransportError {
    fn from(e: CryptoError) -> Self {
        TransportError::Other(e.to_string())
    }
}

impl From<Role> for cursio_channel::Role {
    fn from(role: Role) -> Self {
        match role {
            Role::Initiator => cursio_channel::Role::Initiator,
            Role::Responder => cursio_channel::Role::Responder,
        }
    }
}

/// A [`Transport`] whose messages are encrypted with Noise.
///
/// Construct via [`EncryptedTransport::establish`], which performs the Noise
/// handshake over the inner transport before any application messages flow.
pub struct EncryptedTransport<T: Transport> {
    inner: OpaqueCarrier<T>,
    seal: Seal,
    /// Receive-side reassembly state for chunked messages.
    reasm: Reassembler,
    session_tag: cursio_protocol::SessionTag,
}

impl<T: Transport> EncryptedTransport<T> {
    /// Run the Noise `NNpsk0` handshake over `inner`, then return an encrypted
    /// transport ready for application messages.
    ///
    /// `shared_key` is the SPAKE2 secret from pairing; `role` must match the
    /// role used during pairing so initiator/responder agree on handshake
    /// direction. Both peers must call this at the same point in the protocol.
    pub async fn establish(inner: T, shared_key: &[u8], role: Role) -> Result<Self, CryptoError> {
        let mut inner = OpaqueCarrier(inner);
        let opened = NoisePsk::from_secret(PSK_DERIVATION_TAG, shared_key)
            .open(&mut inner, role.into())
            .await?;
        Ok(Self {
            inner,
            seal: opened.seal,
            reasm: Reassembler::default(),
            session_tag: opened.session_tag,
        })
    }

    /// The tag a session started on this connection is known by, on both ends.
    pub fn session_tag(&self) -> cursio_protocol::SessionTag {
        self.session_tag
    }

    /// Take the connection apart into a path a resumable link can carry a
    /// session over: the raw halves, and the Noise state that seals for them.
    pub fn into_path(self, kind: crate::resume::PathKind) -> crate::resume::Path {
        let direct = self.inner.0.local_ip().is_some_and(crate::direct::is_local);
        let (tx, rx) = Box::new(self.inner.0).split();
        crate::resume::Path {
            tx: Box::new(OpaqueTx(tx)),
            rx: Box::new(OpaqueRx(rx)),
            seal: self.seal,
            kind: kind.into(),
            direct,
        }
    }
}

#[async_trait]
impl<T: Transport> Transport for EncryptedTransport<T> {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        for tagged in chunk(&msg.to_bytes()?)? {
            Carrier::send(&mut self.inner, self.seal.seal(&tagged)?).await?;
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        loop {
            let sealed = Carrier::recv(&mut self.inner).await?;
            let tagged = self.seal.open(&sealed)?;
            if let Some(body) = self.reasm.accept(&tagged)? {
                return Ok(Message::from_bytes(&body)?);
            }
        }
    }

    fn kind(&self) -> &'static str {
        "noise"
    }

    fn local_ip(&self) -> Option<std::net::IpAddr> {
        self.inner.0.local_ip()
    }

    fn split(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        let (inner_tx, inner_rx) = Box::new(self.inner.0).split();
        let tx = EncryptedTx(SealedTx::new(
            Box::new(OpaqueTx(inner_tx)),
            self.seal.clone(),
        ));
        let rx = EncryptedRx(SealedRx::new(Box::new(OpaqueRx(inner_rx)), self.seal));
        (Box::new(tx), Box::new(rx))
    }

    /// Split into a **priority-multiplexed** sender and the normal receiver.
    ///
    /// Like [`Transport::split`], but the tx half is a
    /// [`PrioritizedTx`](crate::priority::PrioritizedTx): callers still just
    /// `send(&msg)`, but frames are queued by [`Message::priority`] and written
    /// by a background drainer, so a bulk transfer can't stall the mouse. The
    /// receiver is unchanged — reassembly happens the same way regardless of how
    /// the peer scheduled its sends. This is what the session uses.
    fn split_prioritized(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        let (inner_tx, inner_rx) = Box::new(self.inner.0).split();
        let tx = crate::priority::PrioritizedTx::spawn(
            Box::new(OpaqueTx(inner_tx)),
            self.seal.clone(),
            crate::lane::classes(),
        );
        let rx = EncryptedRx(SealedRx::new(Box::new(OpaqueRx(inner_rx)), self.seal));
        (Box::new(tx), Box::new(rx))
    }
}

/// Send half of a split [`EncryptedTransport`]: seals then writes.
pub struct EncryptedTx(SealedTx);

#[async_trait]
impl TransportTx for EncryptedTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        Ok(self.0.send_body(&msg.to_bytes()?).await?)
    }
}

/// Receive half of a split [`EncryptedTransport`]: reads, opens, reassembles.
pub struct EncryptedRx(SealedRx);

#[async_trait]
impl TransportRx for EncryptedRx {
    async fn recv(&mut self) -> Result<Message, TransportError> {
        Ok(Message::from_bytes(&self.0.recv_body().await?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::MemTransport;
    use cursio_protocol::{Hello, PeerId};

    #[tokio::test(start_paused = true)]
    async fn an_unresponsive_noise_peer_times_out_and_releases_the_transport_in_both_roles() {
        for role in [Role::Initiator, Role::Responder] {
            let (local, mut remote) = MemTransport::pair();
            let started = tokio::time::Instant::now();
            let result = EncryptedTransport::establish(local, b"paired secret", role).await;
            assert!(
                matches!(result, Err(CryptoError::Noise(message)) if message.contains("timed out"))
            );
            assert_eq!(started.elapsed(), cursio_channel::noise::HANDSHAKE_TIMEOUT);
            assert!(remote.send(&Message::Heartbeat { nonce: 1 }).await.is_err());
        }
    }

    /// Bring up both encrypted ends concurrently (the handshake needs both
    /// sides talking at once) and return them.
    async fn establish_pair(
        key: &[u8],
    ) -> (
        EncryptedTransport<MemTransport>,
        EncryptedTransport<MemTransport>,
    ) {
        let (a, b) = MemTransport::pair();
        let (init, resp) = tokio::join!(
            EncryptedTransport::establish(a, key, Role::Initiator),
            EncryptedTransport::establish(b, key, Role::Responder)
        );
        (init.unwrap(), resp.unwrap())
    }

    #[tokio::test]
    async fn encrypted_roundtrip_with_matching_key() {
        let (mut init, mut resp) = establish_pair(b"shared-secret-from-spake2").await;
        let msg = Message::Hello(Hello::bare(PeerId::nil(), "encrypted"));
        init.send(&msg).await.unwrap();
        assert_eq!(resp.recv().await.unwrap(), msg);
        resp.send(&Message::Heartbeat { nonce: 7 }).await.unwrap();
        assert_eq!(init.recv().await.unwrap(), Message::Heartbeat { nonce: 7 });
        assert_eq!(init.session_tag(), resp.session_tag());
    }

    /// What crosses the inner transport: the handshake and every sealed frame
    /// as `Message::Opaque`, the handshake's with a length prefix of its own,
    /// and nothing of the plaintext — the shape a peer of any age expects.
    #[tokio::test]
    async fn every_frame_rides_as_an_opaque_message_and_hides_the_plaintext() {
        let (a, mut far) = MemTransport::pair();
        let opening = tokio::spawn(EncryptedTransport::establish(a, b"k", Role::Initiator));
        let Message::Opaque(first) = far.recv().await.unwrap() else {
            panic!("the handshake must ride as an opaque message");
        };
        // `NNpsk0`'s first message: an ephemeral key and the tag of an empty
        // payload, behind its own big-endian length.
        assert_eq!(first[..4], [0, 0, 0, 48]);
        assert_eq!(first.len(), 4 + 48);
        let mut responder = OpaqueCarrier(far);
        let respond = NoisePsk::from_secret(PSK_DERIVATION_TAG, b"k");
        let mut replay = OpaqueCarrier(ReplayFirst {
            first: Some(Message::Opaque(first)),
            inner: &mut responder,
        });
        let opened = respond
            .open(&mut replay, cursio_channel::Role::Responder)
            .await
            .unwrap();
        let mut init = opening.await.unwrap().unwrap();
        init.send(&Message::Hello(Hello::bare(PeerId::nil(), "TOPSECRET")))
            .await
            .unwrap();
        let Message::Opaque(sealed) = responder.0.recv().await.unwrap() else {
            panic!("a sealed frame must ride as an opaque message");
        };
        assert!(!sealed.windows(9).any(|w| w == b"TOPSECRET"));
        let tagged = opened.seal.open(&sealed).unwrap();
        let mut reasm = Reassembler::default();
        let body = reasm.accept(&tagged).unwrap().unwrap();
        assert_eq!(
            Message::from_bytes(&body).unwrap(),
            Message::Hello(Hello::bare(PeerId::nil(), "TOPSECRET"))
        );
    }

    /// A transport that answers its first `recv` with a message already read
    /// off the one it wraps.
    struct ReplayFirst<'a> {
        first: Option<Message>,
        inner: &'a mut OpaqueCarrier<MemTransport>,
    }

    #[async_trait]
    impl Transport for ReplayFirst<'_> {
        async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
            self.inner.0.send(msg).await
        }
        async fn recv(&mut self) -> Result<Message, TransportError> {
            match self.first.take() {
                Some(first) => Ok(first),
                None => self.inner.0.recv().await,
            }
        }
        fn kind(&self) -> &'static str {
            "replay"
        }
        fn split(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
            unreachable!("the handshake never splits its transport")
        }
    }

    #[tokio::test]
    async fn mismatched_keys_fail_to_communicate() {
        let (a, b) = MemTransport::pair();
        let (init, resp) = tokio::join!(
            EncryptedTransport::establish(a, b"key-A", Role::Initiator),
            EncryptedTransport::establish(b, b"key-B", Role::Responder)
        );
        assert!(
            init.is_err() || resp.is_err(),
            "handshake should fail with mismatched PSKs"
        );
    }

    /// A message far larger than one Noise frame survives a round-trip,
    /// reassembled from its pieces: the clipboard-image / file / video case
    /// that used to kill the session with `MessageTooLarge`.
    #[tokio::test]
    async fn large_message_roundtrips_via_chunking() {
        let (mut init, mut resp) = establish_pair(b"chunk-key").await;
        let big = Message::Opaque((0..300_000u32).map(|i| i as u8).collect());
        init.send(&big).await.unwrap();
        assert_eq!(resp.recv().await.unwrap(), big);
    }

    /// A body just over what fits whole is chunked and still round-trips.
    #[tokio::test]
    async fn just_over_one_frame_roundtrips() {
        let (mut init, mut resp) = establish_pair(b"boundary-key").await;
        let msg = Message::Opaque(vec![0xABu8; 65535 - 16]);
        init.send(&msg).await.unwrap();
        assert_eq!(resp.recv().await.unwrap(), msg);
    }

    #[tokio::test]
    async fn small_after_large_still_decodes() {
        let (mut init, mut resp) = establish_pair(b"seq-key").await;
        let big = Message::Opaque(vec![7u8; 200_000]);
        init.send(&big).await.unwrap();
        assert_eq!(resp.recv().await.unwrap(), big);
        init.send(&Message::Heartbeat { nonce: 42 }).await.unwrap();
        assert_eq!(resp.recv().await.unwrap(), Message::Heartbeat { nonce: 42 });
    }

    #[tokio::test]
    async fn the_split_halves_seal_and_open_for_each_other() {
        let (init, resp) = establish_pair(b"split-key").await;
        let (mut tx, _) = Box::new(init).split();
        let (_, mut rx) = Box::new(resp).split();
        let big = Message::Opaque(vec![1u8; 150_000]);
        tx.send(&big).await.unwrap();
        tx.send(&Message::Bye).await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), big);
        assert_eq!(rx.recv().await.unwrap(), Message::Bye);
    }
}
