//! H.264 decoding for the viewer's side of a [`TileCodec::LossyVideo`] stream.
//!
//! The mirror of [`crate::video_encoder`]. Where every other codec in this crate
//! is a pure function — bytes in, pixels out — a video decoder is inherently
//! **stateful**, and that difference is the whole reason this type exists rather
//! than another arm in `decode_tile`:
//!
//! - **A packet is not a picture.** Feeding one packet may yield zero frames
//!   (the decoder is still accumulating) or several. Callers must handle "no
//!   output yet" as the ordinary case, not as an error.
//! - **Frames reference earlier frames.** That is exactly what makes the codec
//!   cheap — a still window costs almost nothing because each frame says "same
//!   as before" — but it means the decoder must persist across packets, holding
//!   its reference frames.
//! - **A stream must be entered at a keyframe.** Packets before the first
//!   keyframe reference pictures we never saw, so they decode to garbage.
//!   [`Self::decode`] drops them until the first keyframe arrives instead of
//!   showing a corrupt image.
//!
//! Ordering and delivery are *not* a concern here: the session runs over TCP
//! with unbounded, continuously drained queues, so packets arrive intact and in
//! order. `Priority::Video` only affects which class is *scheduled* first, never
//! whether a message survives.

use crate::video::{contains_keyframe, DecodedFrame};
use crate::WindowError;

/// Decodes one window's H.264 stream back into BGRA frames.
pub struct VideoDecoder {
    decoder: ffmpeg_next::decoder::Video,
    /// BGRA conversion, built lazily: the decoder only reports the true frame
    /// geometry once it has decoded something, and the scaler needs it.
    scaler: Option<ffmpeg_next::software::scaling::Context>,
    /// Reused destination frame.
    bgra: ffmpeg_next::frame::Video,
    /// Whether a keyframe has been seen. Until it has, packets are dropped —
    /// they reference pictures this decoder never received.
    synced: bool,
    /// Frames dropped while waiting to sync, for one honest log line.
    dropped_before_sync: u32,
    /// The newest picture [`Self::feed`] produced, not yet converted.
    newest: Option<ffmpeg_next::frame::Video>,
}

impl VideoDecoder {
    /// Open a decoder for an incoming stream.
    ///
    /// Unlike the encoder, no size is needed up front: H.264 carries its own
    /// geometry, and taking it from the bitstream avoids trusting a
    /// [`WindowMeta`] that may disagree with what was actually encoded.
    pub fn open() -> Result<Self, WindowError> {
        let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::H264)
            .ok_or_else(|| WindowError::Backend("no H.264 decoder in FFmpeg".into()))?;
        let ctx = ffmpeg_next::codec::context::Context::new_with_codec(codec);
        let decoder = ctx
            .decoder()
            .video()
            .map_err(|e| WindowError::Backend(format!("open H.264 decoder: {e}")))?;

