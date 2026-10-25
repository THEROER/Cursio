//! How a change travels and at what quality: the area past which it rides
//! the video stream instead of a lossless tile, and the bitrate, both
//! following what the link is seen to carry.

use cursio_protocol::{Rect, TileCodec};

/// The threshold a link with room to spare uses.
///
/// Above some fraction of the window's pixels a change is sent through the video
/// encoder instead of as a lossless tile, because the two paths win in opposite
/// regimes (2026-07-20 measurements):
///
/// - A *small* dirty rect (caret, a typed character, a hover highlight) is a few
///   hundred bytes of zstd and needs no encoder state — cheaper and simpler than
///   pushing a whole frame through H.264.
/// - A *large* one degenerates: zstd manages only ~2.9x on real content, so a
///   near-full-window update costs ~1.5 MiB, while the video encoder sends the
///   same change for ~0.1–280 KiB because it codes the *difference* from the
///   previous frame.
///
/// Which fraction is not fixed, though — see [`ThresholdLadder`]. This is the
/// value used when the viewer is keeping up.
///
/// The video path is **not** free: it is lossy, and visibly so on the content
/// windows actually contain. H.264 here codes NV12 — 4:2:0 chroma, one colour
/// sample per four pixels — which is built for camera footage and is exactly
/// wrong for text, thin lines and flat UI. At the old flat 15% a scrolled page
/// or a switched tab dirtied enough to cross the line, so ordinary use spent
/// most of its time lossy and the window read as softer than a local one even
/// between two machines at the same resolution and scale.
///
/// At 45% a window whose whole surface repaints (video, a game) still goes to
/// the encoder — the 742 Mbit/s case the design rules out — while partial
/// repaints, which is most of them, stay pixel-exact.
pub const VIDEO_AREA_THRESHOLD_MAX: f64 = 0.45;

/// The threshold a congested link falls back to.
///
/// The original value, and for the original reason: when the link cannot carry
/// lossless updates, sending them anyway does not preserve quality — it builds a
/// backlog, and the picture arrives late instead of arriving soft. Degrading is
/// the lesser harm.
pub const VIDEO_AREA_THRESHOLD_MIN: f64 = 0.15;

/// Frames the host may be ahead of the viewer before quality is stepped down.
///
/// Sent-minus-acked, in frames. Small because a window transfer is meant to feel
/// like a second monitor: two frames of backlog at 60 fps is already 33 ms of
/// staleness, and letting it grow trades the thing the feature exists for.
pub(super) const BACKLOG_HIGH: u32 = 3;

/// Backlog under which the link is considered to have room again.
///
/// Below `BACKLOG_HIGH`, so a link hovering at the limit holds its current
/// quality instead of oscillating between the two.
pub(super) const BACKLOG_LOW: u32 = 1;

/// Moves the lossless/lossy threshold with what the link carries.
///
/// A backlog over [`BACKLOG_HIGH`] steps toward [`VIDEO_AREA_THRESHOLD_MIN`]
/// (more of the window goes through the cheap lossy encoder) on that very
/// ack; a link caught up under [`BACKLOG_LOW`] steps back toward
/// [`VIDEO_AREA_THRESHOLD_MAX`] only after a streak of such acks, and never
/// within a few seconds of a step down — the shape the audio path's bitrate
/// and [`BitrateLadder`] have. Stepping both ways on single acks flapped
/// 0.35 → 0.25 → 0.15 → 0.25 → 0.35 in two seconds on a live view: every
/// raise sent the next repaint lossless, whose size was the next backlog.
///
/// This is what makes quality follow the link rather than a constant: on a link
/// with headroom even a game's full-window repaints can stay lossless, while a
/// congested one gives up fidelity before it gives up latency.
#[derive(Debug)]
pub struct ThresholdLadder {
    pct: u32,
    calm_acks: u32,
    hold_until: Option<std::time::Instant>,
}

