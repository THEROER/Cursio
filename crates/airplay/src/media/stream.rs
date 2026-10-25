//! The mirror data connection: its packets decrypted in wire order and handed
//! to a hardware decoder, or decrypted and dropped when nothing shows them here.

use crate::crypto::MirrorDecryptor;
use crate::mirror::{AvcConfig, MirrorPacket, MirrorPacketDecoder, PayloadType};
use crate::AirPlayError;
use std::net::IpAddr;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// How long the advertised `dataPort` waits for the sender before giving the
/// listener and the video window back.
pub(super) const MIRROR_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) trait VideoSink: Send {
    fn push(&mut self, access_unit: &[u8]) -> Result<(), AirPlayError>;
}

/// Accepts access units and draws nothing.
///
/// For a machine that receives the phone's screen only to pass it to peers.
/// The units still have to travel this far — the tap is fed from the same
/// processor, and every payload must cross the decryptor in wire order or the
/// AES-CTR keystream desynchronises — but nothing here has a window to put
/// them in.
pub(super) struct DiscardingSink;

impl VideoSink for DiscardingSink {
    fn push(&mut self, _access_unit: &[u8]) -> Result<(), AirPlayError> {
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub(super) struct HardwareSink(crate::video::HardwareVideoPipeline);

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl VideoSink for HardwareSink {
    fn push(&mut self, access_unit: &[u8]) -> Result<(), AirPlayError> {
        self.0.push_access_unit(access_unit, None)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub(super) fn hardware_video_sink(
    size: crate::video::ScreenSize,
) -> Result<Box<dyn VideoSink>, AirPlayError> {
    Ok(Box::new(HardwareSink(
        crate::video::HardwareVideoPipeline::new(crate::video::VideoCodec::H264, size)?,
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(super) fn hardware_video_sink(
    _size: crate::video::ScreenSize,
) -> Result<Box<dyn VideoSink>, AirPlayError> {
    Err(AirPlayError::Video(
        "AirPlay hardware video output is supported on Linux and Windows".into(),
    ))
}

pub(super) async fn serve_mirror(
    listener: TcpListener,
    peer_ip: IpAddr,
    decryptor: Box<dyn MirrorDecryptor>,
    sink: Box<dyn VideoSink>,
    tap: Option<crate::MirrorTap>,
) -> Result<(), AirPlayError> {
    let mut stream = loop {
        let accepted = tokio::time::timeout(MIRROR_CONNECT_TIMEOUT, listener.accept())
            .await
            .map_err(|_| {
                AirPlayError::Video("the AirPlay sender never opened the mirror stream".into())
            })??;
        let (stream, peer) = accepted;
        if peer.ip() == peer_ip {
            break stream;
        }
        tracing::warn!(%peer, expected = %peer_ip, "rejected foreign AirPlay media connection");
    };
    stream.set_nodelay(true)?;
    let mut processor = MirrorProcessor::new(decryptor, sink, tap);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        processor.push(&buffer[..count])?;
    }
}

pub(super) struct MirrorProcessor {
    decoder: MirrorPacketDecoder,
    decryptor: Box<dyn MirrorDecryptor>,
    sink: Box<dyn VideoSink>,
    /// Where the mesh listens in, when the receiver was given a tap.
    tap: Option<crate::MirrorTap>,
    avc: Option<AvcConfig>,
    plaintext: Vec<u8>,
    access_unit: Vec<u8>,
    skipped: u64,
}

impl MirrorProcessor {
    pub(super) fn new(
        decryptor: Box<dyn MirrorDecryptor>,
        sink: Box<dyn VideoSink>,
        tap: Option<crate::MirrorTap>,
    ) -> Self {
        Self {
            decoder: MirrorPacketDecoder::new(),
            decryptor,
            sink,
            tap,
            avc: None,
            plaintext: Vec::new(),
            access_unit: Vec::new(),
            skipped: 0,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<(), AirPlayError> {
        self.decoder
            .push(bytes)
            .map_err(|error| AirPlayError::Video(error.to_string()))?;
        while let Some(packet) = self
            .decoder
            .next_packet()
            .map_err(|error| AirPlayError::Video(error.to_string()))?
        {
            // Framing errors above desynchronize the stream and end it. A
            // single undecodable payload does not: iOS recovers on its own at
            // the next IDR, so skipping keeps the session alive instead of
            // freezing the picture until the user reconnects.
            if let Err(error) = self.process(&packet) {
                // Not a fault to skip past: there is nothing left to show it
                // on, and the caller ends the session so the sender stops.
                if matches!(error, AirPlayError::ScreenClosed) {
                    return Err(error);
                }
                self.skipped += 1;
                // One line per skipped frame is one line per 16 ms once
                // something goes persistently wrong, which buries whatever
                // else the log was about to say.
                if self.skipped.is_power_of_two() {
                    tracing::warn!(
                        %error,
                        payload_type = ?packet.payload_type,
                        skipped = self.skipped,
                        "skipped an AirPlay mirror packet"
                    );
                }
            }
        }
        Ok(())
    }

    fn process(&mut self, packet: &MirrorPacket) -> Result<(), AirPlayError> {
        match packet.payload_type {
            PayloadType::CodecConfig => {
                let config = AvcConfig::parse(&packet.payload)
                    .map_err(|error| AirPlayError::Video(error.to_string()))?;
                self.sink.push(config.parameter_sets())?;
                if let Some(tap) = self.tap.as_ref() {
                    tap.publish_parameter_sets(config.parameter_sets());
                }
                self.avc = Some(config);
            }
            PayloadType::Video => {
                // The AES-CTR keystream spans the whole connection, so every
                // video payload has to be decrypted in wire order even when it
                // cannot be rendered. Skipping one before decryption would
                // desynchronize the cipher for the rest of the session.
                self.plaintext.clear();
                self.decryptor
                    .decrypt(&packet.payload, &mut self.plaintext)?;
                if self.plaintext.len() != packet.payload.len() {
                    return Err(AirPlayError::Crypto(
                        "mirror decryptor returned a different payload length".into(),
                    ));
                }
                let config = self.avc.as_ref().ok_or_else(|| {
                    AirPlayError::Video("video arrived before AVC configuration".into())
                })?;
                self.access_unit.clear();
                config
                    .convert_access_unit_into(&self.plaintext, &mut self.access_unit)
                    .map_err(|error| AirPlayError::Video(error.to_string()))?;
                self.sink.push(&self.access_unit)?;
                // Only pay for the copy when a peer is actually watching.
                if let Some(tap) = self.tap.as_ref().filter(|tap| tap.has_readers()) {
                    tap.publish_access_unit(&self.access_unit);
                }
            }
            PayloadType::Heartbeat | PayloadType::Unknown(_) => {}
        }
        Ok(())
    }
}
