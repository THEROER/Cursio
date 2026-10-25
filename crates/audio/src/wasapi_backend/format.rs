//! Between the wire's f32 stereo at 48 kHz and whatever an endpoint
//! mixes at: channel counts folded or spread, and the rate converted
//! with its fractional position carried across packets.

use cursio_protocol::{AudioFormat, SampleKind};

/// The wire format this backend targets: 48 kHz stereo f32, same as Linux.
///
/// Devices really do differ from this — a VB-CABLE endpoint enumerates as
/// "CABLE In 16ch", and plenty of hardware runs at 44.1 kHz. Capture is
/// initialized with the device's own mix format (that is what WASAPI shared
/// mode gives us), so [`to_wire_format`] converts each buffer to the format we
/// advertise. Sending device bytes under a 48k-stereo label is what made a
/// 16-channel endpoint arrive as noise.
pub(super) const SAMPLE_RATE: u32 = 48_000;
pub(super) const CHANNELS: u16 = 2;

/// Convert one buffer from the wire format (48 kHz stereo f32) into the
/// device's mix format, for rendering.
///
/// The mirror image of [`to_wire_format`]: extra channels are filled by
/// repeating the stereo pair (so a multi-channel endpoint isn't left with
/// garbage in the other channels), and the rate is nearest-neighbour resampled.
#[cfg(test)]
pub(super) fn from_wire_format(src: &[u8], dst_channels: u16, dst_rate: u32) -> Vec<u8> {
    from_wire_format_stream(src, dst_channels, dst_rate, &mut 0)
}