        Ok(Self {
            decoder,
            scaler: None,
            bgra: ffmpeg_next::frame::Video::empty(),
            synced: false,
            dropped_before_sync: 0,
            newest: None,
        })
    }

    /// Whether a keyframe has arrived. Until one does every packet is
    /// dropped, and the viewer should ask the host for one.
    pub fn synced(&self) -> bool {
        self.synced
    }

    /// Feed one packet; return whatever complete BGRA frames it produced.
    ///
    /// Each returned frame is a full window image, tightly packed
    /// (`width * height * 4`), ready to blit. An empty result is normal.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<DecodedFrame>, WindowError> {
        if !self.send(packet)? {
            return Ok(Vec::new());
        }
        self.drain()
    }

    /// Feed one packet, keeping only the newest picture it yields, without
    /// converting it; [`Self::take_newest`] converts that one. A viewer that
    /// fell behind decodes every packet — later pictures are built on them —
    /// but converts and shows only the last, where [`Self::decode`] paid a
    /// colour conversion for each picture nobody would see.
    pub fn feed(&mut self, packet: &[u8]) -> Result<(), WindowError> {
        if !self.send(packet)? {
            return Ok(());
        }
        loop {
            let mut frame = ffmpeg_next::frame::Video::empty();
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => self.newest = Some(frame),
                Err(ffmpeg_next::Error::Other {
                    errno: ffmpeg_next::error::EAGAIN,
                })
                | Err(ffmpeg_next::Error::Eof) => return Ok(()),
                Err(e) => return Err(WindowError::Backend(format!("receive_frame: {e}"))),
            }
        }
    }

    /// The newest picture fed since the last call, as BGRA.
    pub fn take_newest(&mut self) -> Result<Option<DecodedFrame>, WindowError> {
        match self.newest.take() {
            Some(frame) => self.convert(&frame).map(Some),
            None => Ok(None),
        }
    }

    /// Hand a packet to the decoder once the stream is synced; false for one
    /// dropped while waiting for a keyframe.
    fn send(&mut self, packet: &[u8]) -> Result<bool, WindowError> {
        if packet.is_empty() {
            return Ok(false);
        }

        // Entering mid-stream: everything before the first keyframe references
        // pictures we never saw, so it would decode to visible garbage.
        if !self.synced {
            if !contains_keyframe(packet) {
                self.dropped_before_sync += 1;
                return Ok(false);
            }
            if self.dropped_before_sync > 0 {
                tracing::info!(
                    dropped = self.dropped_before_sync,
                    "video stream synced on keyframe"
                );
            }
            self.synced = true;
        }

        let pkt = ffmpeg_next::Packet::copy(packet);
        self.decoder
            .send_packet(&pkt)
            .map_err(|e| WindowError::Backend(format!("send_packet: {e}")))?;
        Ok(true)
    }

    /// Collect frames the decoder has ready.
    fn drain(&mut self) -> Result<Vec<DecodedFrame>, WindowError> {
        let mut out = Vec::new();
        let mut frame = ffmpeg_next::frame::Video::empty();

        loop {
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => out.push(self.convert(&frame)?),
                // No more frames from this packet — the ordinary exit.
                Err(ffmpeg_next::Error::Other {
                    errno: ffmpeg_next::error::EAGAIN,
                }) => break,
                Err(ffmpeg_next::Error::Eof) => break,
                Err(e) => return Err(WindowError::Backend(format!("receive_frame: {e}"))),
            }
        }
        Ok(out)
    }
}

impl VideoDecoder {
    /// One decoded picture as tightly packed BGRA.
    fn convert(&mut self, frame: &ffmpeg_next::frame::Video) -> Result<DecodedFrame, WindowError> {
        let (w, h) = (frame.width(), frame.height());
        // Build (or rebuild) the converter once the real geometry is known —
        // it can only be trusted after a successful decode.
        let needs_scaler = match &self.scaler {
            Some(_) => self.bgra.width() != w || self.bgra.height() != h,
            None => true,
        };
        if needs_scaler {
            self.scaler = Some(
                ffmpeg_next::software::scaling::Context::get(
                    frame.format(),
                    w,
                    h,
                    ffmpeg_next::format::Pixel::BGRA,
                    w,
                    h,
                    ffmpeg_next::software::scaling::Flags::BILINEAR,
                )
                .map_err(|e| WindowError::Backend(format!("YUV→BGRA: {e}")))?,
            );
            self.bgra = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::BGRA, w, h);
        }
        self.scaler
            .as_mut()
            .expect("scaler built above")
            .run(frame, &mut self.bgra)
            .map_err(|e| WindowError::Backend(format!("scale: {e}")))?;
        Ok(DecodedFrame {
            width: w,
            height: h,
            // Repack tightly: ffmpeg aligns rows to `stride`, which may exceed
            // `width * 4`, but every consumer here expects no padding.
            pixels: repack(&self.bgra, w, h),
        })
    }
}

