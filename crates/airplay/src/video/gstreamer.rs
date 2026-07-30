use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::{PipelinePlan, VideoCodec};
use crate::AirPlayError;

pub(super) fn check_available(codec: VideoCodec) -> Result<(), AirPlayError> {
    gst::init().map_err(|error| {
        AirPlayError::Video(format!(
            "GStreamer could not start ({error}); install GStreamer 1.x and its hardware video plugins"
        ))
    })?;
    // A missing hardware decoder and a GStreamer that loaded no plugins at all
    // both surface as "element not found", but only the first is fixed by
    // installing a codec. The second happens on Windows whenever the core
    // GStreamer DLLs sit next to the executable: the plugin directory is
    // resolved relative to those copies, so nothing is found however complete
    // the installation is. `fakesink` ships in coreelements, so its absence
    // means the registry rather than the codec.
    if gst::ElementFactory::find("fakesink").is_none() {
        return Err(AirPlayError::Video(
            "GStreamer loaded no plugins; run with its own bin directory on PATH instead of \
             copying its DLLs beside the executable, or point GST_PLUGIN_PATH at the \
             gstreamer-1.0 plugin directory"
                .into(),
        ));
    }
    PipelinePlan::detect(codec, |name| gst::ElementFactory::find(name).is_some()).map(|_| ())
}

/// GStreamer pipeline that accepts Annex-B access units and renders decoded
/// frames without downloading them from GPU memory.
pub struct HardwareVideoPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
}

impl HardwareVideoPipeline {
    pub fn new(codec: VideoCodec) -> Result<Self, AirPlayError> {
        check_available(codec)?;
        let plan = PipelinePlan::detect(codec, |name| gst::ElementFactory::find(name).is_some())?;
        let pipeline = gst::parse::launch(&plan.launch_description())
            .map_err(|error| AirPlayError::Video(error.to_string()))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| AirPlayError::Video("GStreamer did not create a pipeline".into()))?;
        let source = pipeline
            .by_name("airplay-source")
            .ok_or_else(|| AirPlayError::Video("pipeline has no AirPlay appsrc".into()))?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| AirPlayError::Video("AirPlay source is not an appsrc".into()))?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| AirPlayError::Video(error.to_string()))?;
        Ok(Self { pipeline, source })
    }

    /// Queue one complete Annex-B access unit. `appsrc` is bounded and leaky,
    /// so a slow renderer cannot build an unbounded latency backlog.
    pub fn push_access_unit(
        &self,
        bytes: &[u8],
        pts: Option<std::time::Duration>,
    ) -> Result<(), AirPlayError> {
        let mut buffer = gst::Buffer::with_size(bytes.len())
            .map_err(|error| AirPlayError::Video(error.to_string()))?;
        {
            let buffer = buffer
                .get_mut()
                .ok_or_else(|| AirPlayError::Video("new buffer is unexpectedly shared".into()))?;
            buffer
                .copy_from_slice(0, bytes)
                .map_err(|error| AirPlayError::Video(error.to_string()))?;
            if let Some(pts) = pts {
                buffer.set_pts(gst::ClockTime::from_nseconds(
                    pts.as_nanos().min(u128::from(u64::MAX)) as u64,
                ));
            }
        }
        self.source
            .push_buffer(buffer)
            .map_err(|error| AirPlayError::Video(error.to_string()))?;
        Ok(())
    }
}

impl Drop for HardwareVideoPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
