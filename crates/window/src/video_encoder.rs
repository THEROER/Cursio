//! Video encoding for [`TileCodec::LossyVideo`] — used **losslessly**.
//!
//! ## What the measurements said (2026-07-20, GTX 1650, 1080p60)
//!
//! The first live `capture_probe` run failed the traffic budget: zstd on a
//! moving window cost ~1510 KiB/frame, **742 Mbit/s**, past gigabit Ethernet.
//! zstd only reaches ~2.9x on motion because a moving image dirties nearly
//! every pixel, so the dirty rect degenerates to the whole window.
//!
//! Benchmarking the alternatives changed the design:
//!
//! | Path | Realistic window (code + caret) | Worst case (full motion) | Text |
//! |---|---|---|---|
//! | zstd on dirty rects | — | 742 Mbit/s | exact |
//! | **H.264 lossless** | **2.6 Mbit/s** | **61 Mbit/s** | **exact** |
//! | H.264 lossy (`ull`, 12M) | 0.3 Mbit/s | 15 Mbit/s | artefacts |
//!
//! So we encode H.264 **losslessly**. That is the whole trick: inter-frame
//! prediction — not quantisation — is what collapses the bitrate, because
//! consecutive window frames are nearly identical. We get zstd's pixel-exact
//! fidelity at 12–280x less traffic, and 61 Mbit/s leaves 16x headroom on a
//! gigabit LAN even in the pathological case.
//!
//! This deliberately drops the "lossy for motion, lossless for text" split the
//! design originally proposed. That split existed to keep text sharp; a
//! lossless stream keeps *everything* sharp, with no per-tile codec decision, no
//! artefacts on UI, and less machinery. Quality and simplicity pointed the same
//! way, so the tile-codec field stays in the protocol (the wire format is
//! unchanged) but the host has one answer for pixels in motion.
//!
//! ## Why a stream, not a tile
//!
//! Elsewhere a [`WindowFrame`] carries an independent tile that can be decoded
//! alone. An inter-frame codec cannot: each packet references earlier ones, and
//! the encoder is fixed to one frame size. So `LossyVideo` frames are a
//! *stream* — encoder opened at the window's size, every packet's `dirty`
//! covering the whole window, decoded in order. Dirty-rect cropping remains
//! what it always was: the lever for **lossless BGRA tiles**, still the right
//! answer for a small update on an otherwise idle window, where spinning up a
//! video stream would be silly.
//!
//! ## Portability
//!
//! The peer's machine is not this machine, so encoder choice is by what the
//! local FFmpeg can actually **open**, not merely what it lists: this box has
//! `h264_vaapi` compiled in, yet it fails to initialise because the only render
//! node is NVIDIA. Order is NVENC → VAAPI → libx264. Software is a genuine
//! fallback, not a token one: x264 `ultrafast -qp 0` encoded the realistic
//! workload in 0.50 s per 3 s of video — comfortably real-time.
//!
//! ## Only for a viewer that can decode it
//!
//! Lossless H.264 exists only in the High 4:4:4 Predictive profile — NVENC
//! and x264 both switch to it for `qp=0`, even from 4:2:0 input. FFmpeg decodes
//! it; Media Foundation does not, and says nothing: measured on the Windows
//! peer (2026-09-27, `h264_decode_probe`), 30 lossless packets went in and no
//! picture came out, while the same content as a High-profile stream decoded
//! all 30. So the lossless stream is used only for a viewer that announced it
//! decodes it ([`Fidelity`]), and every other viewer gets a lossy High-profile
//! stream at a bitrate.

use crate::WindowError;

/// Which encoder backend a [`VideoEncoder`] ended up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    /// NVIDIA NVENC (`h264_nvenc`).
    Nvenc,
    /// VAAPI (`h264_vaapi`) — Intel/AMD.
    Vaapi,
    /// libx264 on the CPU. Always present; fast enough to be a real fallback.
    Software,
}

