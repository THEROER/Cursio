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

/// Above this fraction of the window's pixels, a change is sent through the
/// video encoder instead of as a lossless tile.
///
/// The two paths win in opposite regimes, per the 2026-07-20 measurements:
///
/// - A *small* dirty rect (caret, a typed character, a hover highlight) is a few
///   hundred bytes of zstd and needs no encoder state — cheaper and simpler than
///   pushing a whole frame through H.264.
/// - A *large* one degenerates: zstd manages only ~2.9x on real content, so a
///   near-full-window update costs ~1.5 MiB, while the video encoder sends the
///   same change for ~0.1–280 KiB because it codes the *difference* from the
///   previous frame.
///
/// 15% is deliberately low. The video path's cost barely depends on the dirty
/// area (it always encodes the full frame), so erring towards it is cheap, while
/// erring the other way risks the 742 Mbit/s case the design rules out.
pub const VIDEO_AREA_THRESHOLD: f64 = 0.15;

/// Whether a change covering `dirty` of a `width`x`height` window should go
/// through the video encoder rather than as a lossless tile.
pub fn should_use_video(dirty: Rect, width: u32, height: u32) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    let changed = (dirty.width as f64 * dirty.height as f64) / (width as f64 * height as f64);
    changed >= VIDEO_AREA_THRESHOLD
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
        assert!(!should_use_video(caret, 1920, 1080));
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
        assert!(should_use_video(most, 1920, 1080));
    }

    #[test]
    fn video_threshold_is_where_it_claims_to_be() {
        // Just under and just over 15% of a 1000x1000 window.
        let under = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 149,
        };
        let over = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 151,
        };
        assert!(!should_use_video(under, 1000, 1000));
        assert!(should_use_video(over, 1000, 1000));
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
        assert!(!should_use_video(any, 0, 0));
    }

    #[test]
    fn tiny_tiles_ride_raw_and_larger_ones_compress() {
        // Below the threshold a zstd frame header can exceed the payload, so
        // compressing would make the tile *bigger*.
        assert_eq!(pick_codec(64), TileCodec::RawBgra8888);
        assert_eq!(pick_codec(64 * 1024), TileCodec::LosslessBgra8888);
    }
}
