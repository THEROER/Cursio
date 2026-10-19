//! The jitter buffer a pull-model playback callback plays from.
//!
//! PipeWire and AAudio both hand the stream a buffer and ask for it filled,
//! at a size *they* choose per cycle. Every rule here is measured against that
//! size, because the buffer size is not a constant of the backend: PipeWire's
//! quantum is whatever the graph runs at (1024 frames by default, up to the
//! largest any linked application asks for), and a virtual microphone that
//! wrote its whole mapped buffer instead of the requested amount was asked for
//! 256 ms a cycle.
//!
//! Sized against a fixed few milliseconds instead, the prefill was smaller
//! than one cycle: playback started on a buffer that one cycle emptied, so the
//! next cycle found less than a prefill, muted and waited — every cycle. And
//! a 200 ms ceiling under a 256 ms cycle could never hold a whole cycle at
//! all. Both were heard on peer microphones as audio cutting in and out four
//! times a second while every chunk arrived on time.
//!
//! So the prefill, the ceiling and the catch-up point are each "one cycle,
//! plus" — the cycle is what plays next, and only what lies beyond it is
//! jitter margin or backlog.

use std::collections::VecDeque;

use crate::declick::{self, Declick};

/// What one [`Playout::fill`] did, for the backend to log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fill {
    /// Bytes of real audio written; the rest of the buffer is silence.
    pub played: usize,
    /// Playback (re)started on this cycle, after its prefill.
    pub started: bool,
    /// This cycle emptied the buffer, so the next waits for a prefill.
    pub ran_dry: bool,
    /// Bytes of the oldest audio dropped to stay under the ceiling.
    pub trimmed: usize,
}

/// One playback stream's queued audio, interleaved little-endian `f32`.
pub(crate) struct Playout {
    buf: VecDeque<u8>,
    started: bool,
    declick: Declick,
    channels: usize,
    rate: usize,
    fade: usize,
    /// Most audio kept queued, before one cycle is added to it.
    max_bytes: usize,
    /// Audio held beyond one cycle before playback starts: the jitter the
    /// stream can absorb without running dry.
    margin_bytes: usize,
    /// The size of the device's last cycle, so a push between cycles trims
    /// against the same ceiling the next fill will.
    cycle_bytes: usize,
}

impl Playout {
    pub(crate) fn new(rate: u32, channels: usize, max_bytes: usize, margin_bytes: usize) -> Self {
        let fade = declick::fade_frames(rate);
        Self {
            buf: VecDeque::with_capacity(max_bytes * 2),
            started: false,
            declick: Declick::new(fade),
            channels,
            rate: rate as usize,
            fade,
            max_bytes,
            margin_bytes,
            cycle_bytes: 0,
        }
    }

    fn frame_bytes(&self) -> usize {
        self.channels * 4
    }

    /// The ceiling: never below one cycle and its margin, or the buffer could
    /// not hold what a single cycle takes.
    fn ceiling(&self) -> usize {
        self.max_bytes.max(self.cycle_bytes + self.margin_bytes)
    }

