//! H.264 decoding on Windows, via a Media Foundation transform.
//!
//! The Windows counterpart to [`crate::video_decoder`]. Same contract — feed
//! packets, get back tightly-packed BGRA [`DecodedFrame`]s — and the same
//! statefulness caveats, which are documented once on the Linux decoder and
//! not repeated here.
//!
//! ## Why Media Foundation and not FFmpeg
//!
//! The Linux decoder is `ffmpeg-next` linked against the system libav\*. There
//! is no system libav\* on Windows, and vendoring an FFmpeg build would drag a
//! C toolchain into a cross-compile that currently needs none. Media Foundation
//! ships *in the OS*: the `.exe` stays self-contained, and enumerating the
//! transform with [`MFTEnumEx`] picks up a hardware decoder when the machine
//! has one, falling back to Microsoft's software H.264 MFT when it doesn't.
//!
//! ## What an MFT makes you handle that FFmpeg hides
//!
//! - **Output format is NV12, not BGRA.** Hardware decoders do not offer BGRA
//!   at all, so the YUV→BGRA conversion is ours to do ([`nv12`]).
//! - **Geometry is not known up front and can change.** The transform reports
//!   `MF_E_TRANSFORM_STREAM_CHANGE` when it has parsed the SPS; only then is
//!   the real frame size available, and the output type must be re-set.
//! - **Rows are stride-aligned.** The stride is a property of the buffer, not
//!   of the width, and it is only truthfully reported through [`IMF2DBuffer`]
//!   — reading a plain lock and assuming `width` bytes per row produces a
//!   sheared image on exactly the hardware where it matters.

use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::video::{contains_keyframe, DecodedFrame};
use crate::WindowError;

/// The one input stream / one output stream an H.264 decoder MFT exposes.
const STREAM_ID: u32 = 0;

/// Decodes one window's H.264 stream back into BGRA frames.
pub struct VideoDecoder {
    transform: IMFTransform,
    /// Frame geometry, known only after the transform has parsed the stream's
    /// SPS and announced a format change. `None` until then.
    size: Option<(u32, u32)>,
    /// Reused NV12 scratch, so a steady stream stops allocating.
    nv12: Vec<u8>,
    /// Whether a keyframe has been seen; see the Linux decoder for why.
    synced: bool,
    /// Frames dropped while waiting to sync, for one honest log line.
    dropped_before_sync: u32,
}

impl VideoDecoder {
    /// Open a decoder for an incoming stream.
    ///
    /// No size is needed up front: H.264 carries its own geometry, and the
    /// transform tells us what it parsed rather than us trusting a
    /// [`WindowMeta`](cursio_protocol::WindowMeta) that may disagree.
    pub fn open() -> Result<Self, WindowError> {
        unsafe {
            // Both are idempotent and refcounted; the surrogate thread may
            // already have done them, and calling again is the documented way
            // to say "this thread needs it too".
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| WindowError::Backend(format!("MFStartup: {e}")))?;

            let transform = find_h264_decoder()?;
            configure(&transform)?;

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| WindowError::Backend(format!("begin streaming: {e}")))?;