impl Default for ThresholdLadder {
    fn default() -> Self {
        // Start optimistic: a link with headroom should not have to earn its
        // way up to full quality, and a congested one is corrected within a
        // few acks.
        Self {
            pct: Self::MAX_PCT,
            calm_acks: 0,
            hold_until: None,
        }
    }
}

impl ThresholdLadder {
    const MAX_PCT: u32 = (VIDEO_AREA_THRESHOLD_MAX * 100.0) as u32;
    const MIN_PCT: u32 = (VIDEO_AREA_THRESHOLD_MIN * 100.0) as u32;
    const STEP_PCT: u32 = 10;
    /// Caught-up acks in a row before one step up: two seconds at the rate
    /// viewers ack.
    const CALM_ACKS_TO_RAISE: u32 = 8;
    /// No raising for this long after a step down.
    const HOLD_AFTER_DROP: std::time::Duration = std::time::Duration::from_secs(4);

    /// Fold in one ack's backlog; the new percentage when it moved.
    pub fn observe(&mut self, gap: u32, now: std::time::Instant) -> Option<u32> {
        if gap > BACKLOG_HIGH {
            self.calm_acks = 0;
            self.hold_until = Some(now + Self::HOLD_AFTER_DROP);
            let next = self.pct.saturating_sub(Self::STEP_PCT).max(Self::MIN_PCT);
            return self.moved_to(next);
        }
        if gap >= BACKLOG_LOW {
            self.calm_acks = 0;
            return None;
        }
        self.calm_acks += 1;
        let held = self.hold_until.is_some_and(|until| now < until);
        if self.calm_acks < Self::CALM_ACKS_TO_RAISE || held {
            return None;
        }
        self.calm_acks = 0;
        self.moved_to((self.pct + Self::STEP_PCT).min(Self::MAX_PCT))
    }

    fn moved_to(&mut self, next: u32) -> Option<u32> {
        if next == self.pct {
            return None;
        }
        self.pct = next;
        Some(next)
    }
}

/// How long a frame may take from the queue to the viewer's screen and back
/// as an ack before it counts as backlog.
///
/// Counting every unacked frame read an ordinary 60 fps stream as congested:
/// encode, the wire, decode, the paint and the ack's return are 50–80 ms
/// between them, which is four or five frames in flight on a link with room
/// to spare — past [`BACKLOG_HIGH`] on every ack. The ladders then pulled the
/// bitrate to its floor precisely when motion was smoothest, and a slower
/// source with the same latency kept its quality. Measured in time, the same
/// latency means the same thing at any frame rate.
pub(super) const IN_FLIGHT_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

/// Queue times remembered, by `seq` modulo this; a frame whose slot has been
/// reused is counted as late, which a viewer this far behind is anyway.
pub(super) const SENT_TIMES_KEPT: usize = 256;

/// What the ack path keeps between acks, behind one lock.
#[derive(Debug, Default)]
pub(super) struct AckState {
    bitrate: BitrateLadder,
    threshold: ThresholdLadder,
    /// The newest `seq` any ack has carried.
    last_acked: Option<u32>,
}

/// The current lossless/lossy threshold and lossy bitrate, shared between the
/// session thread that receives acks and the capture thread that encodes
/// frames.
///
/// Atomics rather than a channel or a lock on the capture side: the capture
/// thread reads them once per frame and must never block on them, and a
/// missed update simply means one more frame at the previous quality. The
/// threshold is stored as the numerator of a percentage so it fits an integer
/// atomic.
#[derive(Debug)]
pub struct AdaptiveQuality {
    threshold_pct: std::sync::atomic::AtomicU32,
    /// Whether the viewer decodes lossless H.264; see
    /// [`VideoCaps`](cursio_protocol::VideoCaps). Starts `false`: a stream the
    /// viewer cannot decode shows nothing, while a lossy one it can is merely
    /// softer.
    lossless: std::sync::atomic::AtomicBool,
    /// The lossy stream's bitrate as a percentage of [`lossy_bitrate`] for its
    /// size, moved by [`BitrateLadder`] on the session thread and read by the
    /// capture thread per encoded frame.
    bitrate_pct: std::sync::atomic::AtomicU32,
    network_budget: std::sync::atomic::AtomicU32,
    network_congested: std::sync::atomic::AtomicBool,
    acks: std::sync::Mutex<AckState>,
    /// When the capture thread last encoded video, in milliseconds since
    /// `epoch`; how the ladder tells an idle source from a congested link.
    last_video_ms: std::sync::atomic::AtomicU64,
    /// When each recent frame was queued: `seq` in the high half, milliseconds
    /// since `epoch` in the low, in slot `seq % SENT_TIMES_KEPT`. Atomics for
    /// the same reason as the rest: the capture thread writes one per frame.
    sent_at: Box<[std::sync::atomic::AtomicU64]>,
    epoch: std::time::Instant,
}

