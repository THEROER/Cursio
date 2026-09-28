//! Hardware-decoded, GPU-resident video presentation.
//!
//! The compressed access unit is copied once into the platform pipeline;
//! decoded pixels stay in GPU-owned memory through presentation.

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod gstreamer;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows_window;

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub use gstreamer::HardwareVideoPipeline;
#[cfg(target_os = "macos")]
pub use macos::{prepare_window, HardwareVideoPipeline};

#[cfg(not(target_os = "macos"))]
pub fn prepare_window() -> Result<(), crate::AirPlayError> {
    Ok(())
}

use crate::AirPlayError;

/// Whether GStreamer's own DLLs can be loaded at all.
///
/// On Windows they are delay-loaded, so the binary starts without them and a
/// machine that never mirrors a phone needs nothing installed. The cost of that
/// is unforgiving: an unresolved delay-load raises a structured exception at
/// the first call rather than returning an error, and nothing here catches one
/// — `gst::init()` took the whole process down without a line in the log.
///
/// `LoadLibraryW` asks the same question the delay-load stub would, and answers
/// with a null handle instead of an exception. Every entry point into GStreamer
/// goes through here first.
///
/// The handles are deliberately not freed: something is about to call into
/// these libraries, and this is the process-wide load either way.
#[cfg(target_os = "windows")]
pub fn gstreamer_present() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;

    // Only the two that everything else depends on. A GStreamer missing just
    // `gstvideo` is not an installation this code can rescue, and probing all
    // five would report the same thing more slowly.
    for name in ["glib-2.0-0.dll", "gstreamer-1.0-0.dll"] {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the
        // call, which is all `LoadLibraryW` requires.
        if unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.is_err() {
            return false;
        }
    }
    true
}

/// Everywhere else the libraries are ordinary shared objects: if the crate
/// linked, they are present.
#[cfg(not(target_os = "windows"))]
pub fn gstreamer_present() -> bool {
    true
}

/// How large the mirrored screen should appear locally.
///
/// Carried from the configured [`VideoProfile`](crate::VideoProfile) rather
/// than read off the stream: the window has to exist before the sink starts,
/// which is before any frame — and before the encoded size is known — has
/// arrived. It is what the sender was told to produce in `/info`, so the two
/// agree unless the sender chose otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenSize {
    width: u16,
    height: u16,
}

impl ScreenSize {
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    pub const fn width(self) -> u32 {
        self.width as u32
    }

    pub const fn height(self) -> u32 {
        self.height as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

impl VideoCodec {
    const fn parser(self) -> &'static str {
        match self {
            Self::H264 => "h264parse",
            Self::Hevc => "h265parse",
        }
    }

    const fn caps(self) -> &'static str {
        match self {
            Self::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
            Self::Hevc => "video/x-h265,stream-format=byte-stream,alignment=au",
        }
    }
}

