//! The frame size an H.264 encoder is opened for, checked the same way by
//! every platform's encoder.

use cursio_protocol::Rect;

use crate::WindowError;

/// Refuse a size the encoder cannot take. 4:2:0 chroma is subsampled by two
/// on both axes, so odd dimensions have no valid chroma plane; rejected rather
/// than silently skewed.
pub fn check_encoder_size(width: u32, height: u32) -> Result<(), WindowError> {
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(WindowError::Backend(format!(
            "video encoder needs even dimensions, got {width}x{height}"
        )));
    }
    if width == 0 || height == 0 {
        return Err(WindowError::Backend("zero-sized window".into()));
    }
    Ok(())
}

/// The rect a video frame covers: the whole window, never a dirty sub-rect.
/// The codec's own inter-frame prediction is what makes an unchanged region
/// cost nothing, and an inter-frame packet is not independently croppable.
pub fn whole_frame(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// A frame handed to an encoder must be tightly packed BGRA at its size
/// (`width * height * 4`, no row padding).
pub fn check_frame_len(bgra: &[u8], width: u32, height: u32) -> Result<(), WindowError> {
    let expected = (width as usize) * (height as usize) * 4;
    if bgra.len() != expected {
        return Err(WindowError::Backend(format!(
            "frame is {} bytes, expected {expected} for {width}x{height}",
            bgra.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_odd_or_empty_size_is_refused_and_an_even_one_taken() {
        assert!(check_encoder_size(1280, 720).is_ok());
        assert!(check_encoder_size(1281, 720).is_err());
        assert!(check_encoder_size(1280, 721).is_err());
        assert!(check_encoder_size(0, 720).is_err());
    }

    #[test]
    fn a_frame_must_be_exactly_the_encoders_size() {
        assert!(check_frame_len(&[0; 2 * 2 * 4], 2, 2).is_ok());
        assert!(check_frame_len(&[0; 2 * 2 * 4 + 4], 2, 2).is_err());
    }
}
