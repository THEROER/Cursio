//! Platform-neutral frame pipeline: diffing, codec routing, tile selection.
//!
//! Both capture backends — the Linux portal/PipeWire one and the Windows
//! Graphics Capture one — receive the same thing: a full BGRA frame, every
//! tick, with no indication of what changed. Everything they do with that frame
//! before it becomes a [`WindowFrame`] is identical, so it lives here rather
//! than being written twice (and drifting).
//!
//! The pipeline is: diff against the previous frame to find the changed
//! bounding box, then decide whether that change rides a lossless tile or the
//! video stream.

mod differ;
mod keyframe;
mod pipeline;
mod quality;

pub use differ::FrameDiffer;
pub use keyframe::{KeyframeClock, KEYFRAME_INTERVAL};
pub use pipeline::{
    try_queue_frames, CapturePipeline, EncoderSource, FrameQueueError, StreamEncoder, SETTLE,
};
pub use quality::{
    lossy_bitrate, pick_codec, should_use_video, AdaptiveQuality, BitrateLadder, ThresholdLadder,
    VIDEO_AREA_THRESHOLD_MAX, VIDEO_AREA_THRESHOLD_MIN,
};