impl EncoderKind {
    /// The FFmpeg encoder to open.
    fn codec_name(self) -> &'static str {
        match self {
            EncoderKind::Nvenc => "h264_nvenc",
            EncoderKind::Vaapi => "h264_vaapi",
            EncoderKind::Software => "libx264",
        }
    }

    /// Preference order. Software must be **last**: it always opens, so trying
    /// it earlier would mask every hardware path.
    fn candidates() -> [EncoderKind; 3] {
        [
            EncoderKind::Nvenc,
            EncoderKind::Vaapi,
            EncoderKind::Software,
        ]
    }

    /// The options for `fidelity`, always low-latency.
    fn options(self, fidelity: Fidelity) -> Vec<(&'static str, &'static str)> {
        let mut options = match fidelity {
            Fidelity::Lossless => self.lossless_options(),
            Fidelity::Lossy { .. } => self.lossy_options(),
        };
        if self != EncoderKind::Vaapi {
            // A frame marked `I` is otherwise only an intra frame to NVENC and
            // x264, which a decoder that missed the stream cannot start from.
            options.push(("forced-idr", "1"));
        }
        if self == EncoderKind::Nvenc {
            // FFmpeg's NVENC wrapper holds `delay` frames before releasing a
            // packet, defaulting to one less than its surface pool (three), and
            // `zerolatency` does not touch it. Capture only encodes when the
            // screen changed, so a held frame waits for the next change: the
            // opening keyframe of a view sat in the encoder for five seconds
            // on a still desktop, and the last frame of every burst of motion
            // never left at all — a closed menu stayed on the viewer's screen.
            options.push(("delay", "0"));
        }
        options
    }

    /// Low-latency, rate-controlled, and in a profile every decoder takes.
    /// The bitrate itself is set on the codec context, not here.
    fn lossy_options(self) -> Vec<(&'static str, &'static str)> {
        match self {
            EncoderKind::Nvenc => vec![
                ("preset", "p4"),
                ("tune", "ll"),
                ("profile", "high"),
                ("rc", "vbr"),
                ("zerolatency", "1"),
            ],
            EncoderKind::Vaapi => vec![("rc_mode", "VBR"), ("profile", "high")],
            EncoderKind::Software => vec![
                ("preset", "superfast"),
                ("tune", "zerolatency"),
                ("profile", "high"),
            ],
        }
    }

    /// The options that put this encoder into lossless, low-latency mode.
    ///
    /// Lossless is expressed differently per backend — NVENC has a dedicated
    /// preset/tune pair, x264 uses `qp=0`, VAAPI needs `qp=0` on an otherwise
    /// constant-quality setup.
    fn lossless_options(self) -> Vec<(&'static str, &'static str)> {
        match self {
            EncoderKind::Nvenc => vec![
                // `preset=lossless` is deprecated in favour of p1–p7 + `tune`.
                // p4 is the balance point measured on this hardware: on a
                // realistic window it matches the old preset exactly
                // (2.62 Mbit/s), on worst-case motion it sits mid-pack
                // (60.9 vs p1's 60.2 and p7's 51.3), and it encodes 3 s of
                // 1080p60 in 1.2 s — real-time with headroom. p7 compresses
                // motion ~16% better but costs 40% more encode time; not worth
                // trading latency for bytes we already have room for.
                ("preset", "p4"),
                ("tune", "lossless"),
                // Emit each frame as soon as it is encoded; no lookahead delay.
                ("zerolatency", "1"),
            ],
            // NOTE: `h264_vaapi` does not accept a software pixel format — it
            // wants `AV_PIX_FMT_VAAPI` surfaces, which means a hardware frames
            // context and uploading each frame to the GPU. That plumbing isn't
            // written, so on an Intel/AMD box this currently fails to open and
            // falls through to libx264 (which is fast enough to be a real
            // fallback: 0.50 s per 3 s of 1080p). Left in place, and honest
            // about it, so the ordering is ready when the upload path lands.
            EncoderKind::Vaapi => vec![("rc_mode", "CQP"), ("qp", "0")],
            EncoderKind::Software => vec![
                ("preset", "ultrafast"),
                ("qp", "0"),
                ("tune", "zerolatency"),
            ],
        }
    }
}

