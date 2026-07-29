use std::ffi::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::Arc;

use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha512};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use super::{CryptoEndpoint, CryptoProvider, CryptoSession, MirrorDecryptor};
use crate::AirPlayError;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

const PAIR_VERIFY_KEY_SALT: &[u8] = b"Pair-Verify-AES-Key";
const PAIR_VERIFY_IV_SALT: &[u8] = b"Pair-Verify-AES-IV";
const FAIRPLAY_SETUP_BYTES: usize = 16;
const FAIRPLAY_HANDSHAKE_BYTES: usize = 164;
const FAIRPLAY_KEY_BYTES: usize = 72;

unsafe extern "C" {
    fn fairplay_init(logger: *mut c_void) -> *mut c_void;
    fn fairplay_setup(fairplay: *mut c_void, request: *const u8, response: *mut u8) -> c_int;
    fn fairplay_handshake(fairplay: *mut c_void, request: *const u8, response: *mut u8) -> c_int;
    fn fairplay_decrypt(fairplay: *mut c_void, input: *const u8, output: *mut u8) -> c_int;
    fn fairplay_destroy(fairplay: *mut c_void);
}

/// AirPlay's transient Ed25519/X25519 pairing and FairPlay SAP provider.
///
/// One long-term Ed25519 identity is shared by all control connections for the
/// lifetime of the provider. Each connection receives isolated handshake and
/// stream-cipher state.
pub struct AppleCryptoProvider {
    signing_key: Arc<SigningKey>,
}

impl AppleCryptoProvider {
    pub fn new() -> Self {
        Self {
            signing_key: Arc::new(SigningKey::generate(&mut OsRng)),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }
}

impl Default for AppleCryptoProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CryptoProvider for AppleCryptoProvider {
    fn start_session(&self) -> Box<dyn CryptoSession> {
        Box::new(AppleCryptoSession {
            signing_key: self.signing_key.clone(),
            fairplay: FairPlayContext::new(),
            setup_done: false,
            client_signing_key: None,
            client_x25519_key: None,
            receiver_x25519_key: None,
            shared_secret: None,
            verified: false,
        })
    }
}

struct AppleCryptoSession {
    signing_key: Arc<SigningKey>,
    fairplay: Option<FairPlayContext>,
    setup_done: bool,
    client_signing_key: Option<VerifyingKey>,
    client_x25519_key: Option<[u8; 32]>,
    receiver_x25519_key: Option<[u8; 32]>,
    shared_secret: Option<[u8; 32]>,
    verified: bool,
}

impl AppleCryptoSession {
    fn handle_fairplay(&mut self, request: &[u8]) -> Result<Vec<u8>, AirPlayError> {
        let fairplay = self
            .fairplay
            .as_mut()
            .ok_or_else(|| crypto_error("could not allocate the FairPlay session"))?;
        match request.len() {
            FAIRPLAY_SETUP_BYTES => {
                if request[4] != 0x03 {
                    return Err(crypto_error("unsupported FairPlay version"));
                }
                if request[14] > 3 {
                    return Err(crypto_error("unsupported FairPlay mode"));
                }
                let mut response = vec![0_u8; 142];
                let status = unsafe {
                    fairplay_setup(
                        fairplay.raw.as_ptr(),
                        request.as_ptr(),
                        response.as_mut_ptr(),
                    )
                };
                if status != 0 {
                    return Err(crypto_error("FairPlay setup failed"));
                }
                Ok(response)
            }
            FAIRPLAY_HANDSHAKE_BYTES => {
                if request[4] != 0x03 {
                    return Err(crypto_error("unsupported FairPlay version"));
                }
                let mut response = vec![0_u8; 32];
                let status = unsafe {
                    fairplay_handshake(
                        fairplay.raw.as_ptr(),
                        request.as_ptr(),
                        response.as_mut_ptr(),
                    )
                };
                if status != 0 {
                    return Err(crypto_error("FairPlay handshake failed"));
                }
                Ok(response)
            }
            length => Err(crypto_error(format!(
                "invalid FairPlay request length: {length}"
            ))),
        }
    }

    fn handle_pair_setup(&mut self, request: &[u8]) -> Result<Vec<u8>, AirPlayError> {
        if request.len() != 32 {
            return Err(crypto_error(format!(
                "invalid pair-setup request length: {}",
                request.len()
            )));
        }
        self.setup_done = true;
        self.verified = false;
        Ok(self.signing_key.verifying_key().to_bytes().to_vec())
    }