/// Copy a frame's rows out without the alignment padding ffmpeg adds.
fn repack(frame: &ffmpeg_next::frame::Video, width: u32, height: u32) -> Vec<u8> {
    let row_bytes = (width as usize) * 4;
    let stride = frame.stride(0);
    let data = frame.data(0);
    let mut out = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let start = y * stride;
        out.extend_from_slice(&data[start..start + row_bytes]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_encoder::{Fidelity, VideoEncoder};

    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; (w as usize) * (h as usize) * 4]
    }

    // Keyframe detection itself is tested in `crate::video`, where it lives.

    #[test]
    fn packets_before_a_keyframe_are_dropped_not_decoded() {
        ffmpeg_next::init().expect("ffmpeg init");
        let mut dec = VideoDecoder::open().expect("decoder");
        // A non-IDR packet arriving first (joined mid-stream): showing this
        // would paint garbage, so it must yield nothing at all.
        let frames = dec.decode(&[0, 0, 1, 0x41, 0x9A, 0x00]).expect("decode");
        assert!(frames.is_empty());
        assert!(!dec.synced);
    }

    /// The real test: encode frames, decode them back, and check the pixels
    /// survive — end to end, through both halves of the codec.
    #[test]
    fn round_trips_through_encoder_and_decoder() {
        ffmpeg_next::init().expect("ffmpeg init");
        let (w, h) = (320u32, 240u32);
        let mut enc = VideoEncoder::open(w, h, Fidelity::Lossless).expect("encoder");
        let mut dec = VideoDecoder::open().expect("decoder");

        let mut decoded = Vec::new();
        for i in 0..8 {
            for packet in enc.encode(&solid(w, h, 0x20 + i * 10)).expect("encode") {
                decoded.extend(dec.decode(&packet).expect("decode"));
            }
        }
        for packet in enc.finish().expect("flush") {
            decoded.extend(dec.decode(&packet).expect("decode"));
        }

        assert!(!decoded.is_empty(), "nothing survived the round trip");
        let first = &decoded[0];
        assert_eq!((first.width, first.height), (w, h));
        assert_eq!(
            first.pixels.len(),
            (w as usize) * (h as usize) * 4,
            "decoded frame must be tightly packed BGRA"
        );
    }

    /// Lossless in, lossless out — for **colour**, which is what the codec
    /// choice was made for.
    ///
    /// Alpha is deliberately excluded: YUV420P has no alpha plane, so it cannot
    /// survive the round trip and always returns as opaque. That costs nothing
    /// here — the portal captures an opaque window, and the surrogate paints it
    /// opaque — but a future translucent-window feature would need a codec with
    /// an alpha channel, so the loss is asserted rather than glossed over.
    #[test]
    fn lossless_round_trip_preserves_colour_exactly() {
        ffmpeg_next::init().expect("ffmpeg init");
        // 160x128: NVENC refuses frames below its minimum dimensions, and a
        // too-small test would silently only ever exercise the x264 fallback.
        let (w, h) = (160u32, 128u32);
        let mut enc = VideoEncoder::open(w, h, Fidelity::Lossless).expect("encoder");
        let mut dec = VideoDecoder::open().expect("decoder");

        // Opaque mid-grey: no chroma detail for 4:2:0 subsampling to lose, so
        // any colour difference here would be genuine codec loss.
        let source = [0x80u8, 0x80, 0x80, 0xFF].repeat((w as usize) * (h as usize));

        let mut decoded = Vec::new();
        for packet in enc.encode(&source).expect("encode") {
            decoded.extend(dec.decode(&packet).expect("decode"));
        }
        for packet in enc.finish().expect("flush") {
            decoded.extend(dec.decode(&packet).expect("decode"));
        }

        let frame = decoded.first().expect("a frame must come back");
        let mismatched = frame
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(source.as_chunks::<4>().0)
            // Compare B, G, R only — index 3 is the alpha we knowingly drop.
            .filter(|(got, want)| got[..3] != want[..3])
            .count();
        assert_eq!(
            mismatched, 0,
            "lossless codec changed the colour of {mismatched} pixels"
        );
    }

    /// A lossy NVENC stream is fed BGRA and converted on the GPU, not by
    /// swscale; its matrix and range must still be the BT.601 limited range
    /// every viewer decodes with. A BT.709 matrix would shift this orange by
    /// about eleven luma levels, a full range would crush its darks.
    #[test]
    fn a_lossy_stream_keeps_its_colour_whichever_side_converts_it() {
        ffmpeg_next::init().expect("ffmpeg init");
        let (w, h) = (320u32, 240u32);
        let mut enc =
            VideoEncoder::open(w, h, Fidelity::Lossy { bitrate: 8_000_000 }).expect("encoder");
        let mut dec = VideoDecoder::open().expect("decoder");
        let source = [0x50u8, 0x3C, 0xC8, 0xFF].repeat((w as usize) * (h as usize));

        let mut decoded = Vec::new();
        for _ in 0..4 {
            for packet in enc.encode(&source).expect("encode") {
                decoded.extend(dec.decode(&packet).expect("decode"));
            }
        }
        for packet in enc.finish().expect("flush") {
            decoded.extend(dec.decode(&packet).expect("decode"));
        }

        let frame = decoded.last().expect("a frame must come back");
        let centre = ((h as usize / 2) * w as usize + w as usize / 2) * 4;
        let got = &frame.pixels[centre..centre + 3];
        for (c, (&got, &want)) in got.iter().zip(&source[..3]).enumerate() {
            let delta = (got as i32 - want as i32).abs();
            assert!(
                delta <= 4,
                "{:?} encoder moved channel {c} by {delta}",
                enc.kind()
            );
        }
    }
}