/// The encoder's own GOP, in frames: effectively never.
///
/// Keyframes are placed by the capture loop's
/// [`KeyframeClock`](crate::frame_pipeline::KeyframeClock) through
/// [`VideoEncoder::force_keyframe`], because a GOP counts frames and frames
/// arrive only when the screen changes. `1 << 30` is x264's own "infinite",
/// and NVENC takes it as a plain length.
const GOP_FRAMES: u32 = 1 << 30;

/// Frame rate the encoder is configured for. Capture is driven by the
/// compositor, which caps at the display's rate; 60 matches the common case and
/// only affects timing metadata.
const FRAME_RATE: i32 = 60;

/// What kind of stream to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    /// Pixel-exact, High 4:4:4 Predictive — only for a viewer that decodes it.
    Lossless,
    /// High profile at about `bitrate` bits per second.
    Lossy { bitrate: u32 },
}

impl Fidelity {
    /// Whether an encoder opened for `self` can serve `other` without being
    /// reopened. The bitrate is not part of it: a lossy encoder changes rate
    /// in place.
    pub fn same_stream(self, other: Fidelity) -> bool {
        matches!(
            (self, other),
            (Fidelity::Lossless, Fidelity::Lossless)
                | (Fidelity::Lossy { .. }, Fidelity::Lossy { .. })
        )
    }
}

/// An H.264 encoder for one window's stream.
pub struct VideoEncoder {
    kind: EncoderKind,
    fidelity: Fidelity,
    width: u32,
    height: u32,
    encoder: ffmpeg_next::encoder::Video,
    /// The caller's BGRA, copied into a frame kept across calls: a fresh 8 MB
    /// frame per 1080p picture was an allocation on every capture.
    bgra: ffmpeg_next::frame::Video,
    /// BGRA → YUV420P conversion and its reused destination, for encoders
    /// that cannot take BGRA themselves; `None` when the encoder converts on
    /// its own hardware. Kept alive across frames: building a scaler
    /// allocates and picks a code path, which is wasted work per frame.
    convert: Option<(
        ffmpeg_next::software::scaling::Context,
        ffmpeg_next::frame::Video,
    )>,
    /// Presentation timestamp, in the encoder's `1/FRAME_RATE` time base. Must
    /// increase monotonically or the encoder rejects the frame.
    pts: i64,
    /// Whether the next [`encode`](Self::encode) must produce an IDR.
    keyframe: bool,
}

impl VideoEncoder {
    /// Open an encoder for a window of this size.
    ///
    /// Tries hardware first, then software, so this fails only if nothing can
    /// encode at all. The size is fixed for the encoder's life: H.264 cannot
    /// change frame size mid-stream, so a window resize means a new encoder
    /// (and a fresh keyframe).
    pub fn open(width: u32, height: u32, fidelity: Fidelity) -> Result<Self, WindowError> {
        crate::frame_pipeline::check_encoder_size(width, height)?;

        let mut attempts = Vec::new();
        for kind in EncoderKind::candidates() {
            match Self::try_open(kind, width, height, fidelity) {
                Ok(enc) => {
                    tracing::info!(?kind, ?fidelity, width, height, "video encoder opened");
                    return Ok(enc);
                }
                Err(e) => {
                    // Expected on machines lacking that hardware; keep the
                    // reason so a total failure can report all of them.
                    tracing::debug!(?kind, error = %e, "encoder unavailable, trying next");
                    attempts.push(format!("{}: {e}", kind.codec_name()));
                }
            }
        }

        Err(WindowError::Backend(format!(
            "no usable H.264 encoder ({})",
            attempts.join("; ")
        )))
    }