pub(super) fn from_wire_format_stream(
    src: &[u8],
    dst_channels: u16,
    dst_rate: u32,
    phase: &mut u64,
) -> Vec<u8> {
    if dst_channels == CHANNELS && dst_rate == SAMPLE_RATE {
        return src.to_vec();
    }
    if dst_channels == 0 {
        return Vec::new();
    }

    let samples: Vec<f32> = src
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let src_frames = samples.len() / CHANNELS as usize;
    if src_frames == 0 {
        return Vec::new();
    }

    let (dst_frames, first) = crate::sample_clock::layout(src_frames, SAMPLE_RATE, dst_rate, phase);
    let mut out = Vec::with_capacity(dst_frames * dst_channels as usize * 4);
    for i in 0..dst_frames {
        let src_i = ((first + i as u64 * SAMPLE_RATE as u64) / dst_rate.max(1) as u64) as usize;
        let base = src_i.min(src_frames - 1) * CHANNELS as usize;
        let (left, right) = (samples[base], samples[base + 1]);

        if dst_channels == 1 {
            // A mono endpoint must hear BOTH channels. Taking only the left
            // silently drops everything panned right — anything hard-panned
            // vanishes outright. Average rather than sum so the result cannot
            // clip when both channels are loud.
            let v = (left + right) * 0.5;
            out.extend_from_slice(&v.to_le_bytes());
            continue;
        }
        for ch in 0..dst_channels {
            // Alternate L/R across however many channels the device has.
            let v = if ch % 2 == 0 { left } else { right };
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// Convert one captured buffer from the device's mix format into the wire
/// format (48 kHz stereo f32 little-endian).
///
/// Handles the two ways a device commonly differs:
/// - **Channel count** — extra channels are dropped and mono is duplicated, so
///   the frame size matches what the peer expects. Without this, a 16-channel
///   frame read as 2 channels slides the stream out of alignment every frame,
///   which is heard as continuous noise.
/// - **Sample rate** — nearest-neighbour resampling to 48 kHz. Cheap and
///   artefact-prone compared to a proper filter, but correct in pitch and
///   duration; a real resampler belongs with the Opus milestone.
///
/// Sample *kind* is not converted: WASAPI shared mode mixes to float, so the
/// mix format is f32 in practice. A non-float device is left alone rather than
/// silently mangled — better a wrong-sounding stream than a wrong-looking one.
#[cfg(test)]
pub(super) fn to_wire_format(src: &[u8], src_channels: u16, src_rate: u32) -> Vec<u8> {
    to_wire_format_stream(src, src_channels, src_rate, &mut 0)
}

pub(super) fn to_wire_format_stream(
    src: &[u8],
    src_channels: u16,
    src_rate: u32,
    phase: &mut u64,
) -> Vec<u8> {
    // Already what we want: hand the bytes straight through.
    if src_channels == CHANNELS && src_rate == SAMPLE_RATE {
        return src.to_vec();
    }
    if src_channels == 0 {
        return Vec::new();
    }

    let samples: Vec<f32> = src
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let src_frames = samples.len() / src_channels as usize;
    if src_frames == 0 {
        return Vec::new();
    }

    // Resample by frame index so the output lasts the right amount of time.
    let (dst_frames, first) = crate::sample_clock::layout(src_frames, src_rate, SAMPLE_RATE, phase);
    let mut out = Vec::with_capacity(dst_frames * CHANNELS as usize * 4);
    for i in 0..dst_frames {
        let src_i = ((first + i as u64 * src_rate.max(1) as u64) / SAMPLE_RATE as u64) as usize;
        let base = src_i.min(src_frames - 1) * src_channels as usize;

        // Take the first two channels; duplicate when the source is mono.
        let left = samples[base];
        let right = if src_channels >= 2 {
            samples[base + 1]
        } else {
            left
        };
        out.extend_from_slice(&left.to_le_bytes());
        out.extend_from_slice(&right.to_le_bytes());
    }
    out
}

pub(crate) fn wire_format() -> AudioFormat {
    AudioFormat {
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS as u8,
        sample_kind: SampleKind::F32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32s(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|s| s.to_le_bytes()).collect()
    }
    fn parse(b: &[u8]) -> Vec<f32> {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect()
    }

    /// The bug the user heard as noise: a 16-channel endpoint (VB-CABLE
    /// enumerates as "CABLE In 16ch") was sent as-is under a stereo label, so
    /// every frame slid out of alignment.
    #[test]
    fn multichannel_capture_is_reduced_to_stereo() {
        // Two frames of 16 channels; only the first two carry signal.
        let mut input = Vec::new();
        for f in 0..2 {
            for ch in 0..16 {
                input.push(if ch < 2 {
                    (f * 2 + ch) as f32 * 0.1
                } else {
                    0.9
                });
            }
        }
        let out = to_wire_format(&f32s(&input), 16, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.0, 0.1, 0.2, 0.3]);
    }

    /// Mono must be duplicated, not left half-length — otherwise the peer reads
    /// one frame as half of the next.
    #[test]
    fn mono_capture_is_duplicated_to_stereo() {
        let out = to_wire_format(&f32s(&[0.5, -0.5]), 1, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.5, 0.5, -0.5, -0.5]);
    }

    /// A 44.1 kHz device must come out at 48 kHz, or it plays at the wrong
    /// pitch and drifts.
    #[test]
    fn rate_conversion_lands_on_the_wire_rate() {
        let frames = 441;
        let input: Vec<f32> = (0..frames * 2).map(|i| i as f32 * 0.001).collect();
        let out = to_wire_format(&f32s(&input), 2, 44_100);
        assert_eq!(
            parse(&out).len() / 2,
            480,
            "441 frames @44.1k should be 480 @48k"
        );
    }

    /// Matching format must be a pass-through — no needless copying or drift.
    #[test]
    fn matching_format_passes_through_untouched() {
        let bytes = f32s(&[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(to_wire_format(&bytes, 2, SAMPLE_RATE), bytes);
        assert_eq!(from_wire_format(&bytes, 2, SAMPLE_RATE), bytes);
    }

    /// Render into a multi-channel endpoint must fill every channel, not leave
    /// the extras as garbage.
    #[test]
    fn render_expands_stereo_to_multichannel() {
        let out = from_wire_format(&f32s(&[0.25, -0.25]), 4, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.25, -0.25, 0.25, -0.25]);
    }

    /// Rendering to a mono endpoint must carry both channels, not just the
    /// left — otherwise hard-panned audio disappears and a stereo mix is heard
    /// as "one ear only".
    #[test]
    fn render_downmixes_stereo_to_mono() {
        // Hard-panned right: a left-only downmix would render pure silence.
        let out = from_wire_format(&f32s(&[0.0, 1.0]), 1, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.5]);

        // Centre content keeps its level rather than doubling into clipping.
        let out = from_wire_format(&f32s(&[0.8, 0.8]), 1, SAMPLE_RATE);
        assert_eq!(parse(&out), vec![0.8]);
    }

    /// Round-tripping through an odd device format must preserve frame count,
    /// so the stream neither stretches nor compresses over time.
    #[test]
    fn round_trip_preserves_duration() {
        let frames = 480;
        let input: Vec<f32> = (0..frames * 2).map(|i| (i as f32 * 0.01).sin()).collect();
        let to_device = from_wire_format(&f32s(&input), 6, 44_100);
        let back = to_wire_format(&to_device, 6, 44_100);
        let got = parse(&back).len() / 2;
        assert!(
            got.abs_diff(frames) <= 2,
            "round trip changed duration: {frames} -> {got}"
        );
    }
}
