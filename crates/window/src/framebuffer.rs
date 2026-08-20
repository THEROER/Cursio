//! The viewer's assembled window image, shared by every surrogate backend.
//!
//! Frames carry only dirty rects, so a viewer must keep the whole picture and
//! blit each update into it. That logic is identical on GTK and Win32 — only
//! the final step (handing the pixels to the toolkit) differs — so it lives
//! here rather than being written twice.

use cursio_protocol::Rect;

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
    /// The host signals a resize by sending a frame whose dirty rect covers the
    /// full new size, so the discarded pixels are about to be replaced wholesale.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
