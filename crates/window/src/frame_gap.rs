//! Noticing that frames of a stream never arrived.
//!
//! A frame can go missing on the way: a session moving to another path does
//! not resend pixels, which only describe the present. Every later H.264
//! packet then references a picture the decoder never had, and a lost tile
//! leaves its region stale, both until the host sends the whole picture —
//! which, with an unbounded GOP, it does only when asked. A decoder cannot
//! notice by itself: it decodes the references it lacks into smears rather
//! than failing. Every viewer of a stream (a surrogate window, a remote-screen
//! node, the phone screen kept for `cursio phone`) therefore watches the
//! numbers instead.
//!
//! Tiles of one update share a `seq`, so only a step of two or more is a
//! loss; a number going backwards is a stream that started over. A missing
//! band of an update whose other bands arrived, or a last frame before the
//! window went still, leaves no step to see.

/// One stream's numbers, as a viewer received them.
#[derive(Debug, Clone, Default)]
pub struct FrameGap {
    last: Option<u32>,
}

impl FrameGap {
    /// Take frame `seq`; returns how many frames before it never arrived.
    pub fn note(&mut self, seq: u32) -> u32 {
        let Some(last) = self.last else {
            self.last = Some(seq);
            return 0;
        };
        let step = seq.wrapping_sub(last);
        if step == 0 {
            return 0;
        }
        self.last = Some(seq);
        if step > u32::MAX / 2 {
            return 0;
        }
        step - 1
    }

    /// The stream starts over; numbers from before mean nothing now.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skipped_number_is_counted_as_missing() {
        let mut gap = FrameGap::default();
        assert_eq!(gap.note(1), 0);
        assert_eq!(gap.note(2), 0);
        assert_eq!(gap.note(5), 2);
        assert_eq!(gap.note(6), 0);
    }

    #[test]
    fn tiles_sharing_a_number_and_a_restarted_stream_are_not_losses() {
        let mut gap = FrameGap::default();
        for seq in [10, 10, 11, 0, 1] {
            assert_eq!(gap.note(seq), 0, "seq {seq}");
        }
    }

    #[test]
    fn numbering_wraps_without_a_loss() {
        let mut gap = FrameGap::default();
        gap.note(u32::MAX);
        assert_eq!(gap.note(0), 0);
    }

    #[test]
    fn a_reset_stream_starts_counting_afresh() {
        let mut gap = FrameGap::default();
        gap.note(3);
        gap.reset();
        assert_eq!(gap.note(9), 0);
    }
}
