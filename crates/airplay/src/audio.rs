//! The audio that accompanies a mirrored iPhone screen.
//!
//! Video and sound are negotiated as separate AirPlay streams: the screen is
//! stream type 110 over TCP, the sound is type 96 over UDP with an RTP header.
//! Until this existed a `SETUP` asking only for audio found nothing it
//! recognised and was answered `461 Unsupported Transport`, so iOS mirrored
//! silently.
//!
//! ## What this stage does and does not do
//!
//! It negotiates the stream, receives it, and reports exactly what the sender
//! is sending. It does **not** yet decrypt or decode: the payload is AAC-ELD
//! under a key whose derivation this crate has not established, and writing
//! either from guesswork produces code that looks plausible, runs, and is
//! silently wrong forever. The description logged here is what pins those down
//! against a real device.

use std::net::{IpAddr, SocketAddr};

use plist::{Dictionary, Value};
use tokio::net::UdpSocket;

use crate::AirPlayError;

/// AirPlay's stream type for the audio that accompanies mirroring.
pub(crate) const AUDIO_STREAM_TYPE: u64 = 96;

/// Fixed part of an RTP header: version/flags, payload type, sequence,
/// timestamp, SSRC.
const RTP_HEADER_BYTES: usize = 12;

/// Largest datagram worth accepting. AirPlay audio frames are far smaller;
/// anything near this is not a frame we understand.
const MAX_AUDIO_DATAGRAM: usize = 4 * 1024;

/// What the sender asked for in a `SETUP` carrying an audio stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioStreamRequest {
    /// Compression type. AirPlay mirroring uses AAC-ELD; other values exist for
    /// the audio-only (RAOP) profile.
    pub compression: Option<u64>,
    /// Samples per frame — 480 for the low-delay profile mirroring uses.
    pub samples_per_frame: Option<u64>,
    /// Channels the sender will produce.
    pub channels: Option<u64>,
    /// Sample rate the sender will produce.
    pub sample_rate: Option<u64>,
    /// The sender's control port, when it offered one.
    pub remote_control_port: Option<u16>,
    /// Opaque id used to key this stream's cipher.
    pub stream_connection_id: Option<u64>,
}

impl AudioStreamRequest {
    /// Read one `streams` entry that describes audio.
    pub(crate) fn parse(stream: &Dictionary) -> Self {
        let integer = |key: &str| stream.get(key).and_then(crate::media::plist_u64_bits);
        Self {
            compression: integer("ct"),
            samples_per_frame: integer("spf"),
            channels: integer("ch"),
            sample_rate: integer("sr"),
            remote_control_port: integer("controlPort").and_then(|port| u16::try_from(port).ok()),
            stream_connection_id: integer("streamConnectionID"),
        }
    }

    /// A human-readable summary for the log, so an unfamiliar sender can be
    /// identified without a packet capture.
    pub(crate) fn describe(&self) -> String {
        format!(
            "ct={:?} spf={:?} ch={:?} sr={:?} controlPort={:?}",
            self.compression,
            self.samples_per_frame,
            self.channels,
            self.sample_rate,
            self.remote_control_port
        )
    }
}

/// The `streams` entry answering an audio `SETUP`.
pub(crate) fn setup_response(data_port: u16, control_port: u16) -> Value {
    let mut stream = Dictionary::new();
    stream.insert("type".into(), AUDIO_STREAM_TYPE.into());
    stream.insert("dataPort".into(), u64::from(data_port).into());
    stream.insert("controlPort".into(), u64::from(control_port).into());
    Value::Dictionary(stream)
}

/// One received audio datagram, split into its RTP header fields and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioPacket {
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub payload_bytes: usize,
    /// The first bytes of the payload, hex-encoded.
    ///
    /// This is the one thing a log can say that settles what the payload
    /// actually is: AAC-ELD framing has recognisable leading bits, while
    /// ciphertext has none — so a glance decides whether the decode stage needs
    /// a key first, without a packet capture or a guess.
    pub prefix: String,
}

impl AudioPacket {
    pub(crate) fn parse(datagram: &[u8]) -> Option<Self> {
        if datagram.len() <= RTP_HEADER_BYTES || datagram.len() > MAX_AUDIO_DATAGRAM {
            return None;
        }
        let payload = &datagram[RTP_HEADER_BYTES..];
        Some(Self {
            // The top bit is the marker flag, not part of the payload type.
            payload_type: datagram[1] & 0x7f,
            sequence: u16::from_be_bytes([datagram[2], datagram[3]]),
            timestamp: u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]),
            payload_bytes: payload.len(),
            prefix: payload
                .iter()
                .take(16)
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        })
    }
}

