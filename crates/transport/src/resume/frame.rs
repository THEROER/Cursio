//! The link's own frames: a numbered payload, and the acknowledgement and
//! resume snapshots of what each lane delivered.

use crate::crypto::link_tag;
use crate::lane::Lane;
use crate::TransportError;

/// `ACK` flag: this one is a keepalive for every path, not only the carrying one.
pub(super) const ACK_ALL_PATHS: u8 = 0x01;
/// `SEQ` flag: the frame is from a lossy message.
pub(super) const SEQ_LOSSY: u8 = 0x01;

pub(super) fn seq_frame(lane: Lane, seq: u64, lossy: bool, tagged: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(link_tag::SEQ_HEADER_LEN + tagged.len());
    frame.push(link_tag::SEQ);
    frame.push(lane as u8);
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.push(if lossy { SEQ_LOSSY } else { 0 });
    frame.extend_from_slice(tagged);
    frame
}

pub(super) fn snapshot_frame(tag: u8, delivered: [u64; Lane::COUNT], flag: u8) -> Vec<u8> {
    let mut frame = vec![tag];
    for seq in delivered {
        frame.extend_from_slice(&seq.to_be_bytes());
    }
    frame.push(flag);
    frame
}

pub(super) fn ack_frame(delivered: [u64; Lane::COUNT], flags: u8) -> Vec<u8> {
    snapshot_frame(link_tag::ACK, delivered, flags)
}

pub(super) fn resume_frame(delivered: [u64; Lane::COUNT], lane: Option<Lane>) -> Vec<u8> {
    snapshot_frame(
        link_tag::RESUME,
        delivered,
        lane.map_or(u8::MAX, |lane| lane as u8),
    )
}

pub(super) fn read_delivered(bytes: &[u8]) -> Result<[u64; Lane::COUNT], TransportError> {
    if bytes.len() != Lane::COUNT * 8 + 1 {
        return Err(TransportError::Other("invalid delivery snapshot".into()));
    }
    Ok(std::array::from_fn(|i| read_u64(&bytes[i * 8..]).unwrap()))
}

pub(super) fn read_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?))
}