    /// Try one backend. Presence is not availability — the codec must also
    /// *open*, which is what rules out `h264_vaapi` on an NVIDIA-only box.
    fn try_open(
        kind: EncoderKind,
        width: u32,
        height: u32,
        fidelity: Fidelity,
    ) -> Result<Self, WindowError> {
        let name = kind.codec_name();

        // NVENC rejects small frames outright ("Frame Dimension less than the
        // minimum supported value"). Measured on a GTX 1650: 128x96 is refused,
        // 160x128 accepted. Checking here rather than letting the open fail
        // keeps the error log clean for the ordinary case of a small window,
        // which simply belongs on x264.
        if kind == EncoderKind::Nvenc && (width < 160 || height < 128) {
            return Err(WindowError::Backend(format!(
                "{width}x{height} is below NVENC's minimum (160x128)"
            )));
        }
        let codec = ffmpeg_next::encoder::find_by_name(name)
            .ok_or_else(|| WindowError::Backend(format!("{name} not built into FFmpeg")))?;

        let ctx = ffmpeg_next::codec::context::Context::new_with_codec(codec);
        let mut enc = ctx
            .encoder()
            .video()
            .map_err(|e| WindowError::Backend(format!("{name}: not a video encoder: {e}")))?;

        // NVENC takes BGRA and converts on the GPU, to BT.601 limited range —
        // measured, and what both viewers' decoders assume — so the lossy
        // stream skips swscale, which cost 7 ms per 1080p frame on the capture
        // thread, nearly half a 60 fps budget. Lossless stays on YUV420P: fed
        // RGB, NVENC's lossless mode codes a different chroma layout than the
        // stream every viewer is known to decode.
        let takes_bgra = kind == EncoderKind::Nvenc && matches!(fidelity, Fidelity::Lossy { .. });

        enc.set_width(width);
        enc.set_height(height);
        enc.set_format(if takes_bgra {
            ffmpeg_next::format::Pixel::BGRA
        } else {
            ffmpeg_next::format::Pixel::YUV420P
        });
        enc.set_time_base(ffmpeg_next::Rational(1, FRAME_RATE));
        enc.set_frame_rate(Some(ffmpeg_next::Rational(FRAME_RATE, 1)));
        enc.set_gop(GOP_FRAMES);
        // No B-frames: they reorder output, so the encoder must hold a frame
        // back to encode a later one. That is latency we refuse to pay for a
        // live window.
        enc.set_max_b_frames(0);
        if let Fidelity::Lossy { bitrate } = fidelity {
            enc.set_bit_rate(bitrate as usize);
            enc.set_max_bit_rate(bitrate as usize * 2);
        }

        let mut opts = ffmpeg_next::Dictionary::new();
        for (k, v) in kind.options(fidelity) {
            opts.set(k, v);
        }

        // Opening is the real test of whether this hardware works here.
        let encoder = enc
            .open_with(opts)
            .map_err(|e| WindowError::Backend(format!("{name}: open failed: {e}")))?;

        // Otherwise swscale does the conversion — `Bilinear` is irrelevant here
        // (no rescaling happens, only a colour-space change) but the parameter
        // is required.
        let convert = if takes_bgra {
            None
        } else {
            let scaler = ffmpeg_next::software::scaling::Context::get(
                ffmpeg_next::format::Pixel::BGRA,
                width,
                height,
                ffmpeg_next::format::Pixel::YUV420P,
                width,
                height,
                ffmpeg_next::software::scaling::Flags::BILINEAR,
            )
            .map_err(|e| WindowError::Backend(format!("{name}: scaler: {e}")))?;
            let yuv =
                ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, width, height);
            Some((scaler, yuv))
        };

