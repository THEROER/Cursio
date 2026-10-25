//! When a video stream must send a picture that stands on its own: at
//! the start, on request, and at least every few seconds.

/// The longest a viewer entering a video stream waits for a picture it can
/// start decoding from.
///
/// Keyframes are the expensive frames — nothing to predict from — which is why
/// this is seconds rather than a fraction of one.
pub const KEYFRAME_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Decides when a video stream's next frame must be a keyframe.
///
/// By elapsed time, not by frame count. An encoder's own GOP counts the frames
/// it is given, and capture gives it one only when the screen changed: at 60
/// changes a second a GOP of 120 is two seconds, but on a desktop where only a
/// clock ticks it is never — so a viewer that missed the stream's opening
/// keyframe dropped every packet after it as undecodable, forever, and saw
/// only black plus whatever lossless tiles happened to land. Asking "has it
/// been two seconds?" at each encode bounds that wait by the clock however
/// seldom frames come.
#[derive(Debug, Default)]
pub struct KeyframeClock {
    last: Option<std::time::Instant>,
}

impl KeyframeClock {
    /// Whether the frame about to be encoded must be a keyframe.
    pub fn due(&self, now: std::time::Instant) -> bool {
        self.last
            .is_none_or(|last| now.saturating_duration_since(last) >= KEYFRAME_INTERVAL)
    }

    /// Record what went out. Read from the bitstream rather than from what was
    /// asked for, so a keyframe the encoder chose itself (a scene cut, NVENC
    /// re-keying on a rate change) restarts the interval too.
    pub fn note_sent(&mut self, packets: &[Vec<u8>], now: std::time::Instant) {
        if packets
            .iter()
            .any(|packet| crate::video::contains_keyframe(packet))
        {
            self.last = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use std::time::Instant;

    /// An Annex B packet holding one IDR slice.
    fn idr() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x65, 0x88, 0x84]
    }

    /// An Annex B packet holding one non-IDR slice.
    fn p_frame() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x41, 0x9a, 0x02]
    }

    #[test]
    fn a_stream_opens_on_a_keyframe() {
        assert!(KeyframeClock::default().due(Instant::now()));
    }

    #[test]
    fn keyframes_come_by_time_however_few_frames_are_encoded() {
        let mut clock = KeyframeClock::default();
        let start = Instant::now();
        clock.note_sent(&[idr()], start);
        assert!(!clock.due(start + Duration::from_millis(1999)));
        // One frame in two seconds — a desktop where only a clock ticks. A
        // frame-counted GOP would not key for minutes here.
        assert!(clock.due(start + KEYFRAME_INTERVAL));
    }

    #[test]
    fn predicted_frames_do_not_restart_the_interval_but_any_keyframe_does() {
        let mut clock = KeyframeClock::default();
        let start = Instant::now();
        clock.note_sent(&[idr()], start);
        clock.note_sent(&[p_frame()], start + Duration::from_millis(1500));
        assert!(clock.due(start + KEYFRAME_INTERVAL));

        // A keyframe the encoder decided on itself counts as well.
        let later = start + Duration::from_secs(3);
        clock.note_sent(&[p_frame(), idr()], later);
        assert!(!clock.due(later + Duration::from_secs(1)));
    }
}