/// Report missing runtime components before an iPhone starts a session.
pub fn dependency_warnings(codec: VideoCodec) -> Vec<String> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        if let Err(error) = gstreamer::check_available(codec) {
            return vec![format!("AirPlay video is unavailable: {error}")];
        }
    }

    #[cfg(target_os = "macos")]
    {
        let _ = codec;
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = codec;
        return vec![
            "AirPlay video is unavailable: this operating system has no presentation backend"
                .to_string(),
        ];
    }

    Vec::new()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMemory {
    DmaBuf,
    D3d11,
    Gl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelinePlan {
    pub codec: VideoCodec,
    pub decoder: &'static str,
    pub sink: &'static str,
    pub memory: GpuMemory,
}

impl PipelinePlan {
    /// Choose a zero-copy decoder/sink pair from the elements installed on the
    /// machine. Software decoders are intentionally not accepted here.
    pub fn detect(
        codec: VideoCodec,
        mut has_element: impl FnMut(&str) -> bool,
    ) -> Result<Self, AirPlayError> {
        #[cfg(target_os = "linux")]
        {
            let nvidia_decoder = match codec {
                VideoCodec::H264 => "nvh264dec",
                VideoCodec::Hevc => "nvh265dec",
            };
            if has_element(nvidia_decoder) && has_element("glimagesink") {
                return Ok(Self {
                    codec,
                    decoder: nvidia_decoder,
                    sink: "glimagesink",
                    memory: GpuMemory::Gl,
                });
            }

            let decoder_candidates = match codec {
                VideoCodec::H264 => ["vah264dec", "vaapih264dec"],
                VideoCodec::Hevc => ["vah265dec", "vaapih265dec"],
            };
            let decoder = decoder_candidates
                .into_iter()
                .find(|name| has_element(name))
                .ok_or_else(|| {
                    AirPlayError::Video(
                        "no VA-API decoder exposing DMA-BUF surfaces is installed".into(),
                    )
                })?;
            let sink =
                if std::env::var_os("WAYLAND_DISPLAY").is_some() && has_element("waylandsink") {
                    "waylandsink"
                } else if has_element("glimagesink") {
                    "glimagesink"
                } else {
                    return Err(AirPlayError::Video(
                        "no DMA-BUF-capable Wayland/GL video sink is installed".into(),
                    ));
                };
            Ok(Self {
                codec,
                decoder,
                sink,
                memory: GpuMemory::DmaBuf,
            })
        }

        #[cfg(target_os = "windows")]
        {
            let decoder = match codec {
                VideoCodec::H264 => "d3d11h264dec",
                VideoCodec::Hevc => "d3d11h265dec",
            };
            if !has_element(decoder) || !has_element("d3d11videosink") {
                return Err(AirPlayError::Video(format!(
                    "{decoder} and d3d11videosink are required for GPU-resident decoding"
                )));
            }
            Ok(Self {
                codec,
                decoder,
                sink: "d3d11videosink",
                memory: GpuMemory::D3d11,
            })
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = (codec, &mut has_element);
            Err(AirPlayError::Video(
                "hardware video presentation is supported on Linux and Windows".into(),
            ))
        }
    }

    pub fn launch_description(&self) -> String {
        let output_caps = match self.memory {
            GpuMemory::DmaBuf => "video/x-raw(memory:DMABuf)",
            GpuMemory::D3d11 => "video/x-raw(memory:D3D11Memory)",
            GpuMemory::Gl => "video/x-raw(memory:GLMemory)",
        };
        let decoder_properties = if self.decoder.starts_with("nv") {
            " max-display-delay=0 num-output-surfaces=0"
        } else {
            ""
        };
        // Nothing here may drop a buffer. These are H.264 access units, not
        // finished pictures: discarding one costs every frame that references
        // it, so the picture stays broken until the next IDR — seconds of
        // blocky, wrongly-coloured output rather than the momentary hitch a
        // leaky queue buys elsewhere. A bounded leaky queue was the original
        // guard against latency building up behind a slow renderer, but a
        // hardware decoder keeps ahead of a mirroring iPhone, and when it does
        // not the honest fix is backpressure to the sender rather than
        // corruption. `max-buffers` is left generous so it only ever signals
        // `enough-data`.
        format!(
            "appsrc name=airplay-source is-live=true block=false format=time \
             max-buffers=64 caps=\"{}\" ! \
             {} config-interval=-1 ! {}{} ! {} ! {} name=airplay-sink sync=false qos=false",
            self.codec.caps(),
            self.codec.parser(),
            self.decoder,
            decoder_properties,
            output_caps,
            self.sink
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_host_fails_instead_of_copying_through_ram() {
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        assert!(PipelinePlan::detect(VideoCodec::H264, |_| true).is_err());
    }

    #[test]
    fn launch_description_is_live_and_never_drops_access_units() {
        let plan = PipelinePlan {
            codec: VideoCodec::H264,
            decoder: "vah264dec",
            sink: "waylandsink",
            memory: GpuMemory::DmaBuf,
        };
        let description = plan.launch_description();
        assert!(description.contains("is-live=true"));
        // Dropping a compressed access unit corrupts every frame that
        // references it, so no stage of this pipeline may leak.
        assert!(!description.contains("leaky"));
        assert!(description.contains("video/x-raw(memory:DMABuf)"));
        assert!(!description.contains("videoconvert"));
        assert!(!description.contains("appsink"));
    }

    #[test]
    fn windows_plan_keeps_d3d11_memory() {
        let plan = PipelinePlan {
            codec: VideoCodec::Hevc,
            decoder: "d3d11h265dec",
            sink: "d3d11videosink",
            memory: GpuMemory::D3d11,
        };
        let description = plan.launch_description();
        assert!(description.contains("video/x-raw(memory:D3D11Memory)"));
        assert!(description.contains("d3d11h265dec"));
        assert!(description.contains("d3d11videosink"));
    }

    #[test]
    fn nvidia_plan_keeps_frames_in_gpu_memory() {
        let plan = PipelinePlan {
            codec: VideoCodec::H264,
            decoder: "nvh264dec",
            sink: "glimagesink",
            memory: GpuMemory::Gl,
        };
        let description = plan.launch_description();
        assert!(description.contains("video/x-raw(memory:GLMemory)"));
        assert!(description.contains("max-display-delay=0"));
        assert!(description.contains("num-output-surfaces=0"));
    }
}
