//! Platform-neutral frame pipeline: diffing, codec routing, tile selection.
//!
//! Both capture backends — the Linux portal/PipeWire one and the Windows
//! Graphics Capture one — receive the same thing: a full BGRA frame, every
//! tick, with no indication of what changed. Everything they do with that frame
//! before it becomes a [`WindowFrame`] is identical, so it lives here rather
//! than being written twice (and drifting).
//!
//! The pipeline is: diff against the previous frame to find the changed
//! bounding box, then decide whether that change rides a lossless tile or the
//! video stream.

use cursio_protocol::{Rect, TileCodec, WindowFrame};
use tokio::sync::mpsc;

/// Tracks the last full frame so each new capture can be reduced to what
/// actually changed.
///
/// PipeWire hands us a **whole** frame every tick — it has no idea which pixels
/// moved. Sending that verbatim is exactly the 8.3 MiB/frame case the design
/// rules out, and it would burn the link on a window that isn't even changing.
/// So we keep the previous frame and diff against it, emitting only the
/// bounding box of the difference. That is the "dirty regions" lever from
/// `docs/window-transfer-design.md`: a static window collapses to zero frames,
/// a blinking cursor to a few hundred bytes.
///
/// A bounding box (rather than a set of precise sub-rects) is the deliberate
/// first cut: one contiguous rect is cheap to compute, cheap to encode, and
/// already captures the dominant win. Splitting a scattered diff into several
/// tiles is a later refinement the protocol already allows.
pub struct FrameDiffer {
    /// Previous frame, tightly packed BGRA (`width * height * 4`), or empty
    /// before the first frame has been seen.
    prev: Vec<u8>,
    width: u32,
    height: u32,
}

