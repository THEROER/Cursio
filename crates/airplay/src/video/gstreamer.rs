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

/// Give the sink a window Cursio owns, so what the user sees is named after
/// Cursio rather than after Direct3D.
///
/// Best-effort by design: a sink left to open its own window still shows the
/// mirrored screen, just under GStreamer's name. Losing the picture entirely
/// because a title could not be set would be the worse trade.
#[cfg(target_os = "windows")]
fn adopt_window(pipeline: &gst::Pipeline) -> Option<super::windows_window::MirrorWindow> {
    use gstreamer_video::prelude::VideoOverlayExt;

    let sink = pipeline.by_name("airplay-sink")?;
    let overlay = sink.dynamic_cast::<gstreamer_video::VideoOverlay>().ok()?;
    let window = match super::windows_window::MirrorWindow::open(1280, 720) {
        Ok(window) => window,
        Err(error) => {
            tracing::warn!(%error, "falling back to GStreamer's own video window");
            return None;
        }
    };
    // The handle must be set before the pipeline plays, or the sink has already
    // created the window this one was meant to replace.
    unsafe { overlay.set_window_handle(window.handle()) };
    Some(window)
}

/// GStreamer pipeline that accepts Annex-B access units and renders decoded
/// frames without downloading them from GPU memory.
pub struct HardwareVideoPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
    /// Kept alive for as long as the pipeline renders into it.
    #[cfg(target_os = "windows")]
    window: Option<super::windows_window::MirrorWindow>,
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
        #[cfg(target_os = "windows")]
        let window = adopt_window(&pipeline);
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| AirPlayError::Video(error.to_string()))?;
        Ok(Self {
            pipeline,
            source,
            #[cfg(target_os = "windows")]
            window,
        })
    }

    /// Queue one complete Annex-B access unit.
    ///
    /// Nothing downstream drops it: an access unit is not a picture, and losing
    /// one corrupts every frame that references it until the next IDR.
    pub fn push_access_unit(
        &self,
        bytes: &[u8],
        pts: Option<std::time::Duration>,
    ) -> Result<(), AirPlayError> {
        // Closing the window is how a user says "stop showing me this", so it
        // ends the mirror session rather than leaving the sender streaming into
        // a window that no longer exists.
        #[cfg(target_os = "windows")]
        if self
            .window
            .as_ref()
            .is_some_and(|window| window.is_closed())
        {
            return Err(AirPlayError::Video(
                "the AirPlay screen window was closed".into(),
            ));
        }
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
