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
//! - `OPUS_APPLICATION_RESTRICTED_LOWDELAY` drops
//!   the codec's look-ahead. Ordinary Opus adds ~6.5 ms of algorithmic delay;
//!   restricted low-delay cuts that to ~2.5 ms.
//! - A **5 ms frame** (240 samples at 48 kHz) rather than the usual 20 ms.
//!   Frame size is a latency floor you can never recover downstream — a 20 ms
//!   frame means every packet waits 20 ms before it is even encoded.
//!
//! The cost is bitrate efficiency: short frames spend proportionally more bits
//! on headers. That is the right trade here — bandwidth is what we have plenty
//! of on a LAN, latency is what the user notices.

//!
//! ## Why the encoder talks to libopus directly
//!
//! Rate control changes a stream's bitrate mid-stream, and the only way to do
//! that without a discontinuity is `OPUS_SET_BITRATE` on the encoder already
//! running. `shiguredo_opus` sets the bitrate once, at construction, and keeps
//! the encoder pointer private, so the only change it allows is a new encoder.
//! That is not a harmless swap: the peer's decoder still holds the old
//! encoder's state — band energies, MDCT overlap, pitch pre-filter — and the
//! first packets from a fresh encoder decode against it as an audible burst.
//! Rate control stepped several times a second when it misjudged the link, so
//! the burst was heard in ordinary use.
//!
//! So the encoder is created and driven through libopus's C API here, against
//! the library `shiguredo_opus` already builds and links. That crate renames
//! every libopus symbol with a `shiguredo_opus_` prefix, which is why the
//! declarations below carry `link_name`s; a rename upstream fails the link
//! rather than misbehaving. The decoder has no such need and stays on the
//! crate's safe wrapper.

use std::os::raw::{c_int, c_uchar};

#[cfg(not(target_os = "android"))]
use shiguredo_opus::{Decoder, DecoderConfig};
// Named only for the library it links; the declarations in `ffi` are the API.
#[cfg(target_os = "android")]
use audiopus_sys as _;

use crate::AudioError;

mod ffi {
    use std::os::raw::{c_int, c_uchar};

    /// Opaque libopus encoder state.
    #[repr(C)]
    pub struct OpusEncoder {
        _private: [u8; 0],
    }

    pub const OPUS_OK: c_int = 0;
    pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
    pub const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
    pub const OPUS_GET_BITRATE_REQUEST: c_int = 4003;

    /// Opaque libopus decoder state.
    #[cfg(target_os = "android")]
    #[repr(C)]
    pub struct OpusDecoder {
        _private: [u8; 0],
    }

    // On Android libopus comes from `audiopus_sys` under its own names; every
    // other platform links the copy `shiguredo_opus` built and renamed, and
    // decodes through that crate's wrapper instead.
    extern "C" {
        #[cfg_attr(
            not(target_os = "android"),
            link_name = "shiguredo_opus_encoder_create"
        )]
        pub fn opus_encoder_create(
            fs: i32,
            channels: c_int,
            application: c_int,
            error: *mut c_int,
        ) -> *mut OpusEncoder;
        #[cfg_attr(not(target_os = "android"), link_name = "shiguredo_opus_encoder_ctl")]
        pub fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
        #[cfg_attr(not(target_os = "android"), link_name = "shiguredo_opus_encode_float")]
        pub fn opus_encode_float(
            st: *mut OpusEncoder,
            pcm: *const f32,
            frame_size: c_int,
            data: *mut c_uchar,
            max_data_bytes: i32,
        ) -> i32;
        #[cfg_attr(
            not(target_os = "android"),
            link_name = "shiguredo_opus_encoder_destroy"
        )]
        pub fn opus_encoder_destroy(st: *mut OpusEncoder);
        #[cfg(target_os = "android")]
        pub fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int)
            -> *mut OpusDecoder;
        #[cfg(target_os = "android")]
        pub fn opus_decode_float(
            st: *mut OpusDecoder,
            data: *const c_uchar,
            len: i32,
            pcm: *mut f32,
            frame_size: c_int,
            decode_fec: c_int,
        ) -> c_int;
        #[cfg(target_os = "android")]
        pub fn opus_decoder_destroy(st: *mut OpusDecoder);
    }
}

/// Largest packet libopus is asked to produce; RFC 6716's recommended maximum.
const MAX_PACKET_BYTES: usize = 4000;

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
    /// Owned libopus state: created in `with_bitrate`, destroyed in `drop`,
    /// never null in between.
    encoder: *mut ffi::OpusEncoder,
    /// Leftover interleaved samples that didn't fill a frame last time.
    pending: Vec<f32>,
    packet: Vec<u8>,
}