    /// Queue newly arrived audio. Returns the bytes of old audio dropped to
    /// make room.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> usize {
        self.buf.extend(bytes);
        self.trim()
    }

    fn trim(&mut self) -> usize {
        let ceiling = self.ceiling();
        if self.buf.len() <= ceiling {
            return 0;
        }
        let mut excess = self.buf.len() - ceiling;
        excess -= excess % self.frame_bytes();
        if excess > 0 {
            declick::skip_crossfaded(&mut self.buf, excess, self.channels, self.fade);
        }
        excess
    }

    /// Fill `out`, one device cycle, from the queue, ramping every edge and
    /// scaling by `gain`. `stopping` ramps to silence and stays there.
    pub(crate) fn fill(&mut self, out: &mut [u8], gain: f32, stopping: bool) -> Fill {
        let frame_bytes = self.frame_bytes();
        let want = out.len() - out.len() % frame_bytes.max(1);
        self.cycle_bytes = want;
        let mut fill = Fill {
            trimmed: self.trim(),
            ..Fill::default()
        };
        if self.started {
            declick::settle_backlog(
                &mut self.buf,
                frame_bytes,
                self.rate,
                self.channels,
                self.fade,
                want,
            );
        }
        if !self.started && self.buf.len() >= want + self.margin_bytes {
            self.started = true;
            fill.started = true;
        }
        let take = if self.started && !(stopping && self.declick.is_silent()) {
            want.min(self.buf.len())
        } else {
            0
        };
        for (slot, byte) in out.iter_mut().zip(self.buf.drain(..take)) {
            *slot = byte;
        }
        self.declick.apply(
            &mut out[..take],
            self.channels,
            stopping || self.buf.is_empty(),
        );
        if (gain - 1.0).abs() > f32::EPSILON {
            for s in out[..take].as_chunks_mut::<4>().0 {
                *s = (f32::from_le_bytes(*s) * gain).to_le_bytes();
            }
        }
        out[take..].fill(0);
        fill.played = take;
        // Ran dry: prefill again rather than play single frames as they
        // trickle in, which is heard as crackle.
        if self.started && self.buf.is_empty() {
            self.started = false;
            fill.ran_dry = true;
        }
        fill
    }

    /// Whether the last frame written was silence.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn is_silent(&self) -> bool {
        self.declick.is_silent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const FRAME: usize = 8;
    const MS: usize = 48 * FRAME;
    const MAX: usize = 200 * MS;
    const MARGIN: usize = 1024 * FRAME;

    fn tone(frames: usize) -> Vec<u8> {
        std::iter::repeat_n(0.5f32, frames * 2)
            .flat_map(f32::to_le_bytes)
            .collect()
    }

    /// Chunks of `chunk` frames arriving exactly on time and a device pulling
    /// `cycle` frames, over `seconds`; returns every fill after the first start.
    fn steady(chunk: usize, cycle: usize, seconds: usize) -> Vec<Fill> {
        let mut playout = Playout::new(RATE, 2, MAX, MARGIN);
        let mut out = vec![0u8; cycle * FRAME];
        let mut fills = Vec::new();
        let (mut arrived, mut played) = (0usize, 0usize);
        let mut playing = false;
        for frame in 0..RATE as usize * seconds {
            if frame >= arrived + chunk {
                playout.push(&tone(chunk));
                arrived += chunk;
            }
            if frame >= played + cycle {
                let fill = playout.fill(&mut out, 1.0, false);
                played += cycle;
                playing |= fill.started;
                if playing {
                    fills.push(fill);
                }
            }
        }
        fills
    }

    #[test]
    fn on_time_chunks_never_run_dry_under_a_cycle_longer_than_a_chunk() {
        // A phone's 16 ms chunks into the graph's default 1024-frame quantum.
        let fills = steady(768, 1024, 3);
        assert!(!fills.is_empty(), "playback started");
        assert!(fills.iter().all(|f| !f.ran_dry), "{fills:?}");
        assert!(fills.iter().all(|f| f.played == 1024 * FRAME));
    }

    #[test]
    fn a_cycle_longer_than_the_ceiling_is_still_played_whole() {
        // 256 ms a cycle against a 200 ms ceiling.
        let fills = steady(768, 12_288, 4);
        assert!(!fills.is_empty(), "playback started");
        assert!(
            fills.iter().all(|f| f.played == 12_288 * FRAME),
            "{fills:?}"
        );
        assert!(fills.iter().all(|f| f.trimmed == 0));
    }

    #[test]
    fn playback_waits_for_a_whole_cycle_and_its_margin() {
        let mut playout = Playout::new(RATE, 2, MAX, MARGIN);
        let mut out = vec![0u8; 1024 * FRAME];
        playout.push(&tone(1024 + 1023));
        assert!(!playout.fill(&mut out, 1.0, false).started);
        assert!(
            out.iter().all(|&b| b == 0),
            "nothing plays before the prefill"
        );

        playout.push(&tone(1));
        let fill = playout.fill(&mut out, 1.0, false);
        assert!(fill.started);
        assert_eq!(fill.played, 1024 * FRAME);
        assert!(!fill.ran_dry, "the margin is still queued");
    }

    #[test]
    fn a_backlog_is_cut_back_to_one_cycle_and_the_settle_point() {
        let mut playout = Playout::new(RATE, 2, MAX, MARGIN);
        let mut out = vec![0u8; 2048 * FRAME];
        playout.push(&tone(2048 + 1024));
        assert!(playout.fill(&mut out, 1.0, false).started);
        playout.push(&tone(48 * 150));

        let fill = playout.fill(&mut out, 1.0, false);
        assert_eq!(fill.played, 2048 * FRAME);
        assert_eq!(
            playout.buf.len(),
            40 * MS,
            "what remains is the settle point"
        );
    }

    #[test]
    fn the_ceiling_drops_the_oldest_audio_between_cycles() {
        let mut playout = Playout::new(RATE, 2, MAX, MARGIN);
        let trimmed = playout.push(&tone(48 * 250));
        assert_eq!(trimmed, 50 * MS);
        assert_eq!(playout.buf.len(), MAX);
    }

    #[test]
    fn gain_scales_only_the_audio_played() {
        let mut playout = Playout::new(RATE, 2, MAX, MARGIN);
        let mut out = vec![0xffu8; 1024 * FRAME];
        playout.push(&tone(1024 * 3));
        playout.fill(&mut out, 0.5, false);
        let last = f32::from_le_bytes(out[out.len() - 4..].try_into().unwrap());
        assert_eq!(last, 0.25);
    }

    #[test]
    fn stopping_ramps_to_silence_and_plays_nothing_more() {
        let mut playout = Playout::new(RATE, 2, MAX, MARGIN);
        let mut out = vec![0u8; 1024 * FRAME];
        playout.push(&tone(1024 * 4));
        playout.fill(&mut out, 1.0, false);
        playout.fill(&mut out, 1.0, true);
        assert!(playout.is_silent());
        let fill = playout.fill(&mut out, 1.0, true);
        assert_eq!(fill.played, 0);
        assert!(out.iter().all(|&b| b == 0));
    }
}