/// Bind the pair of UDP sockets an audio stream needs.
pub(crate) async fn bind_ports(local_ip: IpAddr) -> Result<(UdpSocket, UdpSocket), AirPlayError> {
    let data = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
    let control = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
    Ok((data, control))
}

/// Receive the audio stream and report what it carries.
///
/// Deliberately observational for now. Every field logged here is one that the
/// decode stage needs to get right, and reading them off a real iPhone is the
/// only way to know them.
pub(crate) async fn observe(socket: UdpSocket, request: AudioStreamRequest) {
    let mut datagram = [0_u8; MAX_AUDIO_DATAGRAM];
    let mut received = 0_u64;
    let mut malformed = 0_u64;
    loop {
        let Ok(count) = socket.recv(&mut datagram).await else {
            tracing::debug!(received, "AirPlay audio stream ended");
            return;
        };
        match AudioPacket::parse(&datagram[..count]) {
            Some(packet) => {
                received += 1;
                // The first few describe the stream; after that this would be
                // one line per 10 ms of sound.
                if received <= 3 || received.is_power_of_two() {
                    tracing::info!(
                        received,
                        payload_type = packet.payload_type,
                        sequence = packet.sequence,
                        timestamp = packet.timestamp,
                        payload_bytes = packet.payload_bytes,
                        prefix = %packet.prefix,
                        setup = %request.describe(),
                        "AirPlay audio packet"
                    );
                }
            }
            None => {
                malformed += 1;
                if malformed.is_power_of_two() {
                    tracing::warn!(
                        malformed,
                        bytes = count,
                        "unrecognised AirPlay audio packet"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp(payload_type: u8, sequence: u16, timestamp: u32, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x80, payload_type];
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&[0, 0, 0, 1]);
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn rtp_headers_are_split_from_the_payload() {
        let packet = AudioPacket::parse(&rtp(96, 7, 1234, &[1, 2, 3, 4])).unwrap();
        assert_eq!(packet.payload_type, 96);
        assert_eq!(packet.sequence, 7);
        assert_eq!(packet.timestamp, 1234);
        assert_eq!(packet.payload_bytes, 4);
        assert_eq!(packet.prefix, "01020304");
    }

    /// Long payloads are summarised, not dumped: the point is to identify the
    /// format, and a log line per 10 ms of sound must stay readable.
    #[test]
    fn only_the_start_of_a_payload_is_logged() {
        let packet = AudioPacket::parse(&rtp(96, 1, 0, &[0xab; 400])).unwrap();
        assert_eq!(packet.payload_bytes, 400);
        assert_eq!(packet.prefix.len(), 32);
    }

    /// The marker bit rides in the same byte as the payload type and must not
    /// be read as part of it.
    #[test]
    fn the_marker_bit_is_not_part_of_the_payload_type() {
        let packet = AudioPacket::parse(&rtp(96 | 0x80, 1, 0, &[1])).unwrap();
        assert_eq!(packet.payload_type, 96);
    }

    #[test]
    fn empty_and_oversized_datagrams_are_rejected() {
        assert_eq!(AudioPacket::parse(&rtp(96, 1, 0, &[])), None);
        assert_eq!(AudioPacket::parse(&[0; 4]), None);
        assert_eq!(AudioPacket::parse(&vec![0; MAX_AUDIO_DATAGRAM + 1]), None);
    }

    #[test]
    fn an_audio_stream_setup_is_read_from_its_plist_entry() {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), AUDIO_STREAM_TYPE.into());
        stream.insert("ct".into(), 8_u64.into());
        stream.insert("spf".into(), 480_u64.into());
        stream.insert("ch".into(), 2_u64.into());
        stream.insert("sr".into(), 44100_u64.into());
        stream.insert("controlPort".into(), 6001_u64.into());

        let request = AudioStreamRequest::parse(&stream);
        assert_eq!(request.compression, Some(8));
        assert_eq!(request.samples_per_frame, Some(480));
        assert_eq!(request.channels, Some(2));
        assert_eq!(request.sample_rate, Some(44100));
        assert_eq!(request.remote_control_port, Some(6001));
    }

    #[test]
    fn the_response_names_both_ports() {
        let value = setup_response(7010, 7011);
        let stream = value.as_dictionary().unwrap();
        assert_eq!(
            stream["type"].as_unsigned_integer(),
            Some(AUDIO_STREAM_TYPE)
        );
        assert_eq!(stream["dataPort"].as_unsigned_integer(), Some(7010));
        assert_eq!(stream["controlPort"].as_unsigned_integer(), Some(7011));
    }
}
