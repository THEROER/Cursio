//! The `AudioSpecificConfig` a decoder needs before it can read AAC-ELD.
//!
//! AirPlay sends raw AAC-ELD frames with no in-band header — there is no ADTS
//! sync word to find and nothing that describes the stream. Every decoder
//! therefore has to be told the sample rate, the channel count and the frame
//! length out of band, and MPEG-4 spells that out as an `AudioSpecificConfig`
//! bitstream. The parameters come from the `SETUP` plist (`sr`, `ch`, `spf`),
//! so this turns those into the bits a decoder expects.
//!
//! The layout, from ISO/IEC 14496-3:
//!
//! ```text
//! 5 bits   audioObjectType      31 = escape, meaning the real value follows
//! 6 bits   audioObjectTypeExt   AOT - 32; AAC-ELD is 39, so 7
//! 4 bits   samplingFrequencyIndex
//! 4 bits   channelConfiguration
//!          ELDSpecificConfig:
//! 1 bit      frameLengthFlag    1 = 480 samples, 0 = 512
//! 3 bits     three resilience flags, all off
//! 1 bit      ldSbrPresentFlag   no SBR in what AirPlay sends
//! 4 bits     eldExtType         0 terminates the extension list
//! ```

/// MPEG-4 audio object type for ER AAC ELD.
const AAC_ELD_OBJECT_TYPE: u8 = 39;

/// Sample rates in the order MPEG-4 indexes them.
const SAMPLING_FREQUENCIES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Writes the fields above one bit at a time; nothing here is byte-aligned.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: u32,
}

impl BitWriter {
    fn push(&mut self, value: u32, width: u32) {
        for index in (0..width).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> index) & 1 == 1 {
                let byte = self.bits as usize / 8;
                self.bytes[byte] |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }
}

/// Build the config for a stream described by a `SETUP`.
///
/// Unknown or unlisted values fall back to what AirPlay mirroring actually
/// uses — 44100 Hz stereo — because a missing field means "the usual", not
/// "refuse to play". iPhones leave `ch` out entirely.
pub(crate) fn audio_specific_config(
    sample_rate: u32,
    channels: u8,
    samples_per_frame: u32,
) -> Vec<u8> {
    let frequency_index = SAMPLING_FREQUENCIES
        .iter()
        .position(|rate| *rate == sample_rate)
        .unwrap_or(4) as u32;

    let mut writer = BitWriter::default();
    writer.push(31, 5);
    writer.push(u32::from(AAC_ELD_OBJECT_TYPE - 32), 6);
    writer.push(frequency_index, 4);
    writer.push(u32::from(channels), 4);
    // ELDSpecificConfig
    writer.push(u32::from(samples_per_frame == 480), 1);
    writer.push(0, 3); // resilience flags
    writer.push(0, 1); // ldSbrPresentFlag
    writer.push(0, 4); // eldExtType = ELDEXT_TERM
    writer.bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configuration an iPhone's mirroring audio actually needs. Worked out
    /// by hand from the layout above, so a change to the bit packing has to
    /// disagree with the specification to pass.
    #[test]
    fn the_iphone_profile_packs_to_a_known_config() {
        assert_eq!(
            audio_specific_config(44100, 2, 480),
            [0xf8, 0xe8, 0x50, 0x00]
        );
    }

    #[test]
    fn the_frame_length_flag_distinguishes_480_from_512() {
        let short = audio_specific_config(44100, 2, 480);
        let long = audio_specific_config(44100, 2, 512);
        assert_ne!(short, long);
        // The flag is the first bit after the 19 header bits: byte 2, bit 3.
        assert_eq!(short[2] & 0b0001_0000, 0b0001_0000);
        assert_eq!(long[2] & 0b0001_0000, 0);
    }

    #[test]
    fn the_sample_rate_selects_its_index() {
        // 48000 is index 3, 44100 is index 4.
        assert_ne!(
            audio_specific_config(48000, 2, 480),
            audio_specific_config(44100, 2, 480)
        );
        // An unlisted rate falls back to 44100 rather than producing nonsense.
        assert_eq!(
            audio_specific_config(1234, 2, 480),
            audio_specific_config(44100, 2, 480)
        );
    }

    #[test]
    fn bits_are_written_most_significant_first() {
        let mut writer = BitWriter::default();
        writer.push(0b101, 3);
        writer.push(0b1, 1);
        assert_eq!(writer.bytes, [0b1011_0000]);
    }
}