impl Default for FrameDiffer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDiffer {
    pub fn new() -> Self {
        Self {
            prev: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    /// Forget the reference frame so the next update covers the full surface.
    ///
    /// Required after backpressure drops an update: the viewer did not receive
    /// the state represented by `prev`, so later deltas must not depend on it.
    pub fn reset(&mut self) {
        self.prev.clear();
        self.width = 0;
        self.height = 0;
    }

    /// Compare `frame` (tightly packed BGRA of `width * height`) against the
    /// previous one and return the changed bounding box, or `None` if nothing
    /// changed at all.
    ///
    /// A size change forces a full-frame rect: the viewer needs complete
    /// contents for the new geometry, and the old buffer isn't comparable.
    pub fn diff(&mut self, frame: &[u8], width: u32, height: u32) -> Option<Rect> {
        let full = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let resized = width != self.width || height != self.height;
        if resized || self.prev.len() != frame.len() {
            self.prev = frame.to_vec();
            self.width = width;
            self.height = height;
            return Some(full);
        }

        let row_bytes = (width as usize) * 4;
        let (mut min_x, mut min_y) = (u32::MAX, u32::MAX);
        let (mut max_x, mut max_y) = (0u32, 0u32);

        for y in 0..height as usize {
            let start = y * row_bytes;
            let row_new = &frame[start..start + row_bytes];
            let row_old = &self.prev[start..start + row_bytes];
            if row_new == row_old {
                continue;
            }
            // Row differs: narrow down which pixels within it did, so a caret
            // blink doesn't dirty the window's whole width. Work in whole
            // pixels (4-byte chunks), never raw byte offsets — a partial pixel
            // would produce a rect that slices a BGRA quad in half.
            let first_byte = row_new
                .iter()
                .zip(row_old)
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            let last_byte = row_bytes
                - 1
                - row_new
                    .iter()
                    .rev()
                    .zip(row_old.iter().rev())
                    .position(|(a, b)| a != b)
                    .unwrap_or(0);

            min_x = min_x.min((first_byte / 4) as u32);
            max_x = max_x.max((last_byte / 4) as u32);
            min_y = min_y.min(y as u32);
            max_y = max_y.max(y as u32);
        }

        if min_y == u32::MAX {
            return None; // identical frame — the static-window case
        }

        self.prev.copy_from_slice(frame);
        Some(Rect {
            x: min_x,
            y: min_y,
            width: max_x - min_x + 1,
            height: max_y - min_y + 1,
        })
    }

    /// Copy `dirty` out of a tightly packed BGRA `frame` into its own tile.
    pub fn extract(frame: &[u8], frame_width: u32, dirty: Rect) -> Vec<u8> {
        let row_bytes = (frame_width as usize) * 4;
        let tile_row = (dirty.width as usize) * 4;
        let mut out = Vec::with_capacity(tile_row * dirty.height as usize);
        for y in 0..dirty.height as usize {
            let src = (dirty.y as usize + y) * row_bytes + (dirty.x as usize) * 4;
            out.extend_from_slice(&frame[src..src + tile_row]);
        }
        out
    }
}

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
/// Which fraction is not fixed, though — see [`next_threshold`]. This is the
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

/// How far the threshold moves per adjustment.
const THRESHOLD_STEP: f64 = 0.10;

/// Frames the host may be ahead of the viewer before quality is stepped down.
///
/// Sent-minus-acked, in frames. Small because a window transfer is meant to feel
/// like a second monitor: two frames of backlog at 60 fps is already 33 ms of
/// staleness, and letting it grow trades the thing the feature exists for.
const BACKLOG_HIGH: u32 = 3;

/// Backlog under which the link is considered to have room again.
///
/// Below `BACKLOG_HIGH`, so a link hovering at the limit holds its current
/// quality instead of oscillating between the two.
const BACKLOG_LOW: u32 = 1;

/// Decide the next lossless/lossy threshold from the current one and the
/// sent-vs-acked backlog.
///
/// Hysteretic, the same shape as the audio path's `next_bitrate`: a backlog over
/// [`BACKLOG_HIGH`] steps toward [`VIDEO_AREA_THRESHOLD_MIN`] (more of the window
/// goes through the cheap lossy encoder), a link caught up under [`BACKLOG_LOW`]
/// steps back toward [`VIDEO_AREA_THRESHOLD_MAX`], and in between it holds.
///
/// This is what makes quality follow the link rather than a constant: on a link
/// with headroom even a game's full-window repaints can stay lossless, while a
/// congested one gives up fidelity before it gives up latency.
/// The current lossless/lossy threshold, shared between the session thread that
/// receives acks and the capture thread that encodes frames.
///
/// An atomic rather than a channel or a lock: the capture thread reads it once
/// per frame and must never block on it, and a missed update simply means one
/// more frame at the previous quality. Stored as the numerator of a percentage
/// so it fits an integer atomic.
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
    ladder: std::sync::Mutex<BitrateLadder>,
    /// When the capture thread last encoded video, in milliseconds since
    /// `epoch`; how the ladder tells an idle source from a congested link.
    last_video_ms: std::sync::atomic::AtomicU64,
    epoch: std::time::Instant,
}

impl Default for AdaptiveQuality {
    fn default() -> Self {
        // Start optimistic: a link with headroom should not have to earn its
        // way up to full quality, and a congested one is corrected within a few
        // acks.
        Self {
            threshold_pct: std::sync::atomic::AtomicU32::new(
                (VIDEO_AREA_THRESHOLD_MAX * 100.0) as u32,
            ),
            lossless: std::sync::atomic::AtomicBool::new(false),
            bitrate_pct: std::sync::atomic::AtomicU32::new(BitrateLadder::START_PCT),
            ladder: std::sync::Mutex::new(BitrateLadder::default()),
            last_video_ms: std::sync::atomic::AtomicU64::new(0),
            epoch: std::time::Instant::now(),
        }
    }
}

impl AdaptiveQuality {
    /// The bitrate a lossy stream of this size should run at now.
    pub fn bitrate(&self, width: u32, height: u32) -> u32 {
        let pct = self.bitrate_pct.load(std::sync::atomic::Ordering::Relaxed) as u64;
        (lossy_bitrate(width, height) as u64 * pct / 100) as u32
    }

