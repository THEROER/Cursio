//! mSBC on a transparent SCO link: the H2 framing around each frame.
//!
//! Wideband speech (HFP 1.6+) carries 16 kHz audio as mSBC, a fixed SBC
//! configuration of 57-byte frames holding 120 samples (7.5 ms) each. The
//! controller passes the bytes through untouched ("transparent" air mode), so
//! the framing is this side's: every frame travels as a 60-byte packet of a
//! two-byte H2 synchronisation header, the frame, and one byte of padding.
//!
//! What arrives on the socket is not cut on those boundaries. A USB
//! controller hands SCO over in packets of its own size (24 or 48 bytes, or
//! the socket's MTU), a packet can be lost on the air, and a corrupted one is
//! not marked as such. So the reader is a scanner: it keeps the bytes, looks
//! for an H2 header followed by mSBC's sync word, and takes the 57 bytes
//! after it; anything else is skipped a byte at a time until the stream
//! lines up again. That is what PipeWire and oFono do, for the same reason.

/// One mSBC frame.
pub const FRAME_LEN: usize = 57;
/// One frame in its H2 packet, padding included.
pub const PACKET_LEN: usize = 60;
/// Samples (mono, 16 kHz) in one frame.
pub const FRAME_SAMPLES: usize = 120;
/// The link's sample rate in wideband.
pub const RATE: u32 = 16_000;

/// The first H2 byte, always the same.
const H2_SYNC: u8 = 0x01;
/// The second H2 byte: a two-bit sequence number with each bit doubled,
/// counting 0..4 and round again.
const H2_SEQUENCE: [u8; 4] = [0x08, 0x38, 0xC8, 0xF8];
/// The first byte of every mSBC frame.
const MSBC_SYNC: u8 = 0xAD;

/// Wrap the `n`th frame sent in its packet.
pub fn packet(n: usize, frame: &[u8; FRAME_LEN]) -> [u8; PACKET_LEN] {
    let mut out = [0u8; PACKET_LEN];
    out[0] = H2_SYNC;
    out[1] = H2_SEQUENCE[n % H2_SEQUENCE.len()];
    out[2..2 + FRAME_LEN].copy_from_slice(frame);
    out
}

/// Bytes off the socket in, whole frames out.
#[derive(Default)]
pub struct Reassembler {
    buf: Vec<u8>,
}

impl Reassembler {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole frame, or `None` until more bytes arrive.
    pub fn next_frame(&mut self) -> Option<[u8; FRAME_LEN]> {
        let mut start = 0;
        let found = loop {
            if self.buf.len() - start < 2 + FRAME_LEN {
                break None;
            }
            let at = &self.buf[start..];
            if at[0] == H2_SYNC && H2_SEQUENCE.contains(&at[1]) && at[2] == MSBC_SYNC {
                let mut frame = [0u8; FRAME_LEN];
                frame.copy_from_slice(&at[2..2 + FRAME_LEN]);
                break Some((frame, start + 2 + FRAME_LEN));
            }
            start += 1;
        };
        match found {
            Some((frame, used)) => {
                self.buf.drain(..used);
                Some(frame)
            }
            None => {
                // Keep only what could still begin a packet: the scan above
                // has already ruled out everything before `start`.
                self.buf.drain(..start);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(fill: u8) -> [u8; FRAME_LEN] {
        let mut f = [fill; FRAME_LEN];
        f[0] = MSBC_SYNC;
        f
    }

    #[test]
    fn a_packet_carries_the_header_the_frame_and_padding() {
        let p = packet(2, &frame(7));
        assert_eq!(&p[..2], &[0x01, 0xC8]);
        assert_eq!(p[2], MSBC_SYNC);
        assert_eq!(p[PACKET_LEN - 1], 0);
        assert_eq!(packet(4, &frame(7))[1], 0x08);
    }

    #[test]
    fn frames_come_back_whole_from_packets_cut_at_any_size() {
        let mut wire = Vec::new();
        for n in 0..5 {
            wire.extend_from_slice(&packet(n, &frame(n as u8 + 1)));
        }
        let mut r = Reassembler::default();
        let mut got = Vec::new();
        for piece in wire.chunks(24) {
            r.push(piece);
            while let Some(f) = r.next_frame() {
                got.push(f[1]);
            }
        }
        assert_eq!(got, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn the_stream_lines_up_again_after_garbage() {
        let mut r = Reassembler::default();
        r.push(&[0x55, 0x01, 0x00, 0xAD, 0x12]);
        r.push(&packet(1, &frame(9)));
        let f = r.next_frame().expect("the packet after the garbage");
        assert_eq!(f[1], 9);
        assert!(r.next_frame().is_none());
    }

    #[test]
    fn a_partial_packet_waits_for_the_rest() {
        let p = packet(0, &frame(3));
        let mut r = Reassembler::default();
        r.push(&p[..30]);
        assert!(r.next_frame().is_none());
        r.push(&p[30..]);
        assert_eq!(r.next_frame().unwrap()[1], 3);
    }
}