        Ok(Self {
            kind,
            fidelity,
            width,
            height,
            encoder,
            bgra: ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::BGRA, width, height),
            convert,
            pts: 0,
            keyframe: false,
        })
    }

    /// Make the next encoded frame an IDR, which a decoder can start from.
    pub fn force_keyframe(&mut self) {
        self.keyframe = true;
    }

    /// Which backend this encoder is using.
    pub fn kind(&self) -> EncoderKind {
        self.kind
    }

    /// The kind of stream this encoder produces.
    pub fn fidelity(&self) -> Fidelity {
        self.fidelity
    }

    /// Move a lossy stream to `bitrate` without reopening it.
    ///
    /// Reopening would mean a keyframe — the most expensive frame there is,
    /// sent exactly when the link has just said it is short — and a visible
    /// restart. x264 re-reads the context's rate on the next frame and
    /// reconfigures in place, no keyframe. NVENC through FFmpeg re-keys
    /// anyway: its reconfigure sets `resetEncoder` and `forceIDR` for any rate
    /// change (`nvenc.c`, FFmpeg 7.1; measured here as an IDR on the very next
    /// frame). So NVENC moves only for a change of a quarter or more, which
    /// lets a step down through and makes two small steps up cost one
    /// keyframe instead of two. A lossless stream has no rate to move.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        let Fidelity::Lossy { bitrate: current } = self.fidelity else {
            return;
        };
        if current == bitrate {
            return;
        }
        if self.kind == EncoderKind::Nvenc && current.abs_diff(bitrate) < current / 4 {
            return;
        }
        // SAFETY: the context is owned by `self.encoder` and alive; FFmpeg's
        // encoders read `bit_rate`/`rc_max_rate` from it on each frame.
        unsafe {
            let ctx = self.encoder.as_mut_ptr();
            (*ctx).bit_rate = bitrate as i64;
            (*ctx).rc_max_rate = bitrate as i64 * 2;
        }
        self.fidelity = Fidelity::Lossy { bitrate };
    }

    /// The fixed frame size this encoder was opened for.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Encode one full-window BGRA frame, returning the packets it produced.
    ///
    /// `bgra` must be tightly packed (`width * height * 4`, no row padding) —
    /// what the capture backend already repacks PipeWire's output into.
    ///
    /// Usually one packet comes back, but the contract is a `Vec`: an encoder
    /// may hold a frame briefly or emit several, and callers must not assume
    /// one-in-one-out. An empty result is normal, not an error.
    pub fn encode(&mut self, bgra: &[u8]) -> Result<Vec<Vec<u8>>, WindowError> {
        crate::frame_pipeline::check_frame_len(bgra, self.width, self.height)?;

        // FFmpeg aligns each row to `stride()`, which for BGRA is usually but
        // *not always* `width * 4`, so copy row by row rather than in one
        // memcpy — the same padding trap as PipeWire's `chunk.stride`.
        let row_bytes = (self.width as usize) * 4;
        let stride = self.bgra.stride(0);
        let data = self.bgra.data_mut(0);
        for y in 0..self.height as usize {
            let dst = y * stride;
            let s = y * row_bytes;
            data[dst..dst + row_bytes].copy_from_slice(&bgra[s..s + row_bytes]);
        }

        let frame = match self.convert.as_mut() {
            Some((scaler, yuv)) => {
                scaler
                    .run(&self.bgra, yuv)
                    .map_err(|e| WindowError::Backend(format!("BGRA→YUV420P: {e}")))?;
                yuv
            }
            None => &mut self.bgra,
        };

        // PTS must advance monotonically; the encoder rejects a repeat.
        frame.set_pts(Some(self.pts));
        self.pts += 1;
        // The frame is reused, so the type must be cleared again after a
        // forced keyframe or every later frame would be one too.
        frame.set_kind(if std::mem::take(&mut self.keyframe) {
            ffmpeg_next::picture::Type::I
        } else {
            ffmpeg_next::picture::Type::None
        });

        self.encoder
            .send_frame(frame)
            .map_err(|e| WindowError::Backend(format!("send_frame: {e}")))?;

        self.drain()
    }

    /// Collect whatever packets the encoder has ready.
    ///
    /// `EAGAIN` means "nothing ready yet", which is an ordinary outcome rather
    /// than a failure — hence draining until it appears instead of treating any
    /// error as fatal.
    fn drain(&mut self) -> Result<Vec<Vec<u8>>, WindowError> {
        let mut out = Vec::new();
        let mut packet = ffmpeg_next::Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    if let Some(data) = packet.data() {
                        out.push(data.to_vec());
                    }
                }
                // Nothing more this round.
                Err(ffmpeg_next::Error::Other {
                    errno: ffmpeg_next::error::EAGAIN,
                }) => break,
                Err(ffmpeg_next::Error::Eof) => break,
                Err(e) => return Err(WindowError::Backend(format!("receive_packet: {e}"))),
            }
        }
        Ok(out)
    }

    /// Flush the encoder at end of stream, returning any trailing packets.
    ///
    /// Without this, frames still buffered inside the encoder are lost — which
    /// for a lossless stream means the viewer's last image is stale.
    pub fn finish(&mut self) -> Result<Vec<Vec<u8>>, WindowError> {
        self.encoder
            .send_eof()
            .map_err(|e| WindowError::Backend(format!("send_eof: {e}")))?;
        self.drain()
    }
}