    fn handle_pair_verify(&mut self, request: &[u8]) -> Result<Vec<u8>, AirPlayError> {
        if !self.setup_done {
            return Err(crypto_error("pair-verify arrived before pair-setup"));
        }
        if request.len() != 68 {
            return Err(crypto_error(format!(
                "invalid pair-verify request length: {}",
                request.len()
            )));
        }
        match request[0] {
            1 => self.start_pair_verify(request),
            0 => self.finish_pair_verify(request),
            stage => Err(crypto_error(format!(
                "unsupported pair-verify stage: {stage}"
            ))),
        }
    }

    fn start_pair_verify(&mut self, request: &[u8]) -> Result<Vec<u8>, AirPlayError> {
        let client_x25519_key: [u8; 32] = request[4..36]
            .try_into()
            .expect("pair-verify length was checked");
        let client_ed25519_key: [u8; 32] = request[36..68]
            .try_into()
            .expect("pair-verify length was checked");
        let client_signing_key = VerifyingKey::from_bytes(&client_ed25519_key)
            .map_err(|_| crypto_error("invalid pair-verify Ed25519 public key"))?;

        let receiver_secret = StaticSecret::random_from_rng(OsRng);
        let receiver_public = X25519PublicKey::from(&receiver_secret).to_bytes();
        let client_public = X25519PublicKey::from(client_x25519_key);
        let shared_secret = receiver_secret.diffie_hellman(&client_public).to_bytes();

        let mut signed = [0_u8; 64];
        signed[..32].copy_from_slice(&receiver_public);
        signed[32..].copy_from_slice(&client_x25519_key);
        let mut encrypted_signature = self.signing_key.sign(&signed).to_bytes();
        pair_verify_cipher(&shared_secret).apply_keystream(&mut encrypted_signature);

        self.client_signing_key = Some(client_signing_key);
        self.client_x25519_key = Some(client_x25519_key);
        self.receiver_x25519_key = Some(receiver_public);
        self.shared_secret = Some(shared_secret);
        self.verified = false;

        let mut response = Vec::with_capacity(96);
        response.extend_from_slice(&receiver_public);
        response.extend_from_slice(&encrypted_signature);
        Ok(response)
    }

    fn finish_pair_verify(&mut self, request: &[u8]) -> Result<Vec<u8>, AirPlayError> {
        let shared_secret = self
            .shared_secret
            .ok_or_else(|| crypto_error("pair-verify finish arrived before its handshake"))?;
        let client_public = self
            .client_x25519_key
            .ok_or_else(|| crypto_error("missing client pair-verify key"))?;
        let receiver_public = self
            .receiver_x25519_key
            .ok_or_else(|| crypto_error("missing receiver pair-verify key"))?;
        let client_signing_key = self
            .client_signing_key
            .as_ref()
            .ok_or_else(|| crypto_error("missing client Ed25519 key"))?;

        let mut cipher = pair_verify_cipher(&shared_secret);
        let mut skipped = [0_u8; 64];
        cipher.apply_keystream(&mut skipped);
        let mut signature_bytes: [u8; 64] = request[4..]
            .try_into()
            .expect("pair-verify length was checked");
        cipher.apply_keystream(&mut signature_bytes);

        let mut signed = [0_u8; 64];
        signed[..32].copy_from_slice(&client_public);
        signed[32..].copy_from_slice(&receiver_public);
        client_signing_key
            .verify(&signed, &Signature::from_bytes(&signature_bytes))
            .map_err(|_| crypto_error("pair-verify signature verification failed"))?;
        self.verified = true;
        Ok(Vec::new())
    }
}

impl CryptoSession for AppleCryptoSession {
    fn handle(
        &mut self,
        endpoint: CryptoEndpoint,
        request: &[u8],
    ) -> Result<Vec<u8>, AirPlayError> {
        match endpoint {
            CryptoEndpoint::PairSetup => self.handle_pair_setup(request),
            CryptoEndpoint::PairVerify => self.handle_pair_verify(request),
            CryptoEndpoint::FairPlaySetup => self.handle_fairplay(request),
        }
    }

