//! Length-prefixed framing.
//!
//! Messages travel over a byte stream (TCP today), which has no notion of
//! message boundaries. Each frame is a 4-byte big-endian length prefix
//! followed by exactly that many payload bytes:
//!
//! ```text
//! +-----------------+------------------------+
//! | len: u32 (BE)   | payload: [u8; len]     |
//! +-----------------+------------------------+
//! ```
//!
//! The helpers here are sync and buffer-oriented; the async read/write loop
//! lives in the transport crate and uses these to cut/assemble frames.

/// Upper bound on a single frame's payload, to bound memory a peer can force
/// us to allocate. Pairing/heartbeat frames are tiny; this is generous
/// headroom without letting a malicious length prefix trigger a huge alloc.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024; // 16 MiB

/// Errors from framing/deframing.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame length {0} exceeds maximum {MAX_FRAME_LEN}")]
    TooLarge(usize),
    #[error("incomplete frame: need {needed} bytes, have {have}")]
    Incomplete { needed: usize, have: usize },
}

/// Prepend a 4-byte big-endian length prefix to `payload`, returning the
/// complete frame ready to write to a stream.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Attempt to cut one frame off the front of `buf`.
///
/// Returns:
/// - `Ok(Some((payload, consumed)))` when a full frame is present; `consumed`
///   is how many bytes (prefix + payload) to drain from `buf`.
/// - `Ok(None)` when more bytes are needed — the caller should read more and
///   retry without discarding what it has.
/// - `Err` on a length prefix that violates [`MAX_FRAME_LEN`].
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Vec<u8>, usize)>, FrameError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let total = 4 + len;
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some((buf[4..total].to_vec(), total)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_then_decode_roundtrips() {
        let payload = b"hello cursio";
        let frame = encode_frame(payload).unwrap();
        let (decoded, consumed) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(decoded, payload);
        assert_eq!(consumed, frame.len());
    }

    #[test]
    fn decode_needs_more_when_prefix_incomplete() {
        assert!(decode_frame(&[0, 0]).unwrap().is_none());
    }

    #[test]
    fn decode_needs_more_when_payload_incomplete() {
        let mut frame = encode_frame(b"abcd").unwrap();
        frame.pop(); // drop last payload byte
        assert!(decode_frame(&frame).unwrap().is_none());
    }

    #[test]
    fn decode_leaves_trailing_bytes_for_next_frame() {
        let mut stream = encode_frame(b"one").unwrap();
        stream.extend_from_slice(&encode_frame(b"two").unwrap());
        let (first, consumed) = decode_frame(&stream).unwrap().unwrap();
        assert_eq!(first, b"one");
        let (second, _) = decode_frame(&stream[consumed..]).unwrap().unwrap();
        assert_eq!(second, b"two");
    }

    #[test]
    fn oversized_length_prefix_is_rejected() {
        let bogus = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes();
        assert!(matches!(decode_frame(&bogus), Err(FrameError::TooLarge(_))));
    }
}
