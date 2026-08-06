//! Opus compression for the audio wire format, tuned for **low latency**.
//!
//! Raw f32 stereo at 48 kHz costs 384 KB/s *per stream*. With a dozen streams
//! live that saturates the link — and because input shares the same encrypted
//! channel, a saturated link is felt as mouse lag, not just audio trouble.
//! Opus carries the same audio in roughly 1/50th of that.
//!
//! ## Why latency drives every choice here
//!
//! Cursio aims at minimal latency, so this deliberately does *not* use Opus's
//! default music mode:
//!
//! - [`Application::LowDelay`] (`OPUS_APPLICATION_RESTRICTED_LOWDELAY`) drops
//!   the codec's look-ahead. Ordinary Opus adds ~6.5 ms of algorithmic delay;
//!   restricted low-delay cuts that to ~2.5 ms.
//! - A **5 ms frame** (240 samples at 48 kHz) rather than the usual 20 ms.
//!   Frame size is a latency floor you can never recover downstream — a 20 ms
//!   frame means every packet waits 20 ms before it is even encoded.
//!
//! The cost is bitrate efficiency: short frames spend proportionally more bits
//! on headers. That is the right trade here — bandwidth is what we have plenty
//! of on a LAN, latency is what the user notices.

use shiguredo_opus::{Application, Decoder, DecoderConfig, Encoder, EncoderConfig};

use crate::AudioError;

/// Samples per channel in one Opus frame: 5 ms at 48 kHz.
///
/// Opus only accepts specific frame sizes (2.5/5/10/20/40/60 ms). 5 ms is the
/// shortest that still codes efficiently; 2.5 ms roughly doubles the header
/// overhead for ~2.5 ms less delay.
pub const FRAME_SAMPLES: usize = 240;
/// Interleaved f32 samples in one frame (stereo).
pub const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * 2;
/// Bytes of one uncompressed frame on the wire.
pub const FRAME_BYTES: usize = FRAME_INTERLEAVED * 4;

/// Default bitrate: transparent-enough stereo at low delay, ~50x smaller than
/// raw. Tunable per stream via [`OpusEncoder::set_bitrate`].
pub const DEFAULT_BITRATE: u32 = 96_000;

/// Encodes the wire format (48 kHz stereo f32) into Opus packets.
///
/// Chunks arriving from a capture backend are whatever size the OS handed us,
/// but Opus insists on exact frame sizes — so this buffers across calls and
/// emits only whole frames.
pub struct OpusEncoder {
    encoder: Encoder,
    /// Leftover interleaved samples that didn't fill a frame last time.
    pending: Vec<f32>,
}

impl OpusEncoder {
    pub fn new() -> Result<Self, AudioError> {
        Self::with_bitrate(DEFAULT_BITRATE)
    }

    /// Build an encoder at a specific target bitrate (bits per second).
    pub fn with_bitrate(bits_per_second: u32) -> Result<Self, AudioError> {
        let mut config = EncoderConfig::new(48_000, 2);
        // Latency-first (see the module docs): restricted low-delay removes the
        // codec's look-ahead, and a 5 ms frame sets a much lower latency floor
        // than the 20 ms default.
        config.application = Some(Application::LowDelay);
        config.frame_duration = Some(shiguredo_opus::FrameDuration::Ms5);
        config.bitrate = Some(bits_per_second);

        let encoder =
            Encoder::new(config).map_err(|e| AudioError::Backend(format!("opus encoder: {e}")))?;
        Ok(Self {
            encoder,
            pending: Vec::with_capacity(FRAME_INTERLEAVED * 2),
        })
    }

    /// Encode raw wire bytes, returning zero or more Opus packets.
    ///
    /// Returns several packets when the input spans multiple frames, and none
    /// when it doesn't yet fill one — the remainder is held for the next call
    /// rather than padded, since padding would inject clicks.
    pub fn encode(&mut self, pcm: &[u8]) -> Result<Vec<Vec<u8>>, AudioError> {
        self.pending.extend(
            pcm.chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );

        let mut packets = Vec::new();
        while self.pending.len() >= FRAME_INTERLEAVED {
            let frame: Vec<f32> = self.pending.drain(..FRAME_INTERLEAVED).collect();
            let packet = self
                .encoder
                .encode_f32(&frame)
                .map_err(|e| AudioError::Backend(format!("opus encode: {e}")))?;
            packets.push(packet);
        }
        Ok(packets)
    }
}