            Ok(Self {
                transform,
                size: None,
                nv12: Vec::new(),
                synced: false,
                dropped_before_sync: 0,
            })
        }
    }

    /// Feed one packet; return whatever complete BGRA frames it produced.
    ///
    /// Each returned frame is a full window image, tightly packed
    /// (`width * height * 4`), ready to blit. An empty result is normal.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<DecodedFrame>, WindowError> {
        if packet.is_empty() {
            return Ok(Vec::new());
        }

        // Entering mid-stream: everything before the first keyframe references
        // pictures we never saw, so it would decode to visible garbage.
        if !self.synced {
            if !contains_keyframe(packet) {
                self.dropped_before_sync += 1;
                return Ok(Vec::new());
            }
            if self.dropped_before_sync > 0 {
                tracing::info!(
                    dropped = self.dropped_before_sync,
                    "video stream synced on keyframe"
                );
            }
            self.synced = true;
        }

        unsafe {
            let sample = make_sample(packet)?;
            self.transform
                .ProcessInput(STREAM_ID, &sample, 0)
                .map_err(|e| WindowError::Backend(format!("ProcessInput: {e}")))?;
        }

        self.drain()
    }

    /// Collect frames the transform has ready.
    ///
    /// Three outcomes matter, and only one of them is an error:
    /// `NEED_MORE_INPUT` is the ordinary exit, `STREAM_CHANGE` means the
    /// geometry just became known (or changed) and the output type must be
    /// re-selected before retrying, and anything else is a real failure.
    fn drain(&mut self) -> Result<Vec<DecodedFrame>, WindowError> {
        let mut out = Vec::new();

        loop {
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: STREAM_ID,
                ..Default::default()
            }];
            let mut status = 0u32;

            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };

            match result {
                Ok(()) => {
                    // `pSample` is `ManuallyDrop`, so taking the value out is
                    // what releases the COM reference — leaving it in place
                    // would leak one interface pointer per decoded frame.
                    let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) };
                    if let Some(sample) = sample {
                        if let Some(frame) = self.convert(&sample)? {
                            out.push(frame);
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The SPS has been parsed; the real size is now knowable.
                    unsafe { select_nv12_output(&self.transform)? };
                    self.size = unsafe { output_size(&self.transform) };
                    tracing::debug!(size = ?self.size, "decoder announced frame geometry");
                    continue;
                }
                Err(e) => return Err(WindowError::Backend(format!("ProcessOutput: {e}"))),
            }
        }

        Ok(out)
    }

    /// Turn one NV12 output sample into a tightly-packed BGRA frame.
    fn convert(&mut self, sample: &IMFSample) -> Result<Option<DecodedFrame>, WindowError> {
        let Some((width, height)) = self.size else {
            // A sample before the format change would have no geometry to
            // interpret it with. Not expected, but not worth crashing over.
            tracing::warn!("decoded sample arrived before frame geometry; dropping");
            return Ok(None);
        };

        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| WindowError::Backend(format!("contiguous buffer: {e}")))?;

            // The true stride is a property of the buffer, not of the width.
            // `IMF2DBuffer` is the only honest source; a plain `Lock` gives no
            // stride at all, and assuming `width` shears the image on hardware
            // decoders that pad their rows.
            let (data, stride, locked_2d) = match buffer.cast::<IMF2DBuffer>() {
                Ok(two_d) => {
                    let mut scanline = std::ptr::null_mut();
                    let mut pitch = 0i32;
                    two_d
                        .Lock2D(&mut scanline, &mut pitch)
                        .map_err(|e| WindowError::Backend(format!("Lock2D: {e}")))?;
                    (scanline as *const u8, pitch as usize, Some(two_d))
                }
                Err(_) => {
                    let mut ptr = std::ptr::null_mut();
                    let mut len = 0u32;
                    buffer
                        .Lock(&mut ptr, None, Some(&mut len))
                        .map_err(|e| WindowError::Backend(format!("lock output: {e}")))?;
                    (ptr as *const u8, width as usize, None)
                }
            };

            let need = stride * (height as usize) * 3 / 2;
            self.nv12.clear();
            self.nv12
                .extend_from_slice(std::slice::from_raw_parts(data, need));

            match &locked_2d {
                Some(two_d) => {
                    let _ = two_d.Unlock2D();
                }
                None => {
                    let _ = buffer.Unlock();
                }
            }

            Ok(Some(DecodedFrame {
                width,
                height,
                pixels: crate::video::nv12::to_bgra(&self.nv12, width, height, stride),
            }))
        }
    }
}

/// Read the frame size the transform settled on, after a format change.
unsafe fn output_size(transform: &IMFTransform) -> Option<(u32, u32)> {
    let media_type = transform.GetOutputCurrentType(STREAM_ID).ok()?;
    let packed = media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
    // MF packs frame size as width in the high 32 bits, height in the low.
    Some(((packed >> 32) as u32, packed as u32))
}