impl Default for AdaptiveQuality {
    fn default() -> Self {
        Self {
            threshold_pct: std::sync::atomic::AtomicU32::new(ThresholdLadder::MAX_PCT),
            lossless: std::sync::atomic::AtomicBool::new(false),
            bitrate_pct: std::sync::atomic::AtomicU32::new(BitrateLadder::START_PCT),
            network_budget: std::sync::atomic::AtomicU32::new(u32::MAX),
            network_congested: std::sync::atomic::AtomicBool::new(false),
            acks: std::sync::Mutex::new(AckState::default()),
            last_video_ms: std::sync::atomic::AtomicU64::new(0),
            sent_at: (0..SENT_TIMES_KEPT)
                .map(|_| std::sync::atomic::AtomicU64::new(u64::MAX))
                .collect(),
            epoch: std::time::Instant::now(),
        }
    }
}

impl AdaptiveQuality {
    /// The bitrate a lossy stream of this size should run at now.
    pub fn bitrate(&self, width: u32, height: u32) -> u32 {
        let pct = self.bitrate_pct.load(std::sync::atomic::Ordering::Relaxed) as u64;
        let adaptive = (lossy_bitrate(width, height) as u64 * pct / 100) as u32;
        adaptive.min(
            self.network_budget
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub fn set_network_budget(&self, bits_per_second: u32, congested: bool) {
        self.network_budget.store(
            bits_per_second.max(64_000),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.network_congested
            .store(congested, std::sync::atomic::Ordering::Relaxed);
    }

    /// The capture thread just sent a video frame.
    pub fn note_video_sent(&self) {
        let ms = self.epoch.elapsed().as_millis() as u64;
        self.last_video_ms
            .store(ms.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// The capture thread just queued frame `seq`.
    pub fn note_queued(&self, seq: u32, now: std::time::Instant) {
        let ms = now.saturating_duration_since(self.epoch).as_millis() as u32;
        self.sent_at[seq as usize % SENT_TIMES_KEPT].store(
            (seq as u64) << 32 | ms as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Frames after `acked_seq` up to `sent_seq` the viewer has had
    /// [`IN_FLIGHT_GRACE`] to show and has not.
    ///
    /// A frame with no queue time on record counts as late: nothing but its
    /// age could excuse it.
    fn late_frames(&self, acked_seq: u32, sent_seq: u32, now: std::time::Instant) -> u32 {
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u32;
        let grace = IN_FLIGHT_GRACE.as_millis() as u32;
        let fresh = (acked_seq.wrapping_add(1)..=sent_seq)
            .rev()
            .take(SENT_TIMES_KEPT)
            .filter(|&seq| {
                let slot = self.sent_at[seq as usize % SENT_TIMES_KEPT]
                    .load(std::sync::atomic::Ordering::Relaxed);
                (slot >> 32) as u32 == seq && now_ms.wrapping_sub(slot as u32) < grace
            })
            .count() as u32;
        (sent_seq - acked_seq) - fresh
    }

    /// Whether video went out recently enough for acks to say anything about
    /// how much the link carries.
    fn video_active(&self) -> bool {
        let last = self
            .last_video_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let now = self.epoch.elapsed().as_millis() as u64;
        last != 0 && now.saturating_sub(last) < BitrateLadder::ACTIVE_WINDOW.as_millis() as u64
    }

    /// Whether the video path may use a lossless stream.
    pub fn lossless(&self) -> bool {
        self.lossless.load(std::sync::atomic::Ordering::Relaxed)
            && !self
                .network_congested
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Record what the viewer's decoder takes.
    pub fn set_peer_video(&self, caps: cursio_protocol::VideoCaps) {
        self.lossless
            .store(caps.lossless, std::sync::atomic::Ordering::Relaxed);
    }

    /// The threshold to judge this frame by.
    pub fn threshold(&self) -> f64 {
        self.threshold_pct
            .load(std::sync::atomic::Ordering::Relaxed) as f64
            / 100.0
    }

    /// Fold in an ack: the viewer has presented up to `acked_seq`, and the
    /// capture has queued up to `sent_seq`.
    ///
    /// The gap is counted in frames because the pipeline numbers only frames
    /// it actually queued (see [`CapturePipeline`](crate::frame_pipeline::CapturePipeline)), and only frames older
    /// than [`IN_FLIGHT_GRACE`] count. An ack that is not news
    /// is ignored: one no newer than an earlier ack says nothing the ladders
    /// have not already acted on, and one newer than anything sent belongs to
    /// a stream numbered before this capture. Neither is a reading of the
    /// link, and folding them in moved quality on noise.
    ///
    /// Returns the backlog and the new threshold when the threshold moved,
    /// for the caller to log.
    pub fn on_ack(&self, acked_seq: u32, sent_seq: u32) -> Option<(u32, f64)> {
        self.on_ack_at(acked_seq, sent_seq, std::time::Instant::now())
    }

    fn on_ack_at(
        &self,
        acked_seq: u32,
        sent_seq: u32,
        now: std::time::Instant,
    ) -> Option<(u32, f64)> {
        let active = self.video_active();
        let mut acks = self.acks.lock().unwrap_or_else(|p| p.into_inner());
        if acked_seq > sent_seq || acks.last_acked.is_some_and(|last| acked_seq <= last) {
            return None;
        }
        acks.last_acked = Some(acked_seq);
        let gap = self.late_frames(acked_seq, sent_seq, now);
        if let Some(pct) = acks.bitrate.observe(gap, active, now) {
            self.bitrate_pct
                .store(pct, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(
                gap,
                bitrate_pct = pct,
                "link changed; moving the video bitrate"
            );
        }
        let pct = acks.threshold.observe(gap, now)?;
        self.threshold_pct
            .store(pct, std::sync::atomic::Ordering::Relaxed);
        Some((gap, self.threshold()))
    }
}

/// The bitrate a lossy stream of a `width`x`height` source runs at.
///
/// A tenth of a bit per pixel per frame at 60 fps — about 12 Mbit/s for
/// 1080p, where H.264 High reads as clean on desktop content — within bounds
/// that keep a tiny window legible and a huge one off the cursor's link.
pub fn lossy_bitrate(width: u32, height: u32) -> u32 {
    const MIN: u64 = 2_000_000;
    const MAX: u64 = 40_000_000;
    let bits = (width as u64) * (height as u64) * 60 / 10;
    bits.clamp(MIN, MAX) as u32
}

/// Moves a lossy stream's bitrate with what the link carries.
///
/// The same signal as the tile threshold — frames sent but not yet presented —
/// read with more patience, because a bitrate change is more visible than a
/// threshold change. Down is fast and up is slow, the shape every congestion
/// controller has: a backlog costs latency on the cursor's own link *now*,
/// while headroom unused for a few seconds costs only sharpness. After a step
/// down the ladder holds before climbing again, so a link hovering at its
/// limit settles below it instead of oscillating across it.
///
/// Silence is not calm. A source that stopped changing sends no frames, so
/// every ack reads "caught up" — which says nothing about what the link would
/// carry, and climbing on it would meet the next burst of motion at a bitrate
/// the link never proved it takes. Only acks that arrive while video is
/// flowing count towards a step up; a backlog counts whenever it is seen.
///
/// Levels are percentages of [`lossy_bitrate`] rather than bits per second,
/// so the ladder needs no knowledge of the source's size and a resize keeps
/// the level the link earned.
#[derive(Debug)]
pub struct BitrateLadder {
    pct: u32,
    calm_acks: u32,
    hold_until: Option<std::time::Instant>,
}

impl Default for BitrateLadder {
    fn default() -> Self {
        Self {
            pct: Self::START_PCT,
            calm_acks: 0,
            hold_until: None,
        }
    }
}

impl BitrateLadder {
    pub const START_PCT: u32 = 100;
    /// A fifth of the starting rate — below it H.264 on a desktop smears text
    /// past reading, and a link that cannot carry this is better served by
    /// the tile threshold dropping further.
    pub const MIN_PCT: u32 = 20;
    /// Three and a half times the start: about 43 Mbit/s at 1080p, where
    /// further bits stop being visible on screen content.
    pub const MAX_PCT: u32 = 350;
    /// Acks while flowing and caught up before one step up (a few seconds at
    /// the rate viewers ack).
    const CALM_ACKS_TO_CLIMB: u32 = 6;
    /// No climbing for this long after a step down.
    const HOLD_AFTER_DROP: std::time::Duration = std::time::Duration::from_secs(4);
    /// Video sent within this long counts as flowing.
    pub const ACTIVE_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

    /// Fold in one ack's backlog; the new level when it moved.
    pub fn observe(&mut self, gap: u32, active: bool, now: std::time::Instant) -> Option<u32> {
        if gap > BACKLOG_HIGH {
            self.calm_acks = 0;
            self.hold_until = Some(now + Self::HOLD_AFTER_DROP);
            let next = (self.pct * 7 / 10).max(Self::MIN_PCT);
            return self.moved_to(next);
        }
        if !active || gap > BACKLOG_LOW {
            self.calm_acks = 0;
            return None;
        }
        self.calm_acks += 1;
        let held = self.hold_until.is_some_and(|until| now < until);
        if self.calm_acks < Self::CALM_ACKS_TO_CLIMB || held {
            return None;
        }
        self.calm_acks = 0;
        let next = (self.pct * 115 / 100).min(Self::MAX_PCT);
        self.moved_to(next)
    }

    fn moved_to(&mut self, next: u32) -> Option<u32> {
        if next == self.pct {
            return None;
        }
        self.pct = next;
        Some(next)
    }
}

/// Whether a change covering `dirty` of a `width`x`height` window should go
/// through the video encoder rather than as a lossless tile.
pub fn should_use_video(dirty: Rect, width: u32, height: u32, threshold: f64) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    let changed = (dirty.width as f64 * dirty.height as f64) / (width as f64 * height as f64);
    changed >= threshold
}

/// Pick the codec for a tile.
///
/// The first cut has only the two lossless options, so the choice is about
/// *cost*, not fidelity: tiny tiles aren't worth invoking a compressor for
/// (the zstd frame header alone can exceed the payload), everything larger
/// compresses well because screen content is full of flat runs. Once the lossy
/// encoders exist this is where "is this region text or video?" gets decided.
pub fn pick_codec(tile_len: usize) -> TileCodec {
    const COMPRESS_ABOVE: usize = 1024;
    if tile_len > COMPRESS_ABOVE {
        TileCodec::LosslessBgra8888
    } else {
        TileCodec::RawBgra8888
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_budget_caps_the_encoder_and_congestion_disables_lossless() {
        let quality = AdaptiveQuality::default();
        quality.set_peer_video(cursio_protocol::VideoCaps { lossless: true });
        assert!(quality.lossless());
        quality.set_network_budget(800_000, true);
        assert_eq!(quality.bitrate(1920, 1080), 800_000);
        assert!(!quality.lossless());
        quality.set_network_budget(8_000_000, false);
        assert_eq!(quality.bitrate(1920, 1080), 8_000_000);
        assert!(quality.lossless());
    }

    #[test]
    fn small_changes_stay_on_the_lossless_path() {
        // A caret or a typed character: a handful of pixels on a 1080p window.
        // Sending these through a video encoder would mean coding a full frame
        // to convey a few hundred bytes of change.
        let caret = Rect {
            x: 100,
            y: 200,
            width: 8,
            height: 20,
        };
        assert!(!should_use_video(
            caret,
            1920,
            1080,
            VIDEO_AREA_THRESHOLD_MAX
        ));
    }

    #[test]
    fn large_changes_go_through_video() {
        // Scrolling or video playback dirties most of the window — the case
        // where zstd collapses to ~2.9x and costs 742 Mbit/s.
        let most = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 900,
        };
        assert!(should_use_video(most, 1920, 1080, VIDEO_AREA_THRESHOLD_MAX));
    }

    /// Acks a capture with a chosen backlog, each ack newer than the last.
    #[derive(Default)]
    struct Viewer {
        sent: u32,
    }

    impl Viewer {
        fn ack(
            &mut self,
            q: &AdaptiveQuality,
            gap: u32,
            now: std::time::Instant,
        ) -> Option<(u32, f64)> {
            self.sent += gap + 1;
            q.on_ack_at(self.sent - gap, self.sent, now)
        }
    }

    /// A link that keeps up earns full quality; one that falls behind gives it
    /// up before it gives up latency.
    #[test]
    fn quality_follows_what_the_link_is_carrying() {
        let q = AdaptiveQuality::default();
        let mut viewer = Viewer::default();
        let start = std::time::Instant::now();
        // Starts optimistic, so a fast link pays nothing to get there.
        assert_eq!(q.threshold(), VIDEO_AREA_THRESHOLD_MAX);

        // A backlog steps quality down, once per ack rather than all at once —
        // a single late ack should not collapse the picture.
        let (_, after_one) = viewer
            .ack(&q, BACKLOG_HIGH + 1, start)
            .expect("a backlog moves it");
        assert!(after_one < VIDEO_AREA_THRESHOLD_MAX);

        // Sustained congestion bottoms out at the floor and stays there.
        for _ in 0..20 {
            viewer.ack(&q, BACKLOG_HIGH + 1, start);
        }
        assert_eq!(q.threshold(), VIDEO_AREA_THRESHOLD_MIN);
        assert!(
            viewer.ack(&q, BACKLOG_HIGH + 1, start).is_none(),
            "already at the floor: nothing to report"
        );

        // Recovery climbs back to full quality without a restart.
        let later = start + std::time::Duration::from_secs(10);
        for _ in 0..30 {
            viewer.ack(&q, 0, later);
        }
        assert_eq!(q.threshold(), VIDEO_AREA_THRESHOLD_MAX);
    }

    /// Between the two marks the threshold holds, so a link hovering at the
    /// limit does not oscillate between sharp and soft every few frames.
    #[test]
    fn a_link_at_the_limit_holds_its_quality() {
        let mut ladder = ThresholdLadder::default();
        let now = std::time::Instant::now();
        for gap in (BACKLOG_LOW..=BACKLOG_HIGH).cycle().take(40) {
            assert_eq!(
                ladder.observe(gap, now),
                None,
                "gap {gap} is neither congested nor clear"
            );
        }
    }

    /// The live trace: acks four times a second reading 4, 4, 7, then 0, 0,
    /// then 9, 15 took the threshold 0.35 → 0.25 → 0.15 → 0.25 → 0.35 → 0.25
    /// → 0.15 in under three seconds. A raise now waits out a calm streak and
    /// a hold after the last drop, so the same acks only ever step down.
    #[test]
    fn a_brief_lull_between_backlogs_does_not_raise_the_threshold() {
        let mut ladder = ThresholdLadder::default();
        let start = std::time::Instant::now();
        let quarter = std::time::Duration::from_millis(250);
        let mut moves = Vec::new();
        for (i, gap) in [4, 4, 7, 4, 0, 0, 0, 0, 9, 15, 0, 0]
            .into_iter()
            .enumerate()
        {
            if let Some(pct) = ladder.observe(gap, start + quarter * i as u32) {
                moves.push(pct);
            }
        }
        assert_eq!(moves, vec![35, 25, 15]);
    }

    #[test]
    fn the_threshold_climbs_one_step_per_calm_streak_once_the_hold_is_over() {
        let mut ladder = ThresholdLadder::default();
        let start = std::time::Instant::now();
        ladder.observe(BACKLOG_HIGH + 1, start);
        let during_hold = start + std::time::Duration::from_secs(1);
        for _ in 0..(ThresholdLadder::CALM_ACKS_TO_RAISE * 2) {
            assert_eq!(ladder.observe(0, during_hold), None);
        }
        // A streak that ran through the hold has already shown the link calm.
        let after = start + ThresholdLadder::HOLD_AFTER_DROP;
        assert_eq!(ladder.observe(0, after), Some(ThresholdLadder::MAX_PCT));

        ladder.observe(BACKLOG_HIGH + 1, after);
        let later = after + ThresholdLadder::HOLD_AFTER_DROP;
        for _ in 1..ThresholdLadder::CALM_ACKS_TO_RAISE {
            assert_eq!(ladder.observe(0, later), None);
        }
        assert_eq!(ladder.observe(0, later), Some(ThresholdLadder::MAX_PCT));
    }

    /// Five frames a 60 fps stream queued in the last 80 ms are what an
    /// ordinary pipeline has in flight, not a backlog.
    #[test]
    fn frames_still_in_flight_are_not_backlog() {
        let q = AdaptiveQuality::default();
        let start = std::time::Instant::now();
        for seq in 1..=6 {
            q.note_queued(seq, start + std::time::Duration::from_millis(16) * seq);
        }
        let now = start + std::time::Duration::from_millis(100);
        assert_eq!(q.on_ack_at(1, 6, now), None);
        assert_eq!(
            q.bitrate_pct.load(std::sync::atomic::Ordering::Relaxed),
            BitrateLadder::START_PCT
        );
    }

    /// The same frames still unshown well past the grace are a backlog.
    #[test]
    fn frames_left_unshown_past_the_grace_are_backlog() {
        let q = AdaptiveQuality::default();
        let start = std::time::Instant::now();
        for seq in 1..=6 {
            q.note_queued(seq, start + std::time::Duration::from_millis(16) * seq);
        }
        let now = start + std::time::Duration::from_millis(300);
        assert_eq!(q.on_ack_at(1, 6, now).map(|(gap, _)| gap), Some(5));
    }

    /// An ack that is not news moves nothing: a repeat of the last one, an
    /// older one, or one numbered past anything this capture sent (left over
    /// from the stream before it).
    #[test]
    fn acks_that_are_not_news_leave_quality_alone() {
        let q = AdaptiveQuality::default();
        let now = std::time::Instant::now();
        assert!(
            q.on_ack_at(10, 20, now).is_some(),
            "a real backlog moves it"
        );
        let after = q.threshold();

        assert!(q.on_ack_at(10, 40, now).is_none());
        assert!(q.on_ack_at(5, 40, now).is_none());
        assert!(q.on_ack_at(90, 40, now).is_none());
        assert_eq!(q.threshold(), after);
        assert_eq!(q.bitrate_pct.load(std::sync::atomic::Ordering::Relaxed), 70);
    }

    /// The point of adapting: on a link with headroom, even a full-window
    /// repaint — a game, a video — can stay lossless, which a fixed threshold
    /// could never allow.
    #[test]
    fn a_clear_link_keeps_even_large_repaints_lossless() {
        let q = AdaptiveQuality::default();
        let most_of_the_window = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 430, // ~40% of 1080
        };
        assert!(!should_use_video(
            most_of_the_window,
            1920,
            1080,
            q.threshold()
        ));

        // The same repaint on a congested link goes lossy instead, trading
        // fidelity to keep the picture current.
        let mut viewer = Viewer::default();
        for _ in 0..20 {
            viewer.ack(&q, BACKLOG_HIGH + 1, std::time::Instant::now());
        }
        assert!(should_use_video(
            most_of_the_window,
            1920,
            1080,
            q.threshold()
        ));
    }

    #[test]
    fn video_threshold_is_where_it_claims_to_be() {
        // Just under and just over 45% of a 1000x1000 window.
        let under = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 449,
        };
        let over = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 451,
        };
        assert!(!should_use_video(
            under,
            1000,
            1000,
            VIDEO_AREA_THRESHOLD_MAX
        ));
        assert!(should_use_video(over, 1000, 1000, VIDEO_AREA_THRESHOLD_MAX));

        // The case the raised threshold exists for: a repaint big enough to be
        // ordinary — a scrolled page, a switched tab — stays lossless, so text
        // does not go through 4:2:0 chroma for a change of this size.
        let scrolled = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 300,
        };
        assert!(
            !should_use_video(scrolled, 1000, 1000, VIDEO_AREA_THRESHOLD_MAX),
            "a third of the window repainting must not force the lossy path"
        );
    }

    #[test]
    fn zero_sized_window_never_picks_video() {
        // Guards the division: a zero-area window would otherwise produce NaN.
        let any = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        assert!(!should_use_video(any, 0, 0, VIDEO_AREA_THRESHOLD_MAX));
    }

    #[test]
    fn tiny_tiles_ride_raw_and_larger_ones_compress() {
        // Below the threshold a zstd frame header can exceed the payload, so
        // compressing would make the tile *bigger*.
        assert_eq!(pick_codec(64), TileCodec::RawBgra8888);
        assert_eq!(pick_codec(64 * 1024), TileCodec::LosslessBgra8888);
    }

    use std::time::{Duration, Instant};

    #[test]
    fn a_backlog_drops_the_bitrate_at_once() {
        let mut ladder = BitrateLadder::default();
        assert_eq!(ladder.observe(10, true, Instant::now()), Some(70));
    }

    #[test]
    fn a_calm_flowing_link_climbs_only_after_several_acks() {
        let mut ladder = BitrateLadder::default();
        let now = Instant::now();
        for _ in 0..5 {
            assert_eq!(ladder.observe(0, true, now), None);
        }
        assert_eq!(ladder.observe(0, true, now), Some(115));
    }

    #[test]
    fn an_idle_source_never_climbs() {
        let mut ladder = BitrateLadder::default();
        let now = Instant::now();
        for _ in 0..50 {
            assert_eq!(
                ladder.observe(0, false, now),
                None,
                "no frames because nothing changed says nothing about the link"
            );
        }
    }

    #[test]
    fn after_a_drop_the_ladder_holds_before_climbing_again() {
        let mut ladder = BitrateLadder::default();
        let start = Instant::now();
        ladder.observe(10, true, start);
        for _ in 0..12 {
            assert_eq!(
                ladder.observe(0, true, start + Duration::from_secs(1)),
                None
            );
        }
        let later = start + Duration::from_secs(5);
        let climbed = (0..6).filter_map(|_| ladder.observe(0, true, later)).last();
        assert_eq!(climbed, Some(80));
    }

    #[test]
    fn the_bitrate_stays_within_its_floor_and_ceiling() {
        let mut ladder = BitrateLadder::default();
        let now = Instant::now();
        for _ in 0..50 {
            ladder.observe(10, true, now);
        }
        assert_eq!(ladder.pct, BitrateLadder::MIN_PCT);
        let mut ladder = BitrateLadder::default();
        for _ in 0..500 {
            ladder.observe(0, true, now);
        }
        assert_eq!(ladder.pct, BitrateLadder::MAX_PCT);
    }

    #[test]
    fn a_middling_backlog_holds_the_bitrate() {
        let mut ladder = BitrateLadder::default();
        let now = Instant::now();
        for _ in 0..20 {
            assert_eq!(ladder.observe(2, true, now), None);
        }
    }
}