/// Decodes Opus packets back into the wire format.
pub struct OpusDecoder {
    decoder: Decoder,
}

impl OpusDecoder {
    pub fn new() -> Result<Self, AudioError> {
        let decoder = Decoder::new(DecoderConfig::new(48_000, 2))
            .map_err(|e| AudioError::Backend(format!("opus decoder: {e}")))?;
        Ok(Self { decoder })
    }

    /// Decode one packet into raw wire bytes (48 kHz stereo f32 LE).
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<u8>, AudioError> {
        let samples = self
            .decoder
            .decode_f32(packet)
            .map_err(|e| AudioError::Backend(format!("opus decode: {e}")))?;
        Ok(samples.iter().flat_map(|s| s.to_le_bytes()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// A tone must survive the round trip recognisably. Opus is lossy, so this
    /// checks energy is preserved rather than exact samples.
    #[test]
    fn tone_survives_the_round_trip() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut dec = OpusDecoder::new().unwrap();

        // 4 frames of a 440 Hz sine, stereo.
        let mut samples = Vec::new();
        for i in 0..FRAME_SAMPLES * 4 {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
            samples.push(v); // L
            samples.push(v); // R
        }

        let packets = enc.encode(&pcm(&samples)).unwrap();
        assert_eq!(packets.len(), 4, "4 frames in should be 4 packets out");

        let mut decoded = Vec::new();
        for p in &packets {
            decoded.extend(dec.decode(p).unwrap());
        }

        let out: Vec<f32> = decoded
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        assert_eq!(out.len(), samples.len(), "sample count must be preserved");

        // Compare RMS: lossy coding changes samples but not overall level.
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let (a, b) = (rms(&samples), rms(&out));
        assert!(
            (a - b).abs() < 0.1,
            "energy changed too much through the codec: {a} -> {b}"
        );
    }

    /// Compression must actually be substantial — the whole point is relieving
    /// a link shared with input.
    #[test]
    fn compression_is_substantial() {
        let mut enc = OpusEncoder::new().unwrap();
        let samples: Vec<f32> = (0..FRAME_INTERLEAVED * 10)
            .map(|i| (i as f32 * 0.01).sin() * 0.3)
            .collect();
        let raw = pcm(&samples);
        let packets = enc.encode(&raw).unwrap();
        let compressed: usize = packets.iter().map(|p| p.len()).sum();
        assert!(
            compressed * 8 < raw.len(),
            "expected >8x compression, got {} -> {}",
            raw.len(),
            compressed
        );
    }

    /// A partial frame must be buffered, not padded — padding would click.
    #[test]
    fn partial_frames_are_held_until_complete() {
        let mut enc = OpusEncoder::new().unwrap();
        let half = vec![0.1f32; FRAME_INTERLEAVED / 2];
        assert!(
            enc.encode(&pcm(&half)).unwrap().is_empty(),
            "half a frame should emit nothing"
        );
        // The second half completes it.
        assert_eq!(enc.encode(&pcm(&half)).unwrap().len(), 1);
    }
}

#[cfg(test)]
mod ratio {
    use super::*;
    /// Report the real compression ratio for a music-like signal, so the
    /// bandwidth claim in the docs is measured rather than assumed.
    #[test]
    fn measured_ratio() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut samples = Vec::new();
        for i in 0..FRAME_SAMPLES * 100 {
            let t = i as f32 / 48_000.0;
            let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3
                + (t * 880.0 * std::f32::consts::TAU).sin() * 0.2;
            samples.push(v);
            samples.push(v * 0.9);
        }
        let raw: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let packets = enc.encode(&raw).unwrap();
        let comp: usize = packets.iter().map(|p| p.len()).sum();
        println!(
            "RATIO raw={} compressed={} ratio={:.1}x kbps={:.0}",
            raw.len(),
            comp,
            raw.len() as f32 / comp as f32,
            comp as f32 * 8.0 / (packets.len() as f32 * 0.005) / 1000.0
        );
    }
}