    /// The capture thread just sent a video frame.
    pub fn note_video_sent(&self) {
        let ms = self.epoch.elapsed().as_millis() as u64;
        self.last_video_ms
            .store(ms.max(1), std::sync::atomic::Ordering::Relaxed);
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

    /// Fold in what the viewer has acked: `gap` frames still unpresented.
    ///
    /// Returns the new threshold when it moved, for the caller to log.
    pub fn observe(&self, gap: u32) -> Option<f64> {
        let active = self.video_active();
        if let Ok(mut ladder) = self.ladder.lock() {
            if let Some(pct) = ladder.observe(gap, active, std::time::Instant::now()) {
                self.bitrate_pct
                    .store(pct, std::sync::atomic::Ordering::Relaxed);
                tracing::info!(
                    gap,
                    bitrate_pct = pct,
                    "link changed; moving the video bitrate"
                );
            }
        }
        let current = self.threshold();
        let next = next_threshold(current, gap);
        if (next - current).abs() < f64::EPSILON {
            return None;
        }
        self.threshold_pct.store(
            (next * 100.0).round() as u32,
            std::sync::atomic::Ordering::Relaxed,
        );
        Some(next)
    }
}

pub fn next_threshold(current: f64, gap: u32) -> f64 {
    if gap > BACKLOG_HIGH {
        (current - THRESHOLD_STEP).max(VIDEO_AREA_THRESHOLD_MIN)
    } else if gap < BACKLOG_LOW {
        (current + THRESHOLD_STEP).min(VIDEO_AREA_THRESHOLD_MAX)
    } else {
        current
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameQueueError {
    Full,
    Closed,
}

/// Enqueue every packet belonging to one encoded update, or none of them.
///
/// H.264 encoders may return several packets for a single input frame. Sending
/// only the first packet before discovering that the bounded queue is full
/// would corrupt the decoder state, so capacity is reserved atomically.
pub fn try_queue_frames(
    tx: &mpsc::Sender<WindowFrame>,
    frames: Vec<WindowFrame>,
) -> Result<(), FrameQueueError> {
    if frames.is_empty() {
        return Ok(());
    }
    let permits = tx
        .try_reserve_many(frames.len())
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => FrameQueueError::Full,
            mpsc::error::TrySendError::Closed(()) => FrameQueueError::Closed,
        })?;
    for (permit, frame) in permits.zip(frames) {
        permit.send(frame);
    }
    Ok(())
}

#[cfg(test)]
mod differ_tests {
    use super::*;

    /// A `w`x`h` BGRA frame filled with one colour.
    fn solid(w: u32, h: u32, c: u8) -> Vec<u8> {
        vec![c; (w * h * 4) as usize]
    }

    /// Overwrite one pixel in a tightly packed BGRA frame.
    fn put(frame: &mut [u8], w: u32, x: u32, y: u32, c: u8) {
        let i = ((y * w + x) * 4) as usize;
        frame[i..i + 4].copy_from_slice(&[c, c, c, 0xFF]);
    }

    #[test]
    fn first_frame_is_always_full() {
        let mut d = FrameDiffer::new();
        let f = solid(8, 4, 0x10);
        assert_eq!(
            d.diff(&f, 8, 4),
            Some(Rect {
                x: 0,
                y: 0,
                width: 8,
                height: 4
            })
        );
    }

    #[test]
    fn identical_frame_is_not_sent_at_all() {
        let mut d = FrameDiffer::new();
        let f = solid(8, 4, 0x10);
        d.diff(&f, 8, 4);
        // The static-window case: no rect means no frame on the wire.
        assert_eq!(d.diff(&f, 8, 4), None);
    }

    #[test]
    fn single_changed_pixel_yields_a_1x1_rect() {
        let mut d = FrameDiffer::new();
        let f = solid(8, 4, 0x10);
        d.diff(&f, 8, 4);

        let mut f2 = f.clone();
        put(&mut f2, 8, 5, 2, 0xF0);
        assert_eq!(
            d.diff(&f2, 8, 4),
            Some(Rect {
                x: 5,
                y: 2,
                width: 1,
                height: 1
            })
        );
    }

    #[test]
    fn scattered_changes_collapse_to_their_bounding_box() {
        let mut d = FrameDiffer::new();
        let f = solid(8, 4, 0x10);
        d.diff(&f, 8, 4);

        let mut f2 = f.clone();
        put(&mut f2, 8, 1, 1, 0xF0);
        put(&mut f2, 8, 6, 3, 0xF0);
        assert_eq!(
            d.diff(&f2, 8, 4),
            Some(Rect {
                x: 1,
                y: 1,
                width: 6,
                height: 3
            })
        );
    }

