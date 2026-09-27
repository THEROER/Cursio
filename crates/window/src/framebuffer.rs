//! The viewer's assembled window image, shared by every surrogate backend.
//!
//! Frames carry only dirty rects, so a viewer must keep the whole picture and
//! blit each update into it. That logic is identical on GTK and Win32 — only
//! the final step (handing the pixels to the toolkit) differs — so it lives
//! here rather than being written twice.

use cursio_protocol::{Rect, TileCodec};

use crate::WindowError;

/// The full-window BGRA image the surrogate draws, assembled from tiles.
///
/// Tightly packed (`width * 4` per row, no padding), matching the wire layout
/// and what both `gdk::MemoryTexture` and `StretchDIBits` expect.
pub struct Framebuffer {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            pixels: vec![0; (width as usize) * (height as usize) * 4],
            width,
            height,
        }
    }

    /// Grow/shrink to a new geometry, dropping old contents.
    ///
    /// Only for a size learnt from a video frame (see [`geometry_of`]), which
    /// replaces the discarded pixels wholesale.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.pixels = vec![0; (width as usize) * (height as usize) * 4];
        self.width = width;
        self.height = height;
    }

    /// Grow to at least `width`x`height`, keeping what is already drawn.
    ///
    /// Unlike [`Self::resize`], which is the answer to a full frame that is
    /// about to overwrite everything anyway, this serves the case where only a
    /// tile told us the window got bigger: the rest of the picture is still the
    /// newest we have, and clearing it would flash the window black on every
    /// step of a drag-resize.
    pub fn grow_to(&mut self, width: u32, height: u32) {
        let (w, h) = (self.width.max(width), self.height.max(height));
        if w == self.width && h == self.height {
            return;
        }
        let mut grown = vec![0u8; (w as usize) * (h as usize) * 4];
        let old_row = (self.width as usize) * 4;
        let new_row = (w as usize) * 4;
        for y in 0..self.height as usize {
            let src = y * old_row;
            let dst = y * new_row;
            grown[dst..dst + old_row].copy_from_slice(&self.pixels[src..src + old_row]);
        }
        self.pixels = grown;
        self.width = w;
        self.height = h;
    }

    /// Blit one decoded raw-BGRA tile at `dirty`.
    ///
    /// Rejects a tile that would land outside the framebuffer rather than
    /// clipping it: out-of-bounds means the host and viewer disagree about the
    /// window's size, and silently drawing part of it would leave a plausible
    /// but wrong image on screen.
    pub fn blit(&mut self, dirty: Rect, raw: &[u8]) -> Result<(), WindowError> {
        let (fw, fh) = (self.width, self.height);
        if dirty.x + dirty.width > fw || dirty.y + dirty.height > fh {
            return Err(WindowError::Backend(format!(
                "tile {}x{} at ({},{}) does not fit a {fw}x{fh} window",
                dirty.width, dirty.height, dirty.x, dirty.y
            )));
        }
        let row_bytes = (fw as usize) * 4;
        let tile_row = (dirty.width as usize) * 4;
        for y in 0..dirty.height as usize {
            let dst = (dirty.y as usize + y) * row_bytes + (dirty.x as usize) * 4;
            let src = y * tile_row;
            self.pixels[dst..dst + tile_row].copy_from_slice(&raw[src..src + tile_row]);
        }
        Ok(())
    }
}

/// What an incoming frame says about the size of the window it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Geometry {
    /// Fits the picture as it is.
    Unchanged,
    /// A new size whose frame replaces the whole picture: start over.
    Resize { width: u32, height: u32 },
    /// Larger than the picture, learnt from a tile: keep what is drawn.
    Grow { width: u32, height: u32 },
}

/// Read a frame covering `dirty`, coded as `codec`, against a picture of
/// `width`x`height`.
///
/// Only a video frame may shrink or reset the picture, because only a video
/// frame is known to be the whole window — the stream is the full frame by
/// construction. A tile names what changed and nothing more; reading "starts at
/// the origin and differs in size" as a resize, as the surrogates once did,
/// cleared the picture to black whenever a change happened to touch the
/// top-left corner (GNOME's Activities button, a pointer in the corner), after
/// which only the tiles that followed were drawn again. A tile reaching past
/// the edge still grows the picture: dragging a window's corner produces
/// exactly that, and the blit refuses a tile it cannot fit.
pub fn geometry_of(dirty: Rect, codec: TileCodec, width: u32, height: u32) -> Geometry {
    let reach_w = dirty.x.saturating_add(dirty.width);
    let reach_h = dirty.y.saturating_add(dirty.height);
    if codec == TileCodec::LossyVideo {
        if (reach_w, reach_h) == (width, height) {
            return Geometry::Unchanged;
        }
        return Geometry::Resize {
            width: reach_w,
            height: reach_h,
        };
    }
    if reach_w > width || reach_h > height {
        return Geometry::Grow {
            width: width.max(reach_w),
            height: height.max(reach_h),
        };
    }
    Geometry::Unchanged
}

