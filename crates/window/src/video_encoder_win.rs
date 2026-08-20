//! H.264 encoding on Windows, via a Media Foundation transform.
//!
//! The Windows counterpart to [`crate::video_encoder`], and the reason a
//! Windows host stops paying the lossless-tile price on a moving window (the
//! 742 Mbit/s case in `docs/window-transfer-design.md`). It presents the same
//! surface as the Linux encoder — [`open`](VideoEncoder::open),
//! [`encode`](VideoEncoder::encode), [`size`](VideoEncoder::size),
//! [`full_rect`](VideoEncoder::full_rect) — so the routing in the capture
//! backend reads identically on both platforms.
//!
//! ## This one is not lossless, and that is a real difference
//!
//! The Linux encoder is *pixel-exact*: x264 and NVENC both take `qp=0` /
//! `tune=lossless` through FFmpeg, and the design leans on that to keep text
//! sharp without a per-tile codec split.
//!
//! Media Foundation exposes no dependable lossless mode. `ICodecAPI` has a
//! quality knob, but its meaning is vendor-defined, hardware encoders clamp it,
//! and none of them promise exactness. So this encoder runs at **maximum
//! quality**, not lossless, and the consequence is honest: on a Windows host,
//! large updates are visually near-perfect but not bit-exact.
//!
//! Why that is acceptable here rather than a blocker: the codec routing only
//! sends a region to video once it is changing across a large fraction of the
//! window ([`should_use_video`](crate::frame_pipeline::should_use_video)) —
//! i.e. video and scrolling, where the design already accepted lossy as the
//! right trade. Static text and a blinking caret stay on the lossless zstd tile
//! path, which is byte-exact on every platform. Quantisation artefacts land on
//! motion, where they are least visible, and never on an idle window.
//!
//! ## Colour
//!
//! The transform consumes NV12, so BGRA must be converted on the way in — the
//! mirror of what [`crate::video::nv12`] does on the way out.

use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::video::nv12;
use crate::WindowError;
use cursio_protocol::Rect;

/// The one input / one output stream an H.264 encoder MFT exposes.
const STREAM_ID: u32 = 0;

/// Frame rate the encoder is configured for. Matches the Linux encoder: it only
/// affects timing metadata and the keyframe interval, not capture pacing.
const FRAME_RATE: u32 = 60;

/// Seconds between forced keyframes. A viewer joining mid-stream cannot show
/// anything until one arrives, so this bounds that wait.
const KEYFRAME_INTERVAL_SECS: u32 = 2;

/// Encodes one window's BGRA frames into an H.264 stream.
pub struct VideoEncoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    /// Reused NV12 scratch, so a steady stream stops allocating.
    nv12: Vec<u8>,
    /// Frame index, used to derive presentation timestamps. Must increase
    /// monotonically or the transform rejects the sample.
    frame_index: i64,
}