    #[test]
    fn resize_forces_a_full_frame() {
        let mut d = FrameDiffer::new();
        d.diff(&solid(8, 4, 0x10), 8, 4);
        let bigger = solid(16, 8, 0x10);
        assert_eq!(
            d.diff(&bigger, 16, 8),
            Some(Rect {
                x: 0,
                y: 0,
                width: 16,
                height: 8
            })
        );
    }

    #[test]
    fn reset_forces_the_next_frame_to_be_full() {
        let mut d = FrameDiffer::new();
        let frame = solid(8, 4, 0x10);
        d.diff(&frame, 8, 4);
        assert_eq!(d.diff(&frame, 8, 4), None);
        d.reset();
        assert_eq!(
            d.diff(&frame, 8, 4),
            Some(Rect {
                x: 0,
                y: 0,
                width: 8,
                height: 4,
            })
        );
    }

    #[test]
    fn extract_pulls_exactly_the_dirty_rect() {
        // 4x2 frame; mark the pixel at (2,1) and extract just that 1x1.
        let mut f = solid(4, 2, 0x10);
        put(&mut f, 4, 2, 1, 0xAB);
        let tile = FrameDiffer::extract(
            &f,
            4,
            Rect {
                x: 2,
                y: 1,
                width: 1,
                height: 1,
            },
        );
        assert_eq!(tile, vec![0xAB, 0xAB, 0xAB, 0xFF]);
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

    /// A link that keeps up earns full quality; one that falls behind gives it
    /// up before it gives up latency.
    #[test]
    fn quality_follows_what_the_link_is_carrying() {
        let q = AdaptiveQuality::default();
        // Starts optimistic, so a fast link pays nothing to get there.
        assert_eq!(q.threshold(), VIDEO_AREA_THRESHOLD_MAX);

        // A backlog steps quality down, once per observation rather than all at
        // once — a single late ack should not collapse the picture.
        let after_one = q.observe(BACKLOG_HIGH + 1).expect("a backlog moves it");
        assert!(after_one < VIDEO_AREA_THRESHOLD_MAX);

        // Sustained congestion bottoms out at the floor and stays there.
        for _ in 0..20 {
            q.observe(BACKLOG_HIGH + 1);
        }
        assert_eq!(q.threshold(), VIDEO_AREA_THRESHOLD_MIN);
        assert!(
            q.observe(BACKLOG_HIGH + 1).is_none(),
            "already at the floor: nothing to report"
        );

        // Recovery climbs back to full quality without a restart.
        for _ in 0..20 {
            q.observe(0);
        }
        assert_eq!(q.threshold(), VIDEO_AREA_THRESHOLD_MAX);
    }

    /// Between the two marks the threshold holds, so a link hovering at the
    /// limit does not oscillate between sharp and soft every few frames.
    #[test]
    fn a_link_at_the_limit_holds_its_quality() {
        let steady = 0.35;
        for gap in BACKLOG_LOW..=BACKLOG_HIGH {
            assert_eq!(
                next_threshold(steady, gap),
                steady,
                "gap {gap} is neither congested nor clear"
            );
        }
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
        for _ in 0..20 {
            q.observe(BACKLOG_HIGH + 1);
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
    fn packet_batch_is_enqueued_atomically() {
        fn packet(seq: u32) -> WindowFrame {
            WindowFrame {
                id: 1,
                seq,
                dirty: Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                codec: TileCodec::LossyVideo,
                data: vec![seq as u8],
            }
        }

        let (tx, mut rx) = mpsc::channel(2);
        assert_eq!(try_queue_frames(&tx, vec![packet(1), packet(2)]), Ok(()));
        assert_eq!(
            try_queue_frames(&tx, vec![packet(3)]),
            Err(FrameQueueError::Full)
        );
        assert_eq!(rx.try_recv().unwrap().seq, 1);
        assert_eq!(rx.try_recv().unwrap().seq, 2);
        assert!(rx.try_recv().is_err());
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
