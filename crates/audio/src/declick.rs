//! Short gain ramps where playback starts, stops or jumps.
//!
//! A remote stream begins and ends abruptly by nature: a system event sound
//! opens a stream, plays a few hundred milliseconds and closes it; a jitter
//! buffer that runs dry cuts the signal off mid-waveform; a backlog trim skips
//! straight from one point in the waveform to another. Each of those is a step
//! in the output, and a step is heard as a click however quiet the audio around
//! it. The cure is the one every player uses — a few milliseconds of ramp —
//! applied at the edges only, so steady playback stays a plain copy.
//!
//! Everything here works in place on interleaved little-endian `f32` bytes,
//! which is what both playback backends hold, and allocates nothing: it runs
//! inside the real-time callback.

use std::collections::VecDeque;

/// Length of every ramp. Long enough that the edge carries no audible
/// broadband energy, short enough that the attack of a short sound survives.
const FADE_MS: usize = 5;

/// Ramp length in frames at `sample_rate`.
pub(crate) fn fade_frames(sample_rate: u32) -> usize {
    (sample_rate as usize * FADE_MS / 1000).max(1)
}

/// The gain one playback stream is currently at, carried across buffers so a
/// ramp that straddles two callbacks continues where it left off.
#[derive(Debug, Clone)]
pub(crate) struct Declick {
    gain: f32,
    step: f32,
}

impl Declick {
    /// Starts silent, so the first audio played fades in.
    pub(crate) fn new(fade_frames: usize) -> Self {
        Self {
            gain: 0.0,
            step: 1.0 / fade_frames.max(1) as f32,
        }
    }

    /// Whether the last frame this produced was silence.
    pub(crate) fn is_silent(&self) -> bool {
        self.gain == 0.0
    }

    /// Ramp `real`, the audio actually written into this output buffer, as
    /// interleaved `f32` frames of `channels`.
    ///
    /// `ending` says nothing follows it — the jitter buffer ran dry or the
    /// stream is stopping — so it must reach silence by its own last frame; the
    /// next audio played then fades in from there. A fade-out that finds fewer
    /// frames than a full ramp uses what there is: a steep ramp still beats a
    /// step.
    pub(crate) fn apply(&mut self, real: &mut [u8], channels: usize, ending: bool) {
        let frame_bytes = channels * 4;
        if frame_bytes == 0 {
            return;
        }
        if self.gain >= 1.0 && !ending {
            return;
        }
        let frames = real.len() / frame_bytes;
        let mut gain = self.gain;
        for (i, frame) in real.chunks_exact_mut(frame_bytes).enumerate() {
            gain = (self.gain + (i + 1) as f32 * self.step).min(1.0);
            if ending {
                gain = gain.min((frames - 1 - i) as f32 * self.step);
            }
            for sample in frame.as_chunks_mut::<4>().0 {
                *sample = (f32::from_le_bytes(*sample) * gain).to_le_bytes();
            }
        }
        self.gain = if ending { 0.0 } else { gain };
    }
}

