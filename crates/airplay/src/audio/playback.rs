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

/// How far sound may fall behind the picture before the oldest is dropped.
///
/// Two frames' worth, so a hiccup in delivery is absorbed but a persistent
/// surplus is not: lip-sync is lost well before this is audible as a gap, and
/// the gap is a click while the lag would last the whole session.
#[cfg(any(target_os = "linux", target_os = "windows"))]
const MAX_AUDIO_LAG_NS: u64 = 40_000_000;

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod backend {
    use super::MAX_AUDIO_LAG_NS;
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    use crate::AirPlayError;

    pub(super) struct AudioPlayer {
        pipeline: gst::Pipeline,
        source: gst_app::AppSrc,
        volume: Option<gst::Element>,
    }

    impl AudioPlayer {
        pub(super) fn new(
            sample_rate: u32,
            channels: u8,
            config: &[u8],
        ) -> Result<Self, AirPlayError> {
            gst::init().map_err(|error| {
                AirPlayError::Audio(format!("GStreamer could not start ({error})"))
            })?;
            if gst::ElementFactory::find("avdec_aac").is_none() {
                return Err(AirPlayError::Audio(
                    "avdec_aac is required to decode AirPlay audio; install the GStreamer \
                     libav plugin (gst-libav)"
                        .into(),
                ));
            }

            // The queue is capped and leaky, and that is the whole point.
            // Picture is rendered the moment it arrives, while sound has to
            // pass through the output device's buffer — so audio is behind from
            // the start, and any excess pushed into it stays there. Bounding
            // the queue turns a permanent lag into a momentary one: the oldest
            // sound is dropped instead of pushing everything after it further
            // out of step with the screen.
            let pipeline = gst::parse::launch(&format!(
                "appsrc name=airplay-audio is-live=true format=time ! \
                 avdec_aac ! audioconvert ! audioresample ! \
                 volume name=airplay-volume ! \
                 queue max-size-buffers=0 max-size-bytes=0 \
                 max-size-time={MAX_AUDIO_LAG_NS} leaky=downstream ! \
                 autoaudiosink sync=false"
            ))
            .map_err(|error| AirPlayError::Audio(error.to_string()))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| AirPlayError::Audio("GStreamer did not create a pipeline".into()))?;

            let source = pipeline
                .by_name("airplay-audio")
                .ok_or_else(|| AirPlayError::Audio("pipeline has no audio appsrc".into()))?
                .downcast::<gst_app::AppSrc>()
                .map_err(|_| AirPlayError::Audio("AirPlay audio source is not an appsrc".into()))?;

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

            let volume = pipeline.by_name("airplay-volume");
            pipeline
                .set_state(gst::State::Playing)
                .map_err(|error| AirPlayError::Audio(error.to_string()))?;
            Ok(Self {
                pipeline,
                source,
                volume,
            })
        }

        /// Apply the sender's volume, given in AirPlay's decibel scale.
        pub(super) fn set_volume(&self, decibels: f32) {
            let Some(volume) = self.volume.as_ref() else {
                return;
            };
            volume.set_property("volume", linear_gain(decibels));
        }

        pub(super) fn push(
            &self,
            frame: &[u8],
            pts: std::time::Duration,
            duration: std::time::Duration,
        ) -> Result<(), AirPlayError> {
            let mut buffer = gst::Buffer::with_size(frame.len())
                .map_err(|error| AirPlayError::Audio(error.to_string()))?;
            {
                let buffer = buffer.get_mut().ok_or_else(|| {
                    AirPlayError::Audio("new audio buffer is unexpectedly shared".into())
                })?;
                buffer
                    .copy_from_slice(0, frame)
                    .map_err(|error| AirPlayError::Audio(error.to_string()))?;
                buffer.set_pts(gst::ClockTime::from_nseconds(pts.as_nanos() as u64));
                buffer.set_duration(gst::ClockTime::from_nseconds(duration.as_nanos() as u64));
            }
            self.source
                .push_buffer(buffer)
                .map_err(|error| AirPlayError::Audio(error.to_string()))?;
            Ok(())
        }
    }

    impl Drop for AudioPlayer {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    /// AirPlay's decibels as a linear gain.
    ///
    /// The scale runs from 0 dB at full volume down to about -30 dB, with -144
    /// reserved for muted — so that value has to become exact silence rather than
    /// the vanishingly small number the formula would give.
    fn linear_gain(decibels: f32) -> f64 {
        if decibels <= -144.0 {
            return 0.0;
        }
        f64::from(10.0_f32.powf(decibels / 20.0)).clamp(0.0, 1.0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn full_volume_passes_the_sound_through_unchanged() {
            assert!((linear_gain(0.0) - 1.0).abs() < 1e-9);
        }

        /// -144 is AirPlay's "muted"; the formula alone would leave a whisper.
        #[test]
        fn the_mute_value_is_exact_silence() {
            assert_eq!(linear_gain(-144.0), 0.0);
            assert_eq!(linear_gain(-200.0), 0.0);
        }

        #[test]
        fn quieter_decibels_mean_less_gain() {
            assert!(linear_gain(-10.0) < linear_gain(-5.0));
            assert!(linear_gain(-5.0) < linear_gain(0.0));
            // Half the amplitude is about -6 dB.
            assert!((linear_gain(-6.0) - 0.501).abs() < 0.01);
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod backend {
    use crate::AirPlayError;

    pub(super) struct AudioPlayer;

    impl AudioPlayer {
        pub(super) fn new(_: u32, _: u8, _: &[u8]) -> Result<Self, AirPlayError> {
            Err(AirPlayError::Audio(
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

        pub(super) fn set_volume(&self, _: f32) {}
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

    pub(crate) fn set_volume(&self, decibels: f32) {
        self.player.set_volume(decibels);
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