impl VideoEncoder {
    /// Open an encoder for a window of this size.
    ///
    /// The size is fixed for the encoder's life: H.264 cannot change frame size
    /// mid-stream, so a window resize means a new encoder (and a fresh
    /// keyframe). Same contract as the Linux encoder, which the capture backend
    /// relies on when it compares [`size`](Self::size).
    pub fn open(width: u32, height: u32) -> Result<Self, WindowError> {
        // 4:2:0 chroma is subsampled by two on both axes, so odd dimensions
        // have no valid chroma plane. Reject rather than silently skew.
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(WindowError::Backend(format!(
                "video encoder needs even dimensions, got {width}x{height}"
            )));
        }
        if width == 0 || height == 0 {
            return Err(WindowError::Backend("zero-sized window".into()));
        }

        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| WindowError::Backend(format!("MFStartup: {e}")))?;

            let transform = find_h264_encoder()?;
            // Output first: an encoder needs to know what it is producing
            // before it will accept a description of the input. (The decoder
            // is the other way round — the asymmetry is real, not a typo.)
            set_output_type(&transform, width, height)?;
            set_input_type(&transform, width, height)?;
            configure_quality(&transform);

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| WindowError::Backend(format!("begin streaming: {e}")))?;

            tracing::info!(width, height, "Media Foundation H.264 encoder opened");

            Ok(Self {
                transform,
                width,
                height,
                nv12: Vec::new(),
                frame_index: 0,
            })
        }
    }

    /// The geometry this encoder is fixed to.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The whole-window rect a video frame covers.
    ///
    /// Video packets always describe the full window, never a dirty sub-rect:
    /// the codec's own inter-frame prediction is what makes an unchanged region
    /// cost nothing, so cropping would only break its references.
    pub fn full_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }

    /// Encode one full-window BGRA frame; return whatever packets it produced.
    ///
    /// Zero packets is ordinary — the encoder may hold a frame back. Several is
    /// also ordinary.
    pub fn encode(&mut self, bgra: &[u8]) -> Result<Vec<Vec<u8>>, WindowError> {
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if bgra.len() != expected {
            return Err(WindowError::Backend(format!(
                "frame is {} bytes, encoder expects {expected}",
                bgra.len()
            )));
        }

        nv12::from_bgra(bgra, self.width, self.height, &mut self.nv12);

        unsafe {
            let sample = self.make_sample()?;
            self.transform
                .ProcessInput(STREAM_ID, &sample, 0)
                .map_err(|e| WindowError::Backend(format!("ProcessInput: {e}")))?;
        }
        self.frame_index += 1;

        self.drain()
    }

    /// Flush the encoder at end of stream, returning any packets still held.
    pub fn finish(&mut self) -> Result<Vec<Vec<u8>>, WindowError> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .map_err(|e| WindowError::Backend(format!("drain: {e}")))?;
        }
        self.drain()
    }

    /// Wrap the current NV12 scratch in the `IMFSample` the transform consumes.
    unsafe fn make_sample(&self) -> Result<IMFSample, WindowError> {
        let buffer = MFCreateMemoryBuffer(self.nv12.len() as u32)
            .map_err(|e| WindowError::Backend(format!("MFCreateMemoryBuffer: {e}")))?;

        let mut dst = std::ptr::null_mut();
        buffer
            .Lock(&mut dst, None, None)
            .map_err(|e| WindowError::Backend(format!("lock input buffer: {e}")))?;
        std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), dst, self.nv12.len());
        let _ = buffer.Unlock();
        buffer
            .SetCurrentLength(self.nv12.len() as u32)
            .map_err(|e| WindowError::Backend(format!("SetCurrentLength: {e}")))?;

        let sample =
            MFCreateSample().map_err(|e| WindowError::Backend(format!("MFCreateSample: {e}")))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| WindowError::Backend(format!("AddBuffer: {e}")))?;

        // MF timestamps are in 100ns units. Unlike the decoder, the encoder
        // does need these: rate control and GOP placement are timing-driven.
        let ticks = 10_000_000i64 / FRAME_RATE as i64;
        let _ = sample.SetSampleTime(self.frame_index * ticks);
        let _ = sample.SetSampleDuration(ticks);
        Ok(sample)
    }

    /// Collect packets the transform has ready.
    fn drain(&mut self) -> Result<Vec<Vec<u8>>, WindowError> {
        let mut out = Vec::new();

        // Unlike the decoder, an encoder MFT may want us to supply the output
        // buffer. Ask once which model this one uses.
        let provides_samples = unsafe {
            self.transform
                .GetOutputStreamInfo(STREAM_ID)
                .map(|info| {
                    let allocates = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0;
                    info.dwFlags as i32 & allocates != 0
                })
                .unwrap_or(false)
        };

        loop {
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: STREAM_ID,
                ..Default::default()
            }];

            // When the transform does not allocate, we must hand it a buffer
            // large enough for one compressed frame.
            if !provides_samples {
                let size = unsafe {
                    self.transform
                        .GetOutputStreamInfo(STREAM_ID)
                        .map(|i| i.cbSize)
                        .unwrap_or(0)
                }
                .max(self.width * self.height); // a sane floor if it reports 0
                buffers[0].pSample =
                    std::mem::ManuallyDrop::new(Some(unsafe { alloc_sample(size)? }));
            }

            let mut status = 0u32;
            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };

            match result {
                Ok(()) => {
                    // `pSample` is `ManuallyDrop`, so taking the value out is
                    // what releases the COM reference — leaving it in place
                    // would leak one interface pointer per encoded frame.
                    let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) };
                    if let Some(sample) = sample {
                        out.push(unsafe { read_sample(&sample)? });
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder renegotiated its output; re-set and retry.
                    unsafe { set_output_type(&self.transform, self.width, self.height)? };
                    continue;
                }
                Err(e) => return Err(WindowError::Backend(format!("ProcessOutput: {e}"))),
            }
        }

        Ok(out)
    }
}

