//! Rational sample positions retained across capture and playback packets.
/// Return the output frame count and first source position, in target-rate units.
pub(crate) fn layout(
    frames: usize,
    source_rate: u32,
    target_rate: u32,
    phase: &mut u64,
) -> (usize, u64) {
    let source_rate = source_rate.max(1) as u64;
    let end = frames as u64 * target_rate.max(1) as u64;
    let first = *phase;
    let count = end.saturating_sub(first).div_ceil(source_rate);
    *phase = first + count * source_rate - end;
    (count as usize, first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_five_millisecond_packets_do_not_lose_half_a_sample_each() {
        let mut phase = 0;
        let count: usize = (0..200)
            .map(|_| layout(240, 48000, 44100, &mut phase).0)
            .sum();
        assert_eq!(count, 44100);
        assert_eq!(phase, 0);
    }

    #[test]
    fn chunk_boundaries_do_not_change_the_source_sample_positions() {
        for (source, target) in [(48000u32, 44100u32), (44100, 48000), (48000, 96000)] {
            let (mut phase, mut input, mut output) = (0, 0u64, 0u64);
            for size in [1, 127, 13, 240, 1024, 1, 256] {
                let (count, first) = layout(size, source, target, &mut phase);
                for i in 0..count as u64 {
                    let actual = input + (first + i * source as u64) / target as u64;
                    let expected = output * source as u64 / target as u64;
                    assert_eq!(actual, expected);
                    output += 1;
                }
                input += size as u64;
            }
            assert!(phase < source as u64);
        }
    }
}
