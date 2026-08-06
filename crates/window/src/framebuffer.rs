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
