//! NV12 ↔ BGRA conversion.
//!
//! Media Foundation's H.264 transforms speak NV12 and nothing else — hardware
//! encoders and decoders do not offer a packed RGB type at all — so the colour
//! conversion FFmpeg does behind `swscale` is ours to do here, in both
//! directions: [`to_bgra`] on the way out of a decoder, [`from_bgra`] on the
//! way into an encoder.
//!
//! NV12 is 8-bit luma at full resolution followed by *interleaved* chroma at
//! half resolution in both axes: one `(Cb, Cr)` pair covers a 2x2 block of
//! pixels. So a frame is `stride * height` luma bytes then `stride * height/2`
//! chroma bytes — the chroma plane uses the same stride as luma, which is why
//! only one stride is passed in.
//!
//! The coefficients are BT.601 limited-range ("studio swing": luma 16..235,
//! chroma 16..240), which is what the H.264 our encoder produces is tagged as
//! for the sizes a desktop window has. Getting the range wrong is not subtle —
//! it shows up as visibly washed-out or crushed blacks, not as a slight tint.

/// Convert one NV12 frame to tightly-packed BGRA.
///
/// `stride` is the byte pitch of the luma rows (and of the chroma rows), which
/// on a hardware decoder is generally *larger* than `width`. The output has no
/// padding: exactly `width * height * 4` bytes, which is what the framebuffer
/// and the blit path expect.
pub fn to_bgra(nv12: &[u8], width: u32, height: u32, stride: usize) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 4];

    // Where the interleaved chroma plane starts. Guard the arithmetic: a short
    // buffer would otherwise index out of bounds mid-loop.
    let chroma_base = stride * h;
    if nv12.len() < chroma_base + stride * (h / 2) {
        tracing::warn!(
            len = nv12.len(),
            expected = chroma_base + stride * (h / 2),
            "NV12 buffer shorter than its geometry; returning black frame"
        );
        return out;
    }

    // Row by row through fixed-length scratch rows rather than indexing the
    // whole buffer per pixel: this runs on every decoded frame on the viewer's
    // window thread, and the indexed form cost 13 ms per 1080p frame on a
    // desktop CPU — most of a 60 fps budget before the decoder and the blit
    // had run at all. Spreading chroma to one value per pixel first leaves the
    // colour maths a plain per-pixel map the compiler vectorizes.
    let pairs = w.div_ceil(2) * 2;
    let (mut cb_row, mut cr_row) = (vec![0i16; pairs], vec![0i16; pairs]);
    let mut px_row = vec![0u32; w];
    let mut spread_for = None;
    for (y, out_row) in out.chunks_exact_mut(w * 4).enumerate() {
        let luma = &nv12[y * stride..][..w];
        // Each chroma row serves two luma rows; an odd last row reuses the one
        // above it rather than reading past the plane.
        let chroma_y = (y / 2).min((h / 2).saturating_sub(1));
        if spread_for != Some(chroma_y) {
            let Some(chroma) = nv12
                .get(chroma_base + chroma_y * stride..)
                .and_then(|c| c.get(..pairs))
            else {
                continue;
            };
            for ((cb, cr), &[u, v]) in cb_row
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(cr_row.as_chunks_mut::<2>().0.iter_mut())
                .zip(chroma.as_chunks::<2>().0)
            {
                *cb = [u as i16 - 128; 2];
                *cr = [v as i16 - 128; 2];
            }
            spread_for = Some(chroma_y);
        }
        for (((px, &l), &cb), &cr) in px_row.iter_mut().zip(luma).zip(&cb_row).zip(&cr_row) {
            // BT.601 limited range in 6-bit fixed point, so the maths runs
            // eight or sixteen pixels to a lane in i16. Luma above 235 plus
            // strong chroma can pass i16::MAX; saturating there is exact,
            // since anything that large clamps to 255 anyway.
            let y601 = (l as i16 - 16).max(0) * 74 + 32;
            let b = clamp16(y601.saturating_add(129 * cb) >> 6) as u32;
            let g = clamp16(y601.saturating_sub(25 * cb + 52 * cr) >> 6) as u32;
            let r = clamp16(y601.saturating_add(102 * cr) >> 6) as u32;
            // Opaque: a captured window has no alpha.
            *px = b | g << 8 | r << 16 | 0xFF << 24;
        }
        for (dst, px) in out_row.as_chunks_mut::<4>().0.iter_mut().zip(&px_row) {
            *dst = px.to_le_bytes();
        }
    }

    out
}