/// Find an H.264 decoder MFT, preferring whatever the machine offers first.
///
/// `MFTEnumEx` returns hardware transforms ahead of software ones, so taking
/// the first that activates gets GPU decoding where it exists without naming a
/// vendor. `SYNCMFT` restricts the search to the synchronous model: an async
/// MFT would require an event-driven pump, which buys nothing here because the
/// surrogate thread already has its own loop to drive.
unsafe fn find_h264_decoder() -> Result<IMFTransform, WindowError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &mut activates,
        &mut count,
    )
    .map_err(|e| WindowError::Backend(format!("MFTEnumEx: {e}")))?;

    if activates.is_null() || count == 0 {
        return Err(WindowError::Backend(
            "no H.264 decoder MFT on this machine".into(),
        ));
    }

    // The array is CoTaskMem and each entry is an owned reference; take the
    // first that actually activates, then release every entry either way.
    let list = std::slice::from_raw_parts(activates, count as usize);
    let mut chosen = None;
    for activate in list.iter().flatten() {
        if chosen.is_none() {
            match activate.ActivateObject::<IMFTransform>() {
                Ok(t) => chosen = Some(t),
                Err(e) => {
                    tracing::debug!(error = %e, "an H.264 MFT failed to activate; trying next")
                }
            }
        }
    }
    windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));

    chosen.ok_or_else(|| WindowError::Backend("no H.264 decoder MFT could be activated".into()))
}

/// Set the input type to H.264 and the output type to NV12.
///
/// Order matters: the transform rejects an output type until it knows the
/// input. The output type is set by *enumerating* what the transform offers
/// and taking the NV12 one, rather than constructing it — a decoder may
/// require attributes we would not know to set.
unsafe fn configure(transform: &IMFTransform) -> Result<(), WindowError> {
    let input =
        MFCreateMediaType().map_err(|e| WindowError::Backend(format!("media type: {e}")))?;
    input
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .and_then(|_| input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
        .map_err(|e| WindowError::Backend(format!("describe H.264 input: {e}")))?;
    transform
        .SetInputType(STREAM_ID, &input, 0)
        .map_err(|e| WindowError::Backend(format!("SetInputType: {e}")))?;

    select_nv12_output(transform)
}

/// Pick the transform's own NV12 output type and install it.
///
/// Called again after every `MF_E_TRANSFORM_STREAM_CHANGE`, which is when the
/// real frame size becomes available and the previous output type goes stale.
unsafe fn select_nv12_output(transform: &IMFTransform) -> Result<(), WindowError> {
    for i in 0.. {
        let candidate = match transform.GetOutputAvailableType(STREAM_ID, i) {
            Ok(t) => t,
            // Walked the whole list without finding NV12.
            Err(_) => break,
        };
        if candidate.GetGUID(&MF_MT_SUBTYPE) == Ok(MFVideoFormat_NV12) {
            return transform
                .SetOutputType(STREAM_ID, &candidate, 0)
                .map_err(|e| WindowError::Backend(format!("SetOutputType: {e}")));
        }
    }
    Err(WindowError::Backend(
        "H.264 MFT offers no NV12 output type".into(),
    ))
}

/// Wrap one Annex B packet in the `IMFSample` the transform consumes.
///
/// No timestamp is set: the surrogate presents frames as they arrive rather
/// than scheduling them, so a clock would only add latency. The decoder does
/// not need one to decode.
unsafe fn make_sample(packet: &[u8]) -> Result<IMFSample, WindowError> {
    let buffer = MFCreateMemoryBuffer(packet.len() as u32)
        .map_err(|e| WindowError::Backend(format!("MFCreateMemoryBuffer: {e}")))?;

    let mut dst = std::ptr::null_mut();
    let mut max = 0u32;
    buffer
        .Lock(&mut dst, Some(&mut max), None)
        .map_err(|e| WindowError::Backend(format!("lock input buffer: {e}")))?;
    std::ptr::copy_nonoverlapping(packet.as_ptr(), dst, packet.len());
    let _ = buffer.Unlock();
    buffer
        .SetCurrentLength(packet.len() as u32)
        .map_err(|e| WindowError::Backend(format!("SetCurrentLength: {e}")))?;

    let sample =
        MFCreateSample().map_err(|e| WindowError::Backend(format!("MFCreateSample: {e}")))?;
    sample
        .AddBuffer(&buffer)
        .map_err(|e| WindowError::Backend(format!("AddBuffer: {e}")))?;
    Ok(sample)
}