/// Where a `src_w`x`src_h` picture lands inside a `dst_w`x`dst_h` area when
/// scaled to fit without distortion, centred: `(x, y, width, height)`.
///
/// For a remote-screen view, which keeps one window while its source changes
/// shape underneath — a portrait phone-sized window one moment, an ultrawide
/// monitor the next. Stretching would be the only alternative, and a
/// stretched desktop is unreadable.
pub fn letterbox(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32, u32, u32) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return (0, 0, dst_w, dst_h);
    }
    // Compared by cross-multiplying, so no rounding decides which side fits.
    let (w, h) = if (src_w as u64) * (dst_h as u64) >= (dst_w as u64) * (src_h as u64) {
        let h = ((dst_w as u64) * (src_h as u64) / (src_w as u64)) as u32;
        (dst_w, h.max(1))
    } else {
        let w = ((dst_h as u64) * (src_w as u64) / (src_h as u64)) as u32;
        (w.max(1), dst_h)
    };
    ((dst_w - w) / 2, (dst_h - h) / 2, w, h)
}

/// Draw `src` into the tightly packed `dst_w` × `dst_h` BGRA buffer `dst`,
/// letterboxed, with black bars.
///
/// For a destination whose size cannot follow the source — a published video
/// node keeps the size it was negotiated at. Nearest-neighbour: this runs per
/// frame on the node's thread, and the common case (sizes equal) is a copy.
pub fn scale_into(src: &Framebuffer, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    let need = (dst_w as usize) * (dst_h as usize) * 4;
    if dst.len() < need {
        return;
    }
    if (src.width, src.height) == (dst_w, dst_h) {
        dst[..need].copy_from_slice(&src.pixels[..need]);
        return;
    }
    dst[..need].fill(0);
    let (x0, y0, w, h) = letterbox(src.width, src.height, dst_w, dst_h);
    if src.width == 0 || src.height == 0 {
        return;
    }
    for y in 0..h {
        let sy = (y as u64 * src.height as u64 / h as u64) as usize;
        let src_row = sy * src.width as usize * 4;
        let dst_row = ((y0 + y) as usize * dst_w as usize + x0 as usize) * 4;
        for x in 0..w as usize {
            let sx = x * src.width as usize / w as usize;
            let s = src_row + sx * 4;
            dst[dst_row + x * 4..dst_row + x * 4 + 4].copy_from_slice(&src.pixels[s..s + 4]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_to_the_same_size_is_a_copy() {
        let mut fb = Framebuffer::new(2, 1);
        fb.pixels.copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut out = vec![9u8; 8];
        scale_into(&fb, &mut out, 2, 1);
        assert_eq!(out, fb.pixels);
    }

    #[test]
    fn a_narrower_source_is_centred_between_black_bars() {
        let mut fb = Framebuffer::new(1, 1);
        fb.pixels.copy_from_slice(&[0xFF; 4]);
        let mut out = vec![7u8; 3 * 4];
        scale_into(&fb, &mut out, 3, 1);
        assert_eq!(out, [0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);
    }

    #[test]
    fn a_larger_source_is_sampled_down_to_fit() {
        let mut fb = Framebuffer::new(4, 2);
        for (i, px) in fb.pixels.chunks_mut(4).enumerate() {
            px.fill(i as u8);
        }
        let mut out = vec![0u8; 2 * 4];
        scale_into(&fb, &mut out, 2, 1);
        assert_eq!(out, [0, 0, 0, 0, 2, 2, 2, 2]);
    }

    #[test]
    fn a_wide_source_gets_bars_above_and_below() {
        assert_eq!(letterbox(1920, 1080, 1000, 1000), (0, 219, 1000, 562));
    }

    #[test]
    fn a_tall_source_gets_bars_at_the_sides() {
        assert_eq!(letterbox(1080, 1920, 1000, 1000), (219, 0, 562, 1000));
    }

    #[test]
    fn a_matching_shape_fills_the_area() {
        assert_eq!(letterbox(1280, 720, 1920, 1080), (0, 0, 1920, 1080));
    }

    #[test]
    fn blit_places_a_tile_at_its_offset() {
        let mut fb = Framebuffer::new(4, 2);
        let tile = [0xFF, 0xFF, 0xFF, 0xFF];
        fb.blit(
            Rect {
                x: 2,
                y: 1,
                width: 1,
                height: 1,
            },
            &tile,
        )
        .unwrap();
        // (y * width + x) * bytes-per-pixel, spelled out rather than written as
        // 24: the point of the test is that the offset follows the formula.
        let (x, y, width) = (2_usize, 1_usize, 4_usize);
        let i = (y * width + x) * 4;
        assert_eq!(&fb.pixels[i..i + 4], &tile);
        assert!(fb.pixels[..i].iter().all(|&b| b == 0));
    }

    /// Growing keeps the picture; only a full frame is allowed to clear it.
    ///
    /// A drag-resize is a stream of tiles that reach past the old edge, and the
    /// pixels around them are still the newest the viewer has. Reallocating
    /// blank would flash the window black on every step of the drag.
    #[test]
    fn growing_preserves_the_pixels_already_drawn() {
        let mut fb = Framebuffer::new(2, 2);
        let white = [0xFFu8; 4];
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            fb.blit(
                Rect {
                    x,
                    y,
                    width: 1,
                    height: 1,
                },
                &white,
            )
            .unwrap();
        }

        fb.grow_to(4, 3);
        assert_eq!((fb.width, fb.height), (4, 3));

        // Every original pixel is still where it was, at its new stride.
        for (x, y) in [(0usize, 0usize), (1, 0), (0, 1), (1, 1)] {
            let i = (y * fb.width as usize + x) * 4;
            assert_eq!(&fb.pixels[i..i + 4], &white, "lost the pixel at ({x},{y})");
        }
        // And the new area is blank rather than holding stale bytes.
        let corner = (2 * fb.width as usize + 3) * 4;
        assert_eq!(&fb.pixels[corner..corner + 4], &[0, 0, 0, 0]);
    }

    /// A tile that reaches past the edge is what a drag-resize looks like, so
    /// growing to fit must make the very same blit succeed.
    #[test]
    fn a_tile_past_the_edge_fits_after_growing_to_it() {
        let mut fb = Framebuffer::new(4, 2);
        let dirty = Rect {
            x: 2,
            y: 1,
            width: 4,
            height: 3,
        };
        let tile = vec![0xAAu8; (dirty.width * dirty.height) as usize * 4];
        assert!(fb.blit(dirty, &tile).is_err(), "precondition: does not fit");

        fb.grow_to(dirty.x + dirty.width, dirty.y + dirty.height);
        fb.blit(dirty, &tile).expect("fits once the window grew");
    }

    #[test]
    fn out_of_bounds_tile_is_rejected_not_clipped() {
        let mut fb = Framebuffer::new(4, 2);
        let err = fb.blit(
            Rect {
                x: 3,
                y: 0,
                width: 2,
                height: 1,
            },
            &[0; 8],
        );
        assert!(matches!(err, Err(WindowError::Backend(_))));
    }

    #[test]
    fn resize_reallocates_to_the_new_geometry() {
        let mut fb = Framebuffer::new(4, 2);
        fb.resize(8, 4);
        assert_eq!(fb.width, 8);
        assert_eq!(fb.pixels.len(), 8 * 4 * 4);
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_tile_in_the_top_left_corner_is_drawn_not_taken_for_a_resize() {
        let corner = rect(0, 0, 120, 32);
        for codec in [TileCodec::RawBgra8888, TileCodec::LosslessBgra8888] {
            assert_eq!(geometry_of(corner, codec, 1920, 1080), Geometry::Unchanged);
        }
    }

    #[test]
    fn a_tile_past_the_edge_grows_the_picture() {
        assert_eq!(
            geometry_of(
                rect(1900, 1000, 40, 100),
                TileCodec::RawBgra8888,
                1920,
                1080
            ),
            Geometry::Grow {
                width: 1940,
                height: 1100
            }
        );
    }

    #[test]
    fn a_video_frame_at_another_size_resizes_either_way() {
        let smaller = rect(0, 0, 1280, 720);
        assert_eq!(
            geometry_of(smaller, TileCodec::LossyVideo, 1920, 1080),
            Geometry::Resize {
                width: 1280,
                height: 720
            }
        );
        let same = rect(0, 0, 1920, 1080);
        assert_eq!(
            geometry_of(same, TileCodec::LossyVideo, 1920, 1080),
            Geometry::Unchanged
        );
    }
}
