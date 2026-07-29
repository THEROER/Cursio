//! AirPlay receiver foundations.
//!
//! This crate deliberately owns a listener separate from Cursio's mesh
//! transport. An iPhone speaks AirPlay control and media protocols, not the
//! Cursio Hello/Noise session protocol. At this stage the crate provides
//! discovery, bounded HTTP/RTSP framing, and the initial `/info` probe. Pairing,
//! FairPlay, and media setup fail closed until their protocol stages exist.

mod config;
pub mod crypto;
pub mod discovery;
mod receiver;
pub mod rtsp;
pub mod video;

pub use config::{ConfigError, DeviceId, ReceiverConfig, Resolution, VideoProfile};
pub use receiver::Receiver;

/// Errors produced while advertising or serving the receiver.
#[derive(Debug, thiserror::Error)]
pub enum AirPlayError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("mDNS error: {0}")]
    Mdns(String),
    #[error("LAN discovery error: {0}")]
    Discovery(#[from] cursio_transport::TransportError),
    #[error("control protocol error: {0}")]
    Control(#[from] rtsp::ProtocolError),
    #[error("property-list error: {0}")]
    Plist(#[from] plist::Error),
    #[error("invalid receiver configuration: {0}")]
    Config(#[from] ConfigError),
    #[error("video pipeline error: {0}")]
    Video(String),
    #[error("AirPlay cryptography error: {0}")]
    Crypto(String),
}