/// Allocate an `IMFSample` backed by one empty buffer of `size` bytes.
unsafe fn alloc_sample(size: u32) -> Result<IMFSample, WindowError> {
    let buffer = MFCreateMemoryBuffer(size)
        .map_err(|e| WindowError::Backend(format!("MFCreateMemoryBuffer: {e}")))?;
    let sample =
        MFCreateSample().map_err(|e| WindowError::Backend(format!("MFCreateSample: {e}")))?;
    sample
        .AddBuffer(&buffer)
        .map_err(|e| WindowError::Backend(format!("AddBuffer: {e}")))?;
    Ok(sample)
}

/// Copy one encoded sample's bytes out.
unsafe fn read_sample(sample: &IMFSample) -> Result<Vec<u8>, WindowError> {
    let buffer = sample
        .ConvertToContiguousBuffer()
        .map_err(|e| WindowError::Backend(format!("contiguous buffer: {e}")))?;

    let mut ptr = std::ptr::null_mut();
    let mut len = 0u32;
    buffer
        .Lock(&mut ptr, None, Some(&mut len))
        .map_err(|e| WindowError::Backend(format!("lock output: {e}")))?;
    let data = std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec();
    let _ = buffer.Unlock();
    Ok(data)
}

/// Find an H.264 encoder MFT, preferring hardware.
///
/// `SORTANDFILTER` puts hardware transforms first, so taking the first that
/// activates gets GPU encoding where it exists without naming a vendor, and
/// falls back to Microsoft's software H.264 encoder where it doesn't.
unsafe fn find_h264_encoder() -> Result<IMFTransform, WindowError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_SYNCMFT
            | MFT_ENUM_FLAG_LOCALMFT
            | MFT_ENUM_FLAG_HARDWARE
            | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &mut activates,
        &mut count,
    )
    .map_err(|e| WindowError::Backend(format!("MFTEnumEx: {e}")))?;

    if activates.is_null() || count == 0 {
        return Err(WindowError::Backend(
            "no H.264 encoder MFT on this machine".into(),
        ));
    }

    let list = std::slice::from_raw_parts(activates, count as usize);
    let mut chosen = None;
    for activate in list.iter().flatten() {
        if chosen.is_none() {
            match activate.ActivateObject::<IMFTransform>() {
                Ok(t) => {
                    unlock_if_async(&t);
                    chosen = Some(t);
                }
                Err(e) => {
                    tracing::debug!(error = %e, "an H.264 encoder MFT failed to activate; trying next")
                }
            }
        }
    }
    windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));

    chosen.ok_or_else(|| WindowError::Backend("no H.264 encoder MFT could be activated".into()))
}

/// Let a hardware encoder be driven synchronously.
///
/// Most GPU H.264 MFTs are **async** transforms. Until
/// `MF_TRANSFORM_ASYNC_UNLOCK` is set on them they reject the synchronous
/// ProcessInput/ProcessOutput calls this encoder uses, with
/// `0xC00D6D77` ("the caller may not support async") — which is exactly what
/// happened here, so every frame silently fell back to lossless tiles.
///
/// Best-effort: a software MFT has no async attribute and needs no unlocking.
unsafe fn unlock_if_async(transform: &IMFTransform) {
    let Ok(attrs) = transform.GetAttributes() else {
        return; // no attribute store: a plain synchronous MFT
    };
    if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1 {
        match attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) {
            Ok(()) => tracing::info!("async H.264 MFT unlocked for synchronous use"),
            Err(e) => tracing::warn!(error = %e, "could not unlock the async H.264 MFT"),
        }
    }
}

