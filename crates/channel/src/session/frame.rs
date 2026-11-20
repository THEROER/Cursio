//! The session's own frames: a numbered payload, and the acknowledgement and
//! resume snapshots of what each lane delivered.

use crate::seal::link_tag;
use crate::{ChannelError, LaneId};

/// `ACK` flag: this one is a keepalive for every path, not only the carrying one.
pub(super) const ACK_ALL_PATHS: u8 = 0x01;
/// `SEQ` flag: the frame is from a fresh message.
pub(super) const SEQ_LOSSY: u8 = 0x01;

pub(super) fn seq_frame(lane: LaneId, seq: u64, lossy: bool, tagged: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(link_tag::SEQ_HEADER_LEN + tagged.len());
    frame.push(link_tag::SEQ);
    frame.push(lane.get());
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.push(if lossy { SEQ_LOSSY } else { 0 });
    frame.extend_from_slice(tagged);
    frame
}

fn snapshot_frame(tag: u8, delivered: &[u64], flag: u8) -> Vec<u8> {
    let mut frame = vec![tag];
    for seq in delivered {
        frame.extend_from_slice(&seq.to_be_bytes());
    }
    frame.push(flag);
    frame
}

pub(super) fn ack_frame(delivered: &[u64], flags: u8) -> Vec<u8> {
    snapshot_frame(link_tag::ACK, delivered, flags)
}

pub(super) fn resume_frame(delivered: &[u64], lane: Option<LaneId>) -> Vec<u8> {
    snapshot_frame(
        link_tag::RESUME,
        delivered,
        lane.map_or(u8::MAX, LaneId::get),
    )
}

/// The delivered numbers of a snapshot of `lanes` lanes, flag byte included.
pub(super) fn read_delivered(bytes: &[u8], lanes: usize) -> Result<Vec<u64>, ChannelError> {
    if bytes.len() != lanes * 8 + 1 {
        return Err(ChannelError::Other("invalid delivery snapshot".into()));
    }
    Ok((0..lanes)
        .map(|i| read_u64(&bytes[i * 8..]).expect("length checked"))
        .collect())
}

pub(super) fn read_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes a peer built before the channel was extracted sends and
    /// expects, for Cursio's four lanes.
    #[test]
    fn the_session_frames_keep_their_bytes() {
        assert_eq!(
            seq_frame(LaneId::new(3), 0x0102, true, &[0x00, 0xAA]),
            [0x02, 3, 0, 0, 0, 0, 0, 0, 0x01, 0x02, 0x01, 0x00, 0xAA]
        );
        let mut ack = vec![0x03];
        for n in 1..=4u64 {
            ack.extend_from_slice(&n.to_be_bytes());
        }
        ack.push(ACK_ALL_PATHS);
        assert_eq!(ack_frame(&[1, 2, 3, 4], ACK_ALL_PATHS), ack);
        let resume = resume_frame(&[1, 2, 3, 4], None);
        assert_eq!(resume[0], 0x04);
        assert_eq!(resume[1..33], ack[1..33]);
        assert_eq!(resume[33], u8::MAX);
        assert_eq!(resume_frame(&[0; 4], Some(LaneId::new(2)))[33], 2);
        assert_eq!(read_delivered(&ack[1..], 4).unwrap(), [1, 2, 3, 4]);
        assert!(read_delivered(&ack[1..], 3).is_err());
    }
}
