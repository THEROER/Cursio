//! Pieces of the H.264 path that are the same on every platform.
//!
//! The *decoders* differ — Linux drives `ffmpeg-next` against system libav\*,
//! Windows drives a Media Foundation transform — but what a decoder is asked
//! for, and how a stream is entered, are properties of H.264 and of this
//! crate's contract, not of the backend. They live here so both
//! implementations share one definition instead of drifting apart.
//!
//! See [`crate::video_decoder`] (Linux) and [`crate::video_decoder_win`]
//! (Windows) for the decoders themselves.

/// NV12 → BGRA, needed by the Windows decoder because Media Foundation emits
/// no packed-RGB type. Built on every platform, not just Windows: it is pure
/// arithmetic with no OS calls, and a colour-conversion bug is silent and
/// expensive, so its tests should run wherever `cargo test` runs.
pub mod nv12;

/// One decoded window image, tightly packed BGRA.
///
/// "Tightly packed" is part of the contract: `pixels.len() == width * height *
/// 4` with no row padding. Both decoders repack if their native output is
/// stride-aligned, because every consumer here (the framebuffer, the blit)
/// assumes no gaps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Whether an H.264 packet contains an IDR (keyframe) NAL unit.
///
/// Scans Annex B start codes for NAL type 5 (IDR). This is deliberately a
/// bitstream scan rather than a flag on our own message: it stays correct even
/// if the encoder's packet boundaries change, and it is what actually decides
/// whether the decoder can start here.
pub(crate) fn contains_keyframe(packet: &[u8]) -> bool {
    // NAL units are separated by 0x000001 or 0x00000001; the byte after the
    // start code carries the type in its low 5 bits.
    let mut i = 0;
    while i + 3 < packet.len() {
        let is_start = packet[i] == 0 && packet[i + 1] == 0 && packet[i + 2] == 1;
        let is_long = packet[i] == 0
            && packet[i + 1] == 0
            && packet[i + 2] == 0
            && i + 4 < packet.len()
            && packet[i + 3] == 1;
        if is_start || is_long {
            let nal = if is_long {
                packet[i + 4]
            } else {
                packet[i + 3]
            };
            match nal & 0x1F {
                // 5 = IDR slice; 7 = SPS, which the encoder emits immediately
                // before an IDR, so either marks a valid entry point.
                5 | 7 => return true,
                _ => {}
            }
            i += if is_long { 4 } else { 3 };
        } else {
            i += 1;
        }
    }
    false
}

/// Cut `(x, y, w, h)` out of a tightly packed BGRA picture `width` pixels wide.
///
/// Returns the picture untouched when the area is the whole of it, which is the
/// common case, and clamps an area reaching past the picture rather than
/// indexing out of bounds.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn crop_bgra(pixels: Vec<u8>, width: u32, area: (u32, u32, u32, u32)) -> Vec<u8> {
    let (x, y, w, h) = area;
    let stride = width as usize * 4;
    let height = pixels.len().checked_div(stride).unwrap_or(0);
    let (x, y) = (x as usize, y as usize);
    let w = (w as usize).min((width as usize).saturating_sub(x));
    let h = (h as usize).min(height.saturating_sub(y));
    if x == 0 && y == 0 && w == width as usize {
        let mut pixels = pixels;
        pixels.truncate(w * h * 4);
        return pixels;
    }
    let mut out = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        let start = row * stride + x * 4;
        out.extend_from_slice(&pixels[start..start + w * 4]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cropping_the_macroblock_padding_keeps_the_visible_rows() {
        // A 2x3 picture whose last row is coding padding, like 1080 in 1088.
        let pixels: Vec<u8> = (0..24).collect();
        assert_eq!(crop_bgra(pixels.clone(), 2, (0, 0, 2, 2)), pixels[..16]);
    }

    #[test]
    fn cropping_an_offset_area_takes_only_its_columns() {
        let pixels: Vec<u8> = (0..32).collect();
        // 4x2 picture, keep the middle two pixels of the second row.
        assert_eq!(
            crop_bgra(pixels, 4, (1, 1, 2, 1)),
            (20..28).collect::<Vec<u8>>()
        );
    }

    #[test]
    fn a_crop_past_the_picture_is_clamped() {
        let pixels = vec![7u8; 16];
        assert_eq!(crop_bgra(pixels, 2, (0, 0, 4, 4)).len(), 16);
    }

    #[test]
    fn keyframe_detection_finds_idr_and_sps() {
        // 3-byte start code + NAL type 5 (IDR).
        assert!(contains_keyframe(&[0, 0, 1, 0x65, 0xAA]));
        // 4-byte start code + NAL type 7 (SPS).
        assert!(contains_keyframe(&[0, 0, 0, 1, 0x67, 0xAA]));
        // NAL type 1 = non-IDR slice: not an entry point.
        assert!(!contains_keyframe(&[0, 0, 1, 0x41, 0xAA]));
        assert!(!contains_keyframe(&[]));
    }
}