/// Convert tightly-packed BGRA into NV12, for feeding an encoder.
///
/// The inverse of [`to_bgra`], same BT.601 limited range. Writes into `out`
/// (resizing it as needed) rather than returning a fresh `Vec`, because this
/// runs once per captured frame on the capture thread and a per-frame
/// allocation of a megabyte-plus is pure waste.
///
/// The output is tightly packed — luma stride equals `width` — which is what an
/// encoder MFT accepts for a buffer we allocate ourselves.
///
/// Chroma is **averaged** over each 2x2 block rather than point-sampled. Point
/// sampling is cheaper but throws away three quarters of the colour
/// information, which shows up as ragged fringes on coloured text — exactly the
/// content this path carries.
pub fn from_bgra(bgra: &[u8], width: u32, height: u32, out: &mut Vec<u8>) {
    let (w, h) = (width as usize, height as usize);
    let luma_len = w * h;
    let chroma_len = w * (h / 2);
    out.clear();
    out.resize(luma_len + chroma_len, 0);

    if bgra.len() < luma_len * 4 {
        tracing::warn!(
            len = bgra.len(),
            expected = luma_len * 4,
            "BGRA buffer shorter than its geometry; encoding a black frame"
        );
        // Y=16, chroma=128 is legal limited-range black, not the 0s already
        // there (which would be an out-of-range "blacker than black").
        out[..luma_len].fill(16);
        out[luma_len..].fill(128);
        return;
    }

    let (luma, chroma) = out.split_at_mut(luma_len);

    for y in 0..h {
        for x in 0..w {
            let p = (y * w + x) * 4;
            let (b, g, r) = (bgra[p] as i32, bgra[p + 1] as i32, bgra[p + 2] as i32);
            // BT.601 limited range, fixed point (<<8), mirroring `to_bgra`.
            luma[y * w + x] = clamp(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
        }
    }

    // One (Cb, Cr) pair per 2x2 block, averaged over its four pixels.
    for by in 0..h / 2 {
        for bx in 0..w / 2 {
            let (mut sb, mut sg, mut sr) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = ((by * 2 + dy) * w + bx * 2 + dx) * 4;
                    sb += bgra[p] as i32;
                    sg += bgra[p + 1] as i32;
                    sr += bgra[p + 2] as i32;
                }
            }
            let (b, g, r) = (sb / 4, sg / 4, sr / 4);
            let c = by * w + bx * 2;
            chroma[c] = clamp(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);
            chroma[c + 1] = clamp(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);
        }
    }
}

