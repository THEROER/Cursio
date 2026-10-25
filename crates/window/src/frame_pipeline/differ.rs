//! What changed between two captured frames: the bounding box of the
//! changed pixels, and pulling exactly that rectangle out of the frame.

use cursio_protocol::Rect;

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

    /// The last frame seen, with its size, or `None` before the first.
    pub fn last(&self) -> Option<(&[u8], u32, u32)> {
        (!self.prev.is_empty()).then_some((self.prev.as_slice(), self.width, self.height))
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

#[cfg(test)]
mod tests {
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
}