    fn unwrap_media_key(&mut self, encrypted_key: &[u8]) -> Result<[u8; 16], AirPlayError> {
        if encrypted_key.len() != FAIRPLAY_KEY_BYTES {
            return Err(crypto_error(format!(
                "invalid FairPlay media key length: {}",
                encrypted_key.len()
            )));
        }
        if !self.verified {
            return Err(crypto_error(
                "cannot unwrap the media key before pair-verify completes",
            ));
        }
        let fairplay = self
            .fairplay
            .as_mut()
            .ok_or_else(|| crypto_error("FairPlay session is unavailable"))?;
        let mut key = [0_u8; 16];
        let status = unsafe {
            fairplay_decrypt(
                fairplay.raw.as_ptr(),
                encrypted_key.as_ptr(),
                key.as_mut_ptr(),
            )
        };
        if status != 0 {
            return Err(crypto_error(
                "media key arrived before the FairPlay handshake completed",
            ));
        }

        let shared_secret = self
            .shared_secret
            .ok_or_else(|| crypto_error("missing pair-verify shared secret"))?;
        let mut hasher = Sha512::new();
        hasher.update(key);
        hasher.update(shared_secret);
        key.copy_from_slice(&hasher.finalize()[..16]);
        Ok(key)
    }

    fn start_mirror_decryptor(
        &mut self,
        encrypted_key: &[u8],
        stream_connection_id: u64,
    ) -> Result<Box<dyn MirrorDecryptor>, AirPlayError> {
        let media_key = self.unwrap_media_key(encrypted_key)?;
        Ok(Box::new(AppleMirrorDecryptor::new(
            media_key,
            stream_connection_id,
        )))
    }
}

struct FairPlayContext {
    raw: NonNull<c_void>,
}

impl FairPlayContext {
    fn new() -> Option<Self> {
        NonNull::new(unsafe { fairplay_init(std::ptr::null_mut()) }).map(|raw| Self { raw })
    }
}

// The native context is isolated to one control task and has no thread affinity.
unsafe impl Send for FairPlayContext {}

impl Drop for FairPlayContext {
    fn drop(&mut self) {
        unsafe { fairplay_destroy(self.raw.as_ptr()) };
    }
}

struct AppleMirrorDecryptor {
    cipher: Aes128Ctr,
}

impl AppleMirrorDecryptor {
    fn new(media_key: [u8; 16], stream_connection_id: u64) -> Self {
        let key = stream_value(b"AirPlayStreamKey", stream_connection_id, &media_key);
        let iv = stream_value(b"AirPlayStreamIV", stream_connection_id, &media_key);
        Self {
            cipher: Aes128Ctr::new(&key.into(), &iv.into()),
        }
    }
}

impl MirrorDecryptor for AppleMirrorDecryptor {
    fn decrypt(&mut self, ciphertext: &[u8], plaintext: &mut Vec<u8>) -> Result<(), AirPlayError> {
        plaintext.clear();
        plaintext.extend_from_slice(ciphertext);
        self.cipher.apply_keystream(plaintext);
        Ok(())
    }
}

fn pair_verify_cipher(shared_secret: &[u8; 32]) -> Aes128Ctr {
    let key = salted_value(PAIR_VERIFY_KEY_SALT, shared_secret);
    let iv = salted_value(PAIR_VERIFY_IV_SALT, shared_secret);
    Aes128Ctr::new(&key.into(), &iv.into())
}

fn salted_value(salt: &[u8], value: &[u8]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(salt);
    hasher.update(value);
    hasher.finalize()[..16]
        .try_into()
        .expect("SHA-512 contains sixteen bytes")
}

fn stream_value(prefix: &[u8], stream_connection_id: u64, media_key: &[u8; 16]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(prefix);
    hasher.update(stream_connection_id.to_string().as_bytes());
    hasher.update(media_key);
    hasher.finalize()[..16]
        .try_into()
        .expect("SHA-512 contains sixteen bytes")
}

fn crypto_error(message: impl Into<String>) -> AirPlayError {
    AirPlayError::Crypto(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fairplay_setup_and_handshake_match_wire_shapes() {
        let provider = AppleCryptoProvider::new();
        let mut session = provider.start_session();

        let mut setup = [0_u8; 16];
        setup[..4].copy_from_slice(b"FPLY");
        setup[4] = 3;
        setup[14] = 0;
        let response = session
            .handle(CryptoEndpoint::FairPlaySetup, &setup)
            .unwrap();
        assert_eq!(response.len(), 142);
        assert_eq!(&response[..7], b"FPLY\x03\x01\x02");

        let mut handshake = [0_u8; 164];
        handshake[..4].copy_from_slice(b"FPLY");
        handshake[4] = 3;
        for (index, byte) in handshake[144..].iter_mut().enumerate() {
            *byte = index as u8;
        }
        let response = session
            .handle(CryptoEndpoint::FairPlaySetup, &handshake)
            .unwrap();
        assert_eq!(response.len(), 32);
        assert_eq!(&response[..12], b"FPLY\x03\x01\x04\0\0\0\0\x14");
        assert_eq!(&response[12..], &handshake[144..]);

        setup[4] = 2;
        assert!(session
            .handle(CryptoEndpoint::FairPlaySetup, &setup)
            .is_err());
    }

    #[test]
    fn pair_verify_completes_and_rejects_tampering() {
        let provider = AppleCryptoProvider::new();
        let client_signing = SigningKey::generate(&mut OsRng);
        let client_secret = StaticSecret::random_from_rng(OsRng);
        let client_public = X25519PublicKey::from(&client_secret).to_bytes();

        let mut session = provider.start_session();
        session
            .handle(CryptoEndpoint::PairSetup, &[0_u8; 32])
            .unwrap();
        let mut first = [0_u8; 68];
        first[0] = 1;
        first[4..36].copy_from_slice(&client_public);
        first[36..].copy_from_slice(&client_signing.verifying_key().to_bytes());
        let response = session.handle(CryptoEndpoint::PairVerify, &first).unwrap();
        assert_eq!(response.len(), 96);

        let receiver_public: [u8; 32] = response[..32].try_into().unwrap();
        let shared_secret = client_secret
            .diffie_hellman(&X25519PublicKey::from(receiver_public))
            .to_bytes();
        let mut receiver_signature: [u8; 64] = response[32..].try_into().unwrap();
        pair_verify_cipher(&shared_secret).apply_keystream(&mut receiver_signature);
        let mut receiver_message = [0_u8; 64];
        receiver_message[..32].copy_from_slice(&receiver_public);
        receiver_message[32..].copy_from_slice(&client_public);
        provider
            .signing_key
            .verifying_key()
            .verify(
                &receiver_message,
                &Signature::from_bytes(&receiver_signature),
            )
            .unwrap();

        let mut client_message = [0_u8; 64];
        client_message[..32].copy_from_slice(&client_public);
        client_message[32..].copy_from_slice(&receiver_public);
        let mut client_signature = client_signing.sign(&client_message).to_bytes();
        let mut cipher = pair_verify_cipher(&shared_secret);
        let mut skipped = [0_u8; 64];
        cipher.apply_keystream(&mut skipped);
        cipher.apply_keystream(&mut client_signature);
        let mut second = [0_u8; 68];
        second[4..].copy_from_slice(&client_signature);
        assert!(session.handle(CryptoEndpoint::PairVerify, &second).is_ok());

        let mut tampered_session = provider.start_session();
        tampered_session
            .handle(CryptoEndpoint::PairSetup, &[0_u8; 32])
            .unwrap();
        let response = tampered_session
            .handle(CryptoEndpoint::PairVerify, &first)
            .unwrap();
        let receiver_public: [u8; 32] = response[..32].try_into().unwrap();
        let shared_secret = client_secret
            .diffie_hellman(&X25519PublicKey::from(receiver_public))
            .to_bytes();
        client_message[32..].copy_from_slice(&receiver_public);
        let mut client_signature = client_signing.sign(&client_message).to_bytes();
        let mut cipher = pair_verify_cipher(&shared_secret);
        cipher.apply_keystream(&mut skipped);
        cipher.apply_keystream(&mut client_signature);
        client_signature[3] ^= 0x80;
        second[4..].copy_from_slice(&client_signature);
        assert!(tampered_session
            .handle(CryptoEndpoint::PairVerify, &second)
            .is_err());
    }

    #[test]
    fn mirror_ctr_state_is_continuous_across_calls() {
        let media_key = [0x5a; 16];
        let stream_id = 42;
        let ciphertext: Vec<u8> = (0..79).collect();

        let mut one_shot = AppleMirrorDecryptor::new(media_key, stream_id);
        let mut expected = Vec::new();
        one_shot.decrypt(&ciphertext, &mut expected).unwrap();

        let mut chunked = AppleMirrorDecryptor::new(media_key, stream_id);
        let mut first = Vec::new();
        let mut second = Vec::new();
        chunked.decrypt(&ciphertext[..13], &mut first).unwrap();
        chunked.decrypt(&ciphertext[13..], &mut second).unwrap();
        first.extend_from_slice(&second);
        assert_eq!(first, expected);
    }
}