#[inline]
fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[inline]
fn clamp16(v: i16) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an NV12 buffer of one solid (Y, Cb, Cr) colour.
    fn solid(width: u32, height: u32, stride: usize, y: u8, cb: u8, cr: u8) -> Vec<u8> {
        let h = height as usize;
        let mut buf = vec![0u8; stride * h + stride * (h / 2)];
        for row in 0..h {
            for x in 0..width as usize {
                buf[row * stride + x] = y;
            }
        }
        let base = stride * h;
        for row in 0..h / 2 {
            for x in 0..(width as usize) / 2 {
                buf[base + row * stride + x * 2] = cb;
                buf[base + row * stride + x * 2 + 1] = cr;
            }
        }
        buf
    }

    #[test]
    fn output_is_tightly_packed_regardless_of_stride() {
        // A stride wider than the width is the hardware-decoder case; the
        // output must still have no padding.
        let nv12 = solid(4, 4, 64, 128, 128, 128);
        let bgra = to_bgra(&nv12, 4, 4, 64);
        assert_eq!(bgra.len(), 4 * 4 * 4);
    }

    #[test]
    fn neutral_chroma_and_full_luma_is_white() {
        // Y=235 is limited-range peak white; neutral chroma means no colour.
        let nv12 = solid(2, 2, 2, 235, 128, 128);
        let bgra = to_bgra(&nv12, 2, 2, 2);
        for px in bgra.chunks(4) {
            assert!(
                px[0] > 250 && px[1] > 250 && px[2] > 250,
                "expected white, got {px:?}"
            );
            assert_eq!(px[3], 0xFF);
        }
    }

    #[test]
    fn limited_range_black_is_black_not_grey() {
        // Y=16 is limited-range black. Treating the range as full would leave
        // a visible grey floor here — the classic washed-out-blacks bug.
        let nv12 = solid(2, 2, 2, 16, 128, 128);
        let bgra = to_bgra(&nv12, 2, 2, 2);
        for px in bgra.chunks(4) {
            assert_eq!(&px[..3], &[0, 0, 0], "expected black, got {px:?}");
        }
    }

    #[test]
    fn every_colour_stays_within_two_levels_of_exact_bt601() {
        for y in (0..=255u8).step_by(3) {
            for cb in (0..=255u8).step_by(5) {
                for cr in (0..=255u8).step_by(5) {
                    let got = to_bgra(&solid(2, 2, 2, y, cb, cr), 2, 2, 2);
                    let yf = (y as f64 - 16.0).max(0.0) * 1.164;
                    let (u, v) = (cb as f64 - 128.0, cr as f64 - 128.0);
                    let want = [yf + 2.017 * u, yf - 0.392 * u - 0.813 * v, yf + 1.596 * v];
                    for c in 0..3 {
                        let exact = want[c].round().clamp(0.0, 255.0);
                        let delta = (got[c] as f64 - exact).abs();
                        assert!(delta <= 2.0, "y={y} cb={cb} cr={cr} channel {c}: {delta}");
                    }
                }
            }
        }
    }

    #[test]
    fn odd_sizes_convert_without_reading_past_the_planes() {
        // Five columns take three chroma pairs; three rows take one chroma
        // row, which the last luma row must share rather than look past.
        let nv12 = vec![128u8; 6 * 3 + 6];
        let bgra = to_bgra(&nv12, 5, 3, 6);
        assert_eq!(bgra.len(), 5 * 3 * 4);
        assert!(
            bgra.chunks(4).all(|p| p == [130, 130, 130, 0xFF]),
            "{bgra:?}"
        );
    }

    #[test]
    fn chroma_extremes_produce_saturated_colour() {
        // Max Cr with mid luma is red; max Cb is blue. This is what catches a
        // swapped Cb/Cr, which neutral-grey tests cannot see.
        let red = to_bgra(&solid(2, 2, 2, 128, 128, 240), 2, 2, 2);
        assert!(red[2] > red[0] && red[2] > red[1], "expected red-dominant");

        let blue = to_bgra(&solid(2, 2, 2, 128, 240, 128), 2, 2, 2);
        assert!(
            blue[0] > blue[1] && blue[0] > blue[2],
            "expected blue-dominant"
        );
    }

    /// Build a solid BGRA image.
    fn solid_bgra(width: u32, height: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut buf = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            buf.extend_from_slice(&[b, g, r, 0xFF]);
        }
        buf
    }

    #[test]
    fn bgra_survives_a_round_trip_through_nv12() {
        // 4:2:0 discards three quarters of the chroma, so this cannot be exact
        // — but on solid colours there is nothing to discard, and anything
        // worse than a rounding error means the two conversions disagree.
        for (b, g, r) in [(0, 0, 0), (255, 255, 255), (20, 200, 60), (200, 40, 90)] {
            let src = solid_bgra(4, 4, b, g, r);
            let mut nv12 = Vec::new();
            from_bgra(&src, 4, 4, &mut nv12);
            let back = to_bgra(&nv12, 4, 4, 4);

            for (i, (got, want)) in back.chunks(4).zip(src.chunks(4)).enumerate() {
                for c in 0..3 {
                    let delta = (got[c] as i32 - want[c] as i32).abs();
                    assert!(
                        delta <= 3,
                        "pixel {i} channel {c}: got {} want {} (delta {delta})",
                        got[c],
                        want[c]
                    );
                }
            }
        }
    }

    #[test]
    fn from_bgra_packs_tightly_and_reuses_its_buffer() {
        let src = solid_bgra(8, 6, 10, 20, 30);
        let mut out = vec![0xAAu8; 999]; // pre-dirtied: must be fully replaced
        from_bgra(&src, 8, 6, &mut out);
        // Luma is width*height, chroma is half that: 1.5 bytes per pixel.
        assert_eq!(out.len(), 8 * 6 + 8 * 3);

        // Encoding again into the same buffer must give the same bytes, not
        // append to or keep stale content from the last call.
        let first = out.clone();
        from_bgra(&src, 8, 6, &mut out);
        assert_eq!(first, out);
    }

    #[test]
    fn from_bgra_on_a_short_buffer_yields_legal_black() {
        let mut out = Vec::new();
        from_bgra(&[0u8; 4], 16, 16, &mut out);
        assert_eq!(out.len(), 16 * 16 + 16 * 8);
        // Limited-range black is Y=16 with neutral chroma — NOT zeros, which
        // would be out of range and decode as a murky green.
        assert!(out[..256].iter().all(|&y| y == 16));
        assert!(out[256..].iter().all(|&c| c == 128));
    }

    #[test]
    fn short_buffer_yields_black_rather_than_panicking() {
        // A truncated sample must not index out of bounds.
        let bgra = to_bgra(&[0u8; 4], 16, 16, 16);
        assert_eq!(bgra.len(), 16 * 16 * 4);
        assert!(bgra.chunks(4).all(|p| p[..3] == [0, 0, 0]));
    }
}
