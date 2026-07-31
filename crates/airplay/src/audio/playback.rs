//! Turning decrypted AAC-ELD into sound.
//!
//! The decode goes through GStreamer, which this crate already links for
//! mirroring video on Linux and Windows — `avdec_aac` handles ELD, and the
//! platform sink picks whatever output device the user has. There is no macOS
//! path: that would be AudioToolbox, and macOS receives AirPlay perfectly well
//! from Apple's own receiver, so it is the least useful place to spend the
//! effort.
//!
//! ## Every frame counts, including the silent ones
//!
//! Sound is a continuous line, not a series of pictures. A gap in it is
//! audible immediately as a click or a stutter, so nothing is skipped: the
//! short frames a quiet iPhone sends decode to silence and keep the timeline
//! whole, and dropping them as "filler" is what makes playback stutter in time
//! with the sound rather than run quietly underneath it.
//!
//! Buffers are timestamped from a frame counter for the same reason. `spf`
//! samples arrive every frame at a fixed rate, so the timeline is exactly
//! computable — and without it the sink has no idea when anything should be
//! heard and simply renders whatever turns up, which is what UDP jitter turns
//! into chopping.

use crate::AirPlayError;

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod backend {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    use crate::AirPlayError;

    pub(super) struct AudioPlayer {
        pipeline: gst::Pipeline,
        source: gst_app::AppSrc,
    }

    impl AudioPlayer {
        pub(super) fn new(
            sample_rate: u32,
            channels: u8,
            config: &[u8],
        ) -> Result<Self, AirPlayError> {
            gst::init().map_err(|error| {
                AirPlayError::Video(format!("GStreamer could not start ({error})"))
            })?;
            if gst::ElementFactory::find("avdec_aac").is_none() {
                return Err(AirPlayError::Video(
                    "avdec_aac is required to decode AirPlay audio; install the GStreamer \
                     libav plugin (gst-libav)"
                        .into(),
                ));
            }

            let pipeline = gst::parse::launch(
                "appsrc name=airplay-audio is-live=true format=time ! \
                 avdec_aac ! audioconvert ! audioresample ! autoaudiosink sync=false",
            )
            .map_err(|error| AirPlayError::Video(error.to_string()))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| AirPlayError::Video("GStreamer did not create a pipeline".into()))?;

            let source = pipeline
                .by_name("airplay-audio")
                .ok_or_else(|| AirPlayError::Video("pipeline has no audio appsrc".into()))?
                .downcast::<gst_app::AppSrc>()
                .map_err(|_| AirPlayError::Video("AirPlay audio source is not an appsrc".into()))?;

            // The decoder cannot start without this: raw AAC-ELD carries no
            // header of its own, so the whole description lives in the caps.
            let caps = gst::Caps::builder("audio/mpeg")
                .field("mpegversion", 4i32)
                .field("stream-format", "raw")
                .field("rate", sample_rate as i32)
                .field("channels", i32::from(channels))
                .field("codec_data", gst::Buffer::from_slice(config.to_vec()))
                .build();
            source.set_caps(Some(&caps));

            pipeline
                .set_state(gst::State::Playing)
                .map_err(|error| AirPlayError::Video(error.to_string()))?;
            Ok(Self { pipeline, source })
        }

        pub(super) fn push(
            &self,
            frame: &[u8],
            pts: std::time::Duration,
            duration: std::time::Duration,
        ) -> Result<(), AirPlayError> {
            let mut buffer = gst::Buffer::with_size(frame.len())
                .map_err(|error| AirPlayError::Video(error.to_string()))?;
            {
                let buffer = buffer.get_mut().ok_or_else(|| {
                    AirPlayError::Video("new audio buffer is unexpectedly shared".into())
                })?;
                buffer
                    .copy_from_slice(0, frame)
                    .map_err(|error| AirPlayError::Video(error.to_string()))?;
                buffer.set_pts(gst::ClockTime::from_nseconds(pts.as_nanos() as u64));
                buffer.set_duration(gst::ClockTime::from_nseconds(duration.as_nanos() as u64));
            }
            self.source
                .push_buffer(buffer)
                .map_err(|error| AirPlayError::Video(error.to_string()))?;
            Ok(())
        }
    }

    impl Drop for AudioPlayer {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod backend {
    use crate::AirPlayError;

    pub(super) struct AudioPlayer;

    impl AudioPlayer {
        pub(super) fn new(_: u32, _: u8, _: &[u8]) -> Result<Self, AirPlayError> {
            Err(AirPlayError::Video(
                "playing AirPlay audio is supported on Linux and Windows".into(),
            ))
        }

        pub(super) fn push(
            &self,
            _: &[u8],
            _: std::time::Duration,
            _: std::time::Duration,
        ) -> Result<(), AirPlayError> {
            Ok(())
        }
    }
}

/// Plays decrypted AAC-ELD frames on this machine's default output.
pub(crate) struct AudioPlayback {
    player: backend::AudioPlayer,
    /// How many frames have been handed over, which is the whole timeline: at a
    /// fixed rate and frame size, frame *n* starts at `n * spf / rate`.
    frames: u64,
    frame_duration: std::time::Duration,
    /// Reported once rather than per frame: at 480 samples a failing decoder
    /// would otherwise log a hundred times a second.
    failed: bool,
}

impl AudioPlayback {
    pub(crate) fn new(
        sample_rate: u32,
        channels: u8,
        samples_per_frame: u32,
    ) -> Result<Self, AirPlayError> {
        let config = super::config::audio_specific_config(sample_rate, channels, samples_per_frame);
        tracing::info!(
            sample_rate,
            channels,
            samples_per_frame,
            config = %config.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
            "starting AirPlay audio playback"
        );
        Ok(Self {
            player: backend::AudioPlayer::new(sample_rate, channels, &config)?,
            frames: 0,
            frame_duration: std::time::Duration::from_nanos(
                1_000_000_000 * u64::from(samples_per_frame) / u64::from(sample_rate.max(1)),
            ),
            failed: false,
        })
    }

    pub(crate) fn push(&mut self, frame: &[u8]) {
        if self.failed {
            return;
        }
        let pts = self.frame_duration.saturating_mul(self.frames as u32);
        self.frames += 1;
        if let Err(error) = self.player.push(frame, pts, self.frame_duration) {
            self.failed = true;
            tracing::warn!(%error, "AirPlay audio playback stopped");
        }
    }
}
