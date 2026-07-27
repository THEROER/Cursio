//! Tile encode/decode for the two implemented [`TileCodec`]s.
//!
//! Both the host (encode before sending a [`WindowFrame`]) and the viewer
//! (decode before blitting) need this, and both must agree byte-for-byte, so it
//! lives in one place rather than in each backend. Only the two lossless codecs
//! are handled — [`TileCodec::RawBgra8888`] (identity) and
//! [`TileCodec::LosslessBgra8888`] (zstd of the raw BGRA). The lossy variants
//! are still protocol placeholders; asking to code/decode one is an error, not a
//! silent fallback, so a premature use surfaces instead of shipping garbage.
//!
//! A "tile" here is a `dirty.width * dirty.height` block of BGRA pixels, 4
//! bytes/pixel, row-major, no row padding — the exact layout
//! [`WindowFrame::data`] documents for `RawBgra8888`.

use cursio_protocol::TileCodec;

use crate::WindowError;

/// zstd level for [`TileCodec::LosslessBgra8888`]. Level 1 is deliberately low:
/// the point of the lossless codec is *cheap* CPU on small text/UI tiles that we
/// send often, not maximum ratio. Screen BGRA has long runs of identical pixels
/// (flat backgrounds, glyph edges), which even level 1 collapses well, and a low
/// level keeps encode latency off the frame path. The adaptive controller
/// reaches for lossy codecs when it needs a *smaller* payload, not a slower one.
const ZSTD_LEVEL: i32 = 1;

/// Encode one raw-BGRA tile (`raw` = `width*height*4` bytes) per `codec`.
///
/// Returns the bytes to put in [`WindowFrame::data`]. `RawBgra8888` returns the
/// input unchanged; `LosslessBgra8888` returns its zstd compression. The lossy
/// codecs have no encoder yet and return [`WindowError::Unsupported`].
pub fn encode_tile(codec: TileCodec, raw: &[u8]) -> Result<Vec<u8>, WindowError> {
    match codec {
        TileCodec::RawBgra8888 => Ok(raw.to_vec()),
        TileCodec::LosslessBgra8888 => zstd::encode_all(raw, ZSTD_LEVEL)
            .map_err(|e| WindowError::Backend(format!("zstd encode: {e}"))),
        TileCodec::LossyImage | TileCodec::LossyVideo => Err(WindowError::Unsupported),
    }
}

/// Decode one [`WindowFrame::data`] payload back to raw BGRA (`width*height*4`
/// bytes) given the frame's `codec`.
///
/// The caller passes the tile's `expected_len` (`dirty.width * dirty.height *
/// 4`) so a corrupt or mismatched payload is caught here rather than as an
/// out-of-bounds blit: a decoded length that doesn't match is a protocol error.
pub fn decode_tile(
    codec: TileCodec,
    data: &[u8],
    expected_len: usize,
) -> Result<Vec<u8>, WindowError> {
    let raw = match codec {
        TileCodec::RawBgra8888 => data.to_vec(),
        TileCodec::LosslessBgra8888 => zstd::decode_all(data)
            .map_err(|e| WindowError::Backend(format!("zstd decode: {e}")))?,
        TileCodec::LossyImage | TileCodec::LossyVideo => return Err(WindowError::Unsupported),
    };
    if raw.len() != expected_len {
        return Err(WindowError::Backend(format!(
            "decoded tile is {} bytes, expected {expected_len}",
            raw.len()
        )));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tile() -> Vec<u8> {
        // 4x2 BGRA with flats + an edge, the kind of run zstd likes.
        let mut v = Vec::new();
        for i in 0..(4 * 2) {
            let c = if i < 4 { 0x20 } else { 0xC0 };
            v.extend_from_slice(&[c, c, c, 0xFF]);
        }
        v
    }

    #[test]
    fn raw_round_trips_unchanged() {
        let tile = sample_tile();
        let enc = encode_tile(TileCodec::RawBgra8888, &tile).unwrap();
        assert_eq!(enc, tile);
        let dec = decode_tile(TileCodec::RawBgra8888, &enc, tile.len()).unwrap();
        assert_eq!(dec, tile);
    }

    #[test]
    fn lossless_round_trips_exactly() {
        let tile = sample_tile();
        let enc = encode_tile(TileCodec::LosslessBgra8888, &tile).unwrap();
        let dec = decode_tile(TileCodec::LosslessBgra8888, &enc, tile.len()).unwrap();
        assert_eq!(dec, tile, "lossless codec must be pixel-exact");
    }

    #[test]
    fn wrong_decoded_length_is_rejected() {
        let tile = sample_tile();
        let enc = encode_tile(TileCodec::LosslessBgra8888, &tile).unwrap();
        // Claim a different expected length than the tile really is.
        let err = decode_tile(TileCodec::LosslessBgra8888, &enc, tile.len() + 4);
        assert!(matches!(err, Err(WindowError::Backend(_))));
    }

    #[test]
    fn lossy_codecs_are_unsupported_for_now() {
        assert!(matches!(
            encode_tile(TileCodec::LossyImage, &sample_tile()),
            Err(WindowError::Unsupported)
        ));
        assert!(matches!(
            decode_tile(TileCodec::LossyVideo, &[], 0),
            Err(WindowError::Unsupported)
        ));
    }
}