impl crate::frame_pipeline::StreamEncoder for VideoEncoder {
    fn encode(&mut self, bgra: &[u8]) -> Result<Vec<Vec<u8>>, WindowError> {
        VideoEncoder::encode(self, bgra)
    }

    fn force_keyframe(&mut self) {
        VideoEncoder::force_keyframe(self)
    }

    fn lossy(&self) -> bool {
        matches!(self.fidelity, Fidelity::Lossy { .. })
    }
}

/// Opens FFmpeg encoders for the capture pipeline, following what the viewer
/// decodes and what the link carries.
#[derive(Default)]
pub struct FfmpegEncoders;

impl crate::frame_pipeline::EncoderSource for FfmpegEncoders {
    type Encoder = VideoEncoder;

    fn prepare(
        &mut self,
        slot: &mut Option<VideoEncoder>,
        width: u32,
        height: u32,
        quality: &crate::frame_pipeline::AdaptiveQuality,
    ) -> Option<(u32, u32)> {
        let fidelity = if quality.lossless() {
            Fidelity::Lossless
        } else {
            Fidelity::Lossy {
                bitrate: quality.bitrate(width, height),
            }
        };
        // Geometry changes invalidate the encoder: H.264 is fixed-size.
        if slot.as_ref().is_some_and(|e| e.size() != (width, height)) {
            tracing::info!(width, height, "window resized; reopening video encoder");
            *slot = None;
        }
        if slot
            .as_ref()
            .is_some_and(|e| !e.fidelity().same_stream(fidelity))
        {
            tracing::info!(?fidelity, "the viewer's decoder changed what to encode");
            *slot = None;
        }
        if slot.is_none() {
            match VideoEncoder::open(width, height, fidelity) {
                Ok(enc) => *slot = Some(enc),
                Err(e) => {
                    // Not fatal: the lossless path always works, just more
                    // expensively.
                    tracing::warn!(error = %e, "no video encoder; using lossless tiles");
                    return None;
                }
            }
        }
        let enc = slot.as_mut()?;
        if let Fidelity::Lossy { bitrate } = fidelity {
            enc.set_bitrate(bitrate);
        }
        Some((width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_or_zero_dimensions_are_rejected() {
        assert!(matches!(
            VideoEncoder::open(1921, 1080, Fidelity::Lossless),
            Err(WindowError::Backend(_))
        ));
        assert!(matches!(
            VideoEncoder::open(1920, 1081, Fidelity::Lossless),
            Err(WindowError::Backend(_))
        ));
        assert!(matches!(
            VideoEncoder::open(0, 0, Fidelity::Lossless),
            Err(WindowError::Backend(_))
        ));
    }

    #[test]
    fn software_is_tried_last() {
        let c = EncoderKind::candidates();
        assert_eq!(c[0], EncoderKind::Nvenc);
        assert_eq!(c[1], EncoderKind::Vaapi);
        assert_eq!(c[2], EncoderKind::Software);
    }

    #[test]
    fn every_backend_has_a_lossless_recipe() {
        // A backend with no lossless options would silently encode lossily,
        // which is the one outcome this module exists to prevent.
        for kind in EncoderKind::candidates() {
            let opts = kind.lossless_options();
            assert!(!opts.is_empty(), "{kind:?} has no lossless options");
            let lossless = opts
                .iter()
                .any(|(k, v)| (*k == "qp" && *v == "0") || *v == "lossless");
            assert!(
                lossless,
                "{kind:?} options don't express lossless: {opts:?}"
            );
        }
    }

    /// `profile_idc` from the first SPS in `packet`: 100 is High, 244 is
    /// High 4:4:4 Predictive.
    fn sps_profile(packet: &[u8]) -> Option<u8> {
        packet
            .windows(5)
            .find(|w| w[..3] == [0, 0, 1] && w[3] & 0x1F == 7)
            .map(|w| w[4])
    }

    #[test]
    fn a_lossy_stream_is_high_profile_and_a_lossless_one_is_not() {
        ffmpeg_next::init().expect("ffmpeg init");
        let frame = solid_bgra(320, 240, 0x40);
        let first_packet = |fidelity| {
            let mut enc = VideoEncoder::open(320, 240, fidelity).expect("encoder");
            let mut packets = enc.encode(&frame).expect("encode");
            packets.extend(enc.finish().expect("flush"));
            packets.concat()
        };

        let lossy = first_packet(Fidelity::Lossy { bitrate: 4_000_000 });
        assert_eq!(
            sps_profile(&lossy),
            Some(100),
            "a lossy stream must stay in the High profile every decoder takes"
        );
        let lossless = first_packet(Fidelity::Lossless);
        assert_eq!(sps_profile(&lossless), Some(244));
    }

    #[test]
    fn changing_the_bitrate_keeps_the_stream_going_without_a_keyframe() {
        ffmpeg_next::init().expect("ffmpeg init");
        // x264 specifically: NVENC re-keys on any rate change (see
        // `set_bitrate`), which is a property of FFmpeg's wrapper, not ours.
        let mut enc = VideoEncoder::try_open(
            EncoderKind::Software,
            320,
            240,
            Fidelity::Lossy { bitrate: 8_000_000 },
        )
        .expect("encoder");
        // A small moving block on a still background: a whole-frame colour
        // change would be a scene cut, which earns a keyframe on its own.
        let frame = |i: usize| {
            let mut f = solid_bgra(320, 240, 0x40);
            for y in 100..116 {
                let start = (y * 320 + i * 8) * 4;
                f[start..start + 64].fill(0xF0);
            }
            f
        };
        for i in 0..5 {
            enc.encode(&frame(i)).expect("encode");
        }
        enc.set_bitrate(2_000_000);
        assert_eq!(enc.fidelity(), Fidelity::Lossy { bitrate: 2_000_000 });
        let mut after = Vec::new();
        for i in 5..15 {
            after.extend(enc.encode(&frame(i)).expect("encode"));
        }
        assert!(
            !after.is_empty(),
            "the stream stopped after the rate change"
        );
        assert!(
            !after.iter().any(|p| crate::video::contains_keyframe(p)),
            "a rate change must not cost a keyframe"
        );
    }

    /// A small block moving across a still background, frame `i`.
    fn moving_block(i: usize) -> Vec<u8> {
        let mut f = solid_bgra(320, 240, 0x40);
        for y in 100..116 {
            let start = (y * 320 + (i % 30) * 8) * 4;
            f[start..start + 64].fill(0xF0);
        }
        f
    }

    fn encoders_here() -> Vec<VideoEncoder> {
        let lossy = Fidelity::Lossy { bitrate: 4_000_000 };
        let mut encoders =
            vec![VideoEncoder::try_open(EncoderKind::Software, 320, 240, lossy).expect("x264")];
        // NVENC where this machine has one: its keyframe path is FFmpeg's
        // wrapper, not x264's, and it is the encoder real hosts use.
        if let Ok(nvenc) = VideoEncoder::try_open(EncoderKind::Nvenc, 320, 240, lossy) {
            encoders.push(nvenc);
        }
        encoders
    }

    #[test]
    fn a_forced_keyframe_is_an_idr_on_exactly_that_frame() {
        ffmpeg_next::init().expect("ffmpeg init");
        for mut enc in encoders_here() {
            for i in 0..5 {
                enc.encode(&moving_block(i)).expect("encode");
            }
            enc.force_keyframe();
            let forced = enc.encode(&moving_block(5)).expect("encode");
            assert!(
                forced.iter().any(|p| crate::video::contains_keyframe(p)),
                "{:?} ignored a forced keyframe",
                enc.kind()
            );
            let next = enc.encode(&moving_block(6)).expect("encode");
            assert!(
                !next.iter().any(|p| crate::video::contains_keyframe(p)),
                "{:?} kept keying after the forced frame",
                enc.kind()
            );
        }
    }

    #[test]
    fn the_encoder_does_not_key_on_its_own_frame_count() {
        ffmpeg_next::init().expect("ffmpeg init");
        for mut enc in encoders_here() {
            let keyed: Vec<usize> = (0..150)
                .filter(|&i| {
                    enc.encode(&moving_block(i))
                        .expect("encode")
                        .iter()
                        .any(|p| crate::video::contains_keyframe(p))
                })
                .collect();
            assert_eq!(
                keyed,
                vec![0],
                "{:?} placed keyframes by frame count; the capture loop's clock places them",
                enc.kind()
            );
        }
    }

    #[test]
    fn a_new_bitrate_is_the_same_stream_but_losslessness_is_not() {
        let lossy = Fidelity::Lossy { bitrate: 1 };
        assert!(lossy.same_stream(Fidelity::Lossy { bitrate: 2 }));
        assert!(!lossy.same_stream(Fidelity::Lossless));
    }

    /// A solid-colour BGRA frame of the given size.
    fn solid_bgra(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; (w as usize) * (h as usize) * 4]
    }

    #[test]
    fn encodes_frames_into_packets() {
        ffmpeg_next::init().expect("ffmpeg init");
        let mut enc = VideoEncoder::open(320, 240, Fidelity::Lossless).expect("encoder");

        let mut total = 0usize;
        for i in 0..10 {
            // Vary the content so this isn't just testing the keyframe path.
            let packets = enc
                .encode(&solid_bgra(320, 240, (i * 20) as u8))
                .expect("encode");
            total += packets.iter().map(|p| p.len()).sum::<usize>();
        }
        total += enc
            .finish()
            .expect("flush")
            .iter()
            .map(|p| p.len())
            .sum::<usize>();

        // The real assertion: bytes came out. An encoder that silently produced
        // nothing would leave the viewer with a blank window.
        assert!(total > 0, "encoder produced no data at all");
    }

    #[test]
    fn every_frame_leaves_the_encoder_before_the_next_goes_in() {
        ffmpeg_next::init().expect("ffmpeg init");
        for fidelity in [Fidelity::Lossless, Fidelity::Lossy { bitrate: 4_000_000 }] {
            let mut enc = VideoEncoder::open(640, 480, fidelity).expect("encoder");
            for i in 0..3u8 {
                let packets = enc
                    .encode(&solid_bgra(640, 480, 0x20 + i * 0x30))
                    .expect("encode");
                assert!(
                    !packets.is_empty(),
                    "{:?} held frame {i} back; on a screen that then stops changing \
                     it never reaches the viewer",
                    enc.kind()
                );
            }
        }
    }

    #[test]
    fn wrong_sized_frame_is_rejected() {
        ffmpeg_next::init().expect("ffmpeg init");
        let mut enc = VideoEncoder::open(320, 240, Fidelity::Lossless).expect("encoder");
        // A short buffer would otherwise read out of bounds during the row copy.
        assert!(matches!(
            enc.encode(&solid_bgra(320, 120, 0)),
            Err(WindowError::Backend(_))
        ));
    }

    #[test]
    fn static_content_costs_far_less_than_raw() {
        ffmpeg_next::init().expect("ffmpeg init");
        let (w, h) = (640u32, 480u32);
        let mut enc = VideoEncoder::open(w, h, Fidelity::Lossless).expect("encoder");

        let frame = solid_bgra(w, h, 0x40);
        let mut total = 0usize;
        for _ in 0..30 {
            total += enc
                .encode(&frame)
                .expect("encode")
                .iter()
                .map(|p| p.len())
                .sum::<usize>();
        }
        total += enc
            .finish()
            .expect("flush")
            .iter()
            .map(|p| p.len())
            .sum::<usize>();

        // 30 unchanging frames vs one raw frame. This is the whole premise of
        // the codec: inter-frame prediction should make repeats nearly free, so
        // 30 of them must cost well under a *single* uncompressed frame.
        let one_raw = (w as usize) * (h as usize) * 4;
        assert!(
            total < one_raw,
            "30 static frames cost {total} B, more than one raw frame ({one_raw} B)"
        );
    }

    /// Software is always available, so opening must succeed on *some* backend;
    /// a failure means the plumbing itself is broken.
    #[test]
    fn opens_on_some_backend() {
        ffmpeg_next::init().expect("ffmpeg init");
        let enc = VideoEncoder::open(640, 480, Fidelity::Lossless).expect("some encoder must open");
        assert_eq!(enc.size(), (640, 480));
    }
}
