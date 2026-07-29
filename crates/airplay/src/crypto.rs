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

/// Cryptographic state for one AirPlay control connection.
pub trait CryptoSession: Send {
    fn handle(
        &mut self,
        endpoint: CryptoEndpoint,
        request: &[u8],
    ) -> Result<Vec<u8>, crate::AirPlayError>;

    /// Unwrap the encrypted 16-byte media key delivered by the sender.
    fn unwrap_media_key(&mut self, encrypted_key: &[u8]) -> Result<[u8; 16], crate::AirPlayError>;
}

/// Creates isolated cryptographic state for each control connection.
pub trait CryptoProvider: Send + Sync {
    fn start_session(&self) -> Box<dyn CryptoSession>;
}