/// Describe the H.264 stream the encoder should produce.
unsafe fn set_output_type(
    transform: &IMFTransform,
    width: u32,
    height: u32,
) -> Result<(), WindowError> {
    let ty = MFCreateMediaType().map_err(|e| WindowError::Backend(format!("media type: {e}")))?;
    ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .and_then(|_| ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
        .and_then(|_| ty.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(width, height)))
        .and_then(|_| ty.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(FRAME_RATE, 1)))
        .and_then(|_| ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1)))
        .and_then(|_| ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))
        // Every H.264 encoder MFT requires a bitrate on its output type, even
        // in a quality-driven rate-control mode where it is only a ceiling.
        // Sized generously: the design measured 138 Mbit/s worst case, and a
        // low cap here would quietly cost quality on exactly the motion the
        // video path exists to carry.
        .and_then(|_| ty.SetUINT32(&MF_MT_AVG_BITRATE, 200_000_000))
        .map_err(|e| WindowError::Backend(format!("describe H.264 output: {e}")))?;

    transform
        .SetOutputType(STREAM_ID, &ty, 0)
        .map_err(|e| WindowError::Backend(format!("SetOutputType: {e}")))
}

/// Describe the NV12 frames we will feed in.
unsafe fn set_input_type(
    transform: &IMFTransform,
    width: u32,
    height: u32,
) -> Result<(), WindowError> {
    let ty = MFCreateMediaType().map_err(|e| WindowError::Backend(format!("media type: {e}")))?;
    ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .and_then(|_| ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
        .and_then(|_| ty.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(width, height)))
        .and_then(|_| ty.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(FRAME_RATE, 1)))
        .and_then(|_| ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1)))
        .and_then(|_| ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))
        .map_err(|e| WindowError::Backend(format!("describe NV12 input: {e}")))?;

    transform
        .SetInputType(STREAM_ID, &ty, 0)
        .map_err(|e| WindowError::Backend(format!("SetInputType: {e}")))
}

/// Ask for maximum quality and low latency, best-effort.
///
/// Every one of these is optional: `ICodecAPI` is not implemented by all
/// transforms, and a given encoder may reject any individual property. None of
/// them is required for correct output, so a failure is logged at debug and
/// ignored rather than failing the open — a working encoder at default
/// settings beats no encoder at all.
unsafe fn configure_quality(transform: &IMFTransform) {
    let Ok(codec_api) = transform.cast::<ICodecAPI>() else {
        tracing::debug!("encoder exposes no ICodecAPI; using its defaults");
        return;
    };

    let set = |name: *const windows::core::GUID, value: u32| {
        let var = windows::Win32::System::Variant::VARIANT::from(value as i32);
        if codec_api.SetValue(name, &var).is_err() {
            tracing::debug!("encoder rejected a quality property; leaving it at default");
        }
    };

    // Quality-driven rather than bitrate-driven: a window's cost should follow
    // its content, not be forced up to fill a budget on an idle window.
    set(
        &CODECAPI_AVEncCommonRateControlMode,
        eAVEncCommonRateControlMode_Quality.0 as u32,
    );
    // 0..100, higher is better. Not lossless — see the module docs.
    set(&CODECAPI_AVEncCommonQuality, 100);
    // Bound how long a mid-stream viewer waits for its first showable frame.
    set(
        &CODECAPI_AVEncMPVGOPSize,
        FRAME_RATE * KEYFRAME_INTERVAL_SECS,
    );

    // Latency, not compression, is what this stream is judged on.
    //
    // A window transfer is meant to feel like a second monitor, so a frame that
    // arrives late is worse than a frame that costs more bytes. Two encoder
    // defaults trade exactly the wrong way for that:
    //
    // * **B-frames** are predicted from a *later* picture, so the encoder must
    //   hold a frame back until the one after it exists. That reordering delay
    //   is paid on every frame, and it is why every low-latency preset in the
    //   field (x264's `zerolatency` among them) sets B-frames to zero.
    // * **Lookahead / quality buffering** is what `LowLatencyMode` turns off;
    //   without it the encoder may sit on output waiting for more input to
    //   decide with.
    //
    // Both are advisory — `set` logs and continues when an encoder declines,
    // which the software fallback does for some of these.
    set(&CODECAPI_AVEncCommonLowLatency, 1);
    set(&CODECAPI_AVEncMPVDefaultBPictureCount, 0);
}

/// Media Foundation packs paired 32-bit values (size, frame rate, aspect
/// ratio) into one `u64`: first in the high half, second in the low.
fn pack_ratio(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | (b as u64)
}