/// Drop the first `skip` bytes of `buf`, crossfading the audio that would
/// have played next into the audio that now plays instead.
///
/// Discarding the oldest audio to bound latency jumps the waveform from one
/// instant to another; blending the two over a ramp turns that jump into a
/// short mix. `skip` must be a whole number of frames. When there is not a full
/// ramp on either side, the blend is as long as the shorter side allows.
pub(crate) fn skip_crossfaded(
    buf: &mut VecDeque<u8>,
    skip: usize,
    channels: usize,
    fade_frames: usize,
) {
    let frame_bytes = channels * 4;
    let skip = skip.min(buf.len());
    if frame_bytes == 0 || skip == 0 {
        buf.drain(..skip);
        return;
    }
    let blend = fade_frames
        .min(skip / frame_bytes)
        .min((buf.len() - skip) / frame_bytes);
    let read = |buf: &VecDeque<u8>, at: usize| {
        f32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
    };
    for frame in 0..blend {
        let t = (frame + 1) as f32 / (blend + 1) as f32;
        for channel in 0..channels {
            let at = frame * frame_bytes + channel * 4;
            let mixed = read(buf, at) * (1.0 - t) + read(buf, skip + at) * t;
            for (k, byte) in mixed.to_le_bytes().into_iter().enumerate() {
                buf[skip + at + k] = byte;
            }
        }
    }
    buf.drain(..skip);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn samples(bytes: &[u8]) -> Vec<f32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect()
    }

    #[test]
    fn playback_fades_in_from_silence_on_both_channels_alike() {
        let mut declick = Declick::new(4);
        let mut buf = bytes(&[1.0; 12]);

        declick.apply(&mut buf, 2, false);

        assert_eq!(
            samples(&buf),
            vec![0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]
        );
    }

    #[test]
    fn a_fade_in_continues_across_buffer_boundaries() {
        let mut declick = Declick::new(4);
        let mut first = bytes(&[1.0, 1.0]);
        let mut second = bytes(&[1.0, 1.0, 1.0]);

        declick.apply(&mut first, 1, false);
        declick.apply(&mut second, 1, false);

        assert_eq!(samples(&first), vec![0.25, 0.5]);
        assert_eq!(samples(&second), vec![0.75, 1.0, 1.0]);
    }

    #[test]
    fn steady_playback_is_left_bit_exact() {
        let mut declick = Declick::new(2);
        declick.apply(&mut bytes(&[1.0; 4]), 1, false);
        let original = bytes(&[0.3, -0.7, 0.123_456, -1.0]);
        let mut buf = original.clone();

        declick.apply(&mut buf, 1, false);

        assert_eq!(buf, original);
    }

    #[test]
    fn an_ending_buffer_reaches_silence_on_its_last_frame() {
        let mut declick = Declick::new(4);
        declick.apply(&mut bytes(&[1.0; 8]), 1, false);
        let mut buf = bytes(&[1.0; 12]);

        declick.apply(&mut buf, 2, true);

        assert_eq!(
            samples(&buf),
            vec![1.0, 1.0, 1.0, 1.0, 0.75, 0.75, 0.5, 0.5, 0.25, 0.25, 0.0, 0.0]
        );
        assert!(declick.is_silent());
    }

    #[test]
    fn audio_after_an_underrun_fades_back_in() {
        let mut declick = Declick::new(2);
        declick.apply(&mut bytes(&[1.0; 4]), 1, false);
        declick.apply(&mut bytes(&[1.0; 4]), 1, true);
        let mut buf = bytes(&[1.0; 3]);

        declick.apply(&mut buf, 1, false);

        assert_eq!(samples(&buf), vec![0.5, 1.0, 1.0]);
    }

    #[test]
    fn a_short_stream_fades_in_and_out_without_exceeding_its_own_level() {
        let mut declick = Declick::new(4);
        let mut buf = bytes(&[1.0; 5]);

        declick.apply(&mut buf, 1, true);

        let out = samples(&buf);
        assert_eq!(out.first(), Some(&0.25));
        assert_eq!(out.last(), Some(&0.0));
        assert!(out.iter().all(|s| (0.0..=1.0).contains(s)));
    }

    #[test]
    fn a_backlog_trim_blends_into_the_new_position_instead_of_jumping() {
        // Two stereo frames of +1 that would have played next, then the -1
        // audio that plays after the trim.
        let mut buf: VecDeque<u8> =
            bytes(&[1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0, -1.0, -1.0])
                .into_iter()
                .collect();

        skip_crossfaded(&mut buf, 16, 2, 4);

        let out = samples(buf.make_contiguous());
        let third = 1.0 / 3.0;
        let expected = [third, third, -third, -third, -1.0, -1.0];
        assert_eq!(out.len(), expected.len(), "only the skipped frames go");
        for (got, want) in out.iter().zip(expected) {
            assert!((got - want).abs() < 1e-6, "{out:?}");
        }
    }

    #[test]
    fn a_trim_with_nothing_after_it_simply_drops() {
        let mut buf: VecDeque<u8> = bytes(&[1.0, 1.0]).into_iter().collect();

        skip_crossfaded(&mut buf, 8, 1, 4);

        assert!(buf.is_empty());
    }
}
