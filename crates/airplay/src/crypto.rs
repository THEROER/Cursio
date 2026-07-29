//! Pluggable AirPlay pairing and FairPlay boundary.
//!
//! Native mirroring requires protocol state that is separate from Cursio's
//! mesh cryptography. The receiver owns the control flow, while an explicitly
//! supplied provider owns the per-connection pairing/FairPlay implementation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoEndpoint {
    PairSetup,
    PairVerify,
    FairPlaySetup,
}

/// Stateful decryption for one AirPlay mirroring data connection.
///
/// The stream cipher state spans packet boundaries, so ciphertext must be fed
/// in wire order through one instance for the lifetime of the connection.
pub trait MirrorDecryptor: Send {
    fn decrypt(
        &mut self,
        ciphertext: &[u8],
        plaintext: &mut Vec<u8>,
    ) -> Result<(), crate::AirPlayError>;
}

/// Cryptographic state for one AirPlay control connection.
pub trait CryptoSession: Send {
    fn handle(
        &mut self,
        endpoint: CryptoEndpoint,
        request: &[u8],
    ) -> Result<Vec<u8>, crate::AirPlayError>;

    /// Unwrap the encrypted 16-byte media key delivered by the sender.
    fn unwrap_media_key(&mut self, encrypted_key: &[u8]) -> Result<[u8; 16], crate::AirPlayError>;

    /// Build the stream cipher from the FairPlay-wrapped key, verified pairing
    /// secret, and AirPlay stream id.
    fn start_mirror_decryptor(
        &mut self,
        encrypted_key: &[u8],
        stream_connection_id: u64,
    ) -> Result<Box<dyn MirrorDecryptor>, crate::AirPlayError> {
        let _ = (encrypted_key, stream_connection_id);
        Err(crate::AirPlayError::Crypto(
            "crypto provider does not implement mirror stream derivation".into(),
        ))
    }
}

/// Creates isolated cryptographic state for each control connection.
pub trait CryptoProvider: Send + Sync {
    fn start_session(&self) -> Box<dyn CryptoSession>;
}