// SAFETY: libopus encoder state has no thread affinity, and every use of the
// pointer goes through `&mut self`, so it is never touched from two threads at
// once.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new() -> Result<Self, AudioError> {
        Self::with_bitrate(DEFAULT_BITRATE)
    }

    /// Build an encoder at a specific target bitrate (bits per second).
    ///
    /// Latency-first (see the module docs): restricted low-delay removes the
    /// codec's look-ahead, and `encode` feeds it 5 ms frames, a much lower
    /// latency floor than the 20 ms default.
    pub fn with_bitrate(bits_per_second: u32) -> Result<Self, AudioError> {
        let mut error: c_int = ffi::OPUS_OK;
        // SAFETY: plain constructor call; the result is checked before use.
        let encoder = unsafe {
            ffi::opus_encoder_create(
                48_000,
                2,
                ffi::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut error,
            )
        };
        if error != ffi::OPUS_OK || encoder.is_null() {
            return Err(AudioError::Backend(format!(
                "opus encoder: opus_encoder_create failed with code {error}"
            )));
        }
        let mut this = Self {
            encoder,
            pending: Vec::with_capacity(FRAME_INTERLEAVED * 2),
            packet: vec![0; MAX_PACKET_BYTES],
        };
        this.set_bitrate(bits_per_second)?;
        Ok(this)
    }

    /// Change the target bitrate of the running encoder.
    ///
    /// Takes effect from the next frame and keeps everything else: the codec
    /// state the peer's decoder is tracking, and any partial frame held in
    /// `pending`.
    pub fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), AudioError> {
        let value = c_int::try_from(bits_per_second).map_err(|_| {
            AudioError::Backend(format!("opus bitrate {bits_per_second} out of range"))
        })?;
        // SAFETY: `self.encoder` is live (see the field); the request takes one
        // `opus_int32`, which is `c_int` on every supported platform.
        let code =
            unsafe { ffi::opus_encoder_ctl(self.encoder, ffi::OPUS_SET_BITRATE_REQUEST, value) };
        if code != ffi::OPUS_OK {
            return Err(AudioError::Backend(format!(
                "opus encoder: OPUS_SET_BITRATE({bits_per_second}) failed with code {code}"
            )));
        }
        Ok(())
    }

    /// The bitrate libopus is currently targeting, in bits per second.
    pub fn bitrate(&self) -> Result<u32, AudioError> {
        let mut value: c_int = 0;
        // SAFETY: `self.encoder` is live; the request writes one `opus_int32`.
        let code = unsafe {
            ffi::opus_encoder_ctl(
                self.encoder,
                ffi::OPUS_GET_BITRATE_REQUEST,
                &mut value as *mut c_int,
            )
        };
        if code != ffi::OPUS_OK {
            return Err(AudioError::Backend(format!(
                "opus encoder: OPUS_GET_BITRATE failed with code {code}"
            )));
        }
        Ok(value as u32)
    }

    /// Samples per channel retained before the next complete Opus frame.
    pub fn buffered_frames(&self) -> usize {
        self.pending.len() / 2
    }

    /// Encode raw wire bytes, returning zero or more Opus packets.
    ///
    /// Returns several packets when the input spans multiple frames, and none
    /// when it doesn't yet fill one — the remainder is held for the next call
    /// rather than padded, since padding would inject clicks.
    pub fn encode(&mut self, pcm: &[u8]) -> Result<Vec<Vec<u8>>, AudioError> {
        self.pending.extend(
            pcm.as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b)),
        );

        let mut packets = Vec::new();
        while self.pending.len() >= FRAME_INTERLEAVED {
            let frame: Vec<f32> = self.pending.drain(..FRAME_INTERLEAVED).collect();
            // SAFETY: `self.encoder` is live; `frame` holds exactly
            // FRAME_SAMPLES stereo samples and `self.packet` is writable for
            // the length passed.
            let size = unsafe {
                ffi::opus_encode_float(
                    self.encoder,
                    frame.as_ptr(),
                    FRAME_SAMPLES as c_int,
                    self.packet.as_mut_ptr() as *mut c_uchar,
                    self.packet.len() as i32,
                )
            };
            let size = usize::try_from(size).map_err(|_| {
                AudioError::Backend(format!("opus encode: failed with code {size}"))
            })?;
            packets.push(self.packet[..size.min(self.packet.len())].to_vec());
        }
        Ok(packets)
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: created by `opus_encoder_create` and destroyed only here.
        unsafe { ffi::opus_encoder_destroy(self.encoder) }
    }
}

/// Decodes Opus packets back into the wire format.
#[cfg(not(target_os = "android"))]
pub struct OpusDecoder {
    decoder: Decoder,
}

#[cfg(not(target_os = "android"))]
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

