//! Hardware-decoded, GPU-resident video presentation.
//!
//! The compressed access unit is copied once into the platform pipeline;
//! decoded pixels stay in GPU-owned memory through presentation.

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod gstreamer;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub use gstreamer::HardwareVideoPipeline;
#[cfg(target_os = "macos")]
pub use macos::HardwareVideoPipeline;

use crate::AirPlayError;

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
            return Ok(Self {
                codec,
                decoder,
                sink,
                memory: GpuMemory::DmaBuf,
            });
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
            return Ok(Self {
                codec,
                decoder,
                sink: "d3d11videosink",
                memory: GpuMemory::D3d11,
            });
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
        format!(
            "appsrc name=airplay-source is-live=true block=false format=time \
             max-buffers=2 leaky-type=downstream caps=\"{}\" ! \
             {} config-interval=-1 ! {}{} ! {} ! {} sync=false qos=false",
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
    fn launch_description_is_live_and_bounded() {
        let plan = PipelinePlan {
            codec: VideoCodec::H264,
            decoder: "vah264dec",
            sink: "waylandsink",
            memory: GpuMemory::DmaBuf,
        };
        let description = plan.launch_description();
        assert!(description.contains("max-buffers=2"));
        assert!(description.contains("leaky-type=downstream"));
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
