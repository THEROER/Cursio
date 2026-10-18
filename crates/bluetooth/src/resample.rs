//! Between the link's sample rate (8 kHz CVSD, 16 kHz mSBC) and the 48 kHz
//! stereo `f32` every Cursio audio stream carries.
//!
//! Both rates divide 48 kHz, so the factor is a whole number and a linear
//! interpolator up and a box average down are enough: the link is speech
//! band-limited at 4 kHz, far below where either filter's shortcomings are
//! audible, and neither keeps more than one sample of state, so neither adds
//! delay to a call.

/// The audio path's rate.
pub const PATH_RATE: u32 = 48_000;

/// Link PCM (mono `i16`) up to path PCM (stereo `f32`, little endian bytes).
pub struct Upsampler {
    factor: usize,
    last: f32,
}

impl Upsampler {
    pub fn new(link_rate: u32) -> Self {
        Upsampler {
            factor: (PATH_RATE / link_rate) as usize,
            last: 0.0,
        }
    }

    pub fn push(&mut self, mono: &[i16], out: &mut Vec<u8>) {
        for &s in mono {
            let s = s as f32 / 32768.0;
            for k in 1..=self.factor {
                let v = self.last + (s - self.last) * k as f32 / self.factor as f32;
                let bytes = v.to_le_bytes();
                out.extend_from_slice(&bytes);
                out.extend_from_slice(&bytes);
            }
            self.last = s;
        }
    }
}

/// Path PCM (stereo `f32` bytes) down to link PCM (mono `i16`).
pub struct Downsampler {
    factor: usize,
    sum: f32,
    count: usize,
}

impl Downsampler {
    pub fn new(link_rate: u32) -> Self {
        Downsampler {
            factor: (PATH_RATE / link_rate) as usize,
            sum: 0.0,
            count: 0,
        }
    }

    pub fn push(&mut self, stereo_f32: &[u8], out: &mut Vec<i16>) {
        for frame in stereo_f32.as_chunks::<8>().0 {
            let l = f32::from_le_bytes(frame[0..4].try_into().expect("4 bytes"));
            let r = f32::from_le_bytes(frame[4..8].try_into().expect("4 bytes"));
            self.sum += (l + r) * 0.5;
            self.count += 1;
            if self.count == self.factor {
                let v = (self.sum / self.factor as f32).clamp(-1.0, 1.0);
                out.push((v * 32767.0) as i16);
                self.sum = 0.0;
                self.count = 0;
            }
        }
    }
}

/// Music PCM (interleaved `i16`, mono or stereo, any rate) to path PCM.
///
/// Linear interpolation at a fractional step: 44.1 kHz does not divide
/// 48 kHz. Fine for listening; the phone picks 48 kHz whenever it can, which
/// makes this a copy.
pub struct MusicResampler {
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame, in input frames past `prev`.
    pos: f64,
    prev: [f32; 2],
}

impl MusicResampler {
    pub fn new(rate: u32) -> Self {
        MusicResampler {
            step: f64::from(rate) / f64::from(PATH_RATE),
            pos: 1.0,
            prev: [0.0; 2],
        }
    }

    pub fn push(&mut self, pcm: &[i16], channels: u8, out: &mut Vec<u8>) {
        let channels = usize::from(channels.clamp(1, 2));
        for frame in pcm.chunks(channels) {
            let cur = [
                f32::from(frame[0]) / 32768.0,
                f32::from(*frame.get(1).unwrap_or(&frame[0])) / 32768.0,
            ];
            while self.pos <= 1.0 {
                let t = self.pos as f32;
                for (prev, cur) in self.prev.iter().zip(cur) {
                    let v = prev + (cur - prev) * t;
                    out.extend_from_slice(&v.to_le_bytes());
                }
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(bytes: &[u8]) -> Vec<f32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect()
    }

    #[test]
    fn one_link_sample_becomes_six_stereo_frames_at_8_khz() {
        let mut up = Upsampler::new(8_000);
        let mut out = Vec::new();
        up.push(&[16384], &mut out);
        let samples = frames(&out);
        assert_eq!(samples.len(), 12);
        assert!((samples[10] - 0.5).abs() < 1e-6);
        assert_eq!(samples[10], samples[11]);
    }

    #[test]
    fn a_constant_level_survives_the_round_trip() {
        let mut up = Upsampler::new(8_000);
        let mut down = Downsampler::new(8_000);
        let mut path = Vec::new();
        up.push(&[8000; 10], &mut path);
        let mut link = Vec::new();
        down.push(&path, &mut link);
        assert_eq!(link.len(), 10);
        assert!((link[9] - 8000).abs() <= 1);
    }

    #[test]
    fn music_at_48_khz_passes_frame_for_frame_and_44_1_grows_by_the_ratio() {
        let mut same = MusicResampler::new(48_000);
        let mut out = Vec::new();
        same.push(&[1000, -1000].repeat(480), 2, &mut out);
        assert_eq!(out.len() / 8, 480);

        let mut up = MusicResampler::new(44_100);
        let mut out = Vec::new();
        up.push(&[1000].repeat(4410), 1, &mut out);
        let frames = out.len() / 8;
        assert!((4799..=4801).contains(&frames), "{frames}");
    }

    #[test]
    fn a_partial_group_waits_for_the_rest() {
        let mut down = Downsampler::new(16_000);
        let mut link = Vec::new();
        down.push(&[0u8; 16], &mut link);
        assert!(link.is_empty());
        down.push(&[0u8; 8], &mut link);
        assert_eq!(link.len(), 1);
    }
}