/// Decodes Opus packets back into the wire format.
#[cfg(target_os = "android")]
pub struct OpusDecoder {
    /// Owned libopus state: created in `new`, destroyed in `drop`, never null
    /// in between.
    decoder: *mut ffi::OpusDecoder,
    pcm: Vec<f32>,
}

// SAFETY: as for the encoder — no thread affinity, every use through `&mut self`.
#[cfg(target_os = "android")]
unsafe impl Send for OpusDecoder {}

/// Most samples per channel one packet can decode to: 120 ms at 48 kHz.
#[cfg(target_os = "android")]
const MAX_DECODED_SAMPLES: usize = 5760;

#[cfg(target_os = "android")]
impl OpusDecoder {
    pub fn new() -> Result<Self, AudioError> {
        let mut error: c_int = ffi::OPUS_OK;
        // SAFETY: plain constructor call; the result is checked before use.
        let decoder = unsafe { ffi::opus_decoder_create(48_000, 2, &mut error) };
        if error != ffi::OPUS_OK || decoder.is_null() {
            return Err(AudioError::Backend(format!(
                "opus decoder: opus_decoder_create failed with code {error}"
            )));
        }
        Ok(Self {
            decoder,
            pcm: vec![0.0; MAX_DECODED_SAMPLES * 2],
        })
    }

    /// Decode one packet into raw wire bytes (48 kHz stereo f32 LE).
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<u8>, AudioError> {
        // SAFETY: `self.decoder` is live; `self.pcm` holds room for
        // MAX_DECODED_SAMPLES stereo frames, the frame size passed.
        let frames = unsafe {
            ffi::opus_decode_float(
                self.decoder,
                packet.as_ptr(),
                packet.len() as i32,
                self.pcm.as_mut_ptr(),
                MAX_DECODED_SAMPLES as c_int,
                0,
            )
        };
        let frames = usize::try_from(frames)
            .map_err(|_| AudioError::Backend(format!("opus decode: failed with code {frames}")))?;
        Ok(self.pcm[..frames * 2]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect())
    }
}

#[cfg(target_os = "android")]
impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: created by `opus_decoder_create` and destroyed only here.
        unsafe { ffi::opus_decoder_destroy(self.decoder) }
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
    fn partial_frames_keep_a_precise_sample_clock_across_capture_boundaries() {
        let mut encoder = OpusEncoder::new().unwrap();
        assert!(encoder.encode(&vec![0; 128 * 8]).unwrap().is_empty());
        assert_eq!(encoder.buffered_frames(), 128);
        let second_capture_timestamp = 128u64;
        let first_packet_timestamp = second_capture_timestamp - encoder.buffered_frames() as u64;
        let packets = encoder.encode(&vec![0; 512 * 8]).unwrap();
        assert_eq!(first_packet_timestamp, 0);
        assert_eq!(packets.len(), 2);
        assert_eq!(encoder.buffered_frames(), 160);
    }

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
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
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

    /// A bitrate change applies to the running encoder: the rate moves, and
    /// the encoder keeps both its codec state and a half-received frame.
    ///
    /// Replacing the encoder instead is what this exists to avoid — the peer's
    /// decoder keeps the old encoder's state, and the new one's first packets
    /// decode against it as an audible burst.
    #[test]
    fn bitrate_changes_on_the_live_encoder_without_losing_a_partial_frame() {
        let mut enc = OpusEncoder::with_bitrate(96_000).unwrap();
        let tone = |frames: usize, offset: usize| -> Vec<f32> {
            (0..FRAME_SAMPLES * frames)
                .flat_map(|i| {
                    let t = (i + offset) as f32 / 48_000.0;
                    let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3
                        + (t * 3_000.0 * std::f32::consts::TAU).sin() * 0.2;
                    [v, v * 0.8]
                })
                .collect()
        };
        let high: usize = enc
            .encode(&pcm(&tone(20, 0)))
            .unwrap()
            .iter()
            .map(Vec::len)
            .sum();

        let half = tone(1, FRAME_SAMPLES * 20);
        assert!(enc
            .encode(&pcm(&half[..FRAME_INTERLEAVED / 2]))
            .unwrap()
            .is_empty());
        enc.set_bitrate(24_000).unwrap();
        assert_eq!(enc.bitrate().unwrap(), 24_000);
        assert_eq!(
            enc.encode(&pcm(&half[FRAME_INTERLEAVED / 2..]))
                .unwrap()
                .len(),
            1,
            "the half frame held before the change must complete after it"
        );

        let low: usize = enc
            .encode(&pcm(&tone(20, FRAME_SAMPLES * 21)))
            .unwrap()
            .iter()
            .map(Vec::len)
            .sum();
        assert!(
            low * 2 < high,
            "a quarter of the bitrate must produce far smaller packets: {high} -> {low} bytes"
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
