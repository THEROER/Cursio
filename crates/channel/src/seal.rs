//! Sealing frames with Noise, and cutting messages into frames that fit.
//!
//! Every plaintext frame starts with a one-byte tag. `WHOLE` (0x00) is
//! followed by a complete encoded message — the common case: input, sound and
//! control all fit in one frame. `PART` (0x01) is followed by
//! `[u32 message][u16 index][u16 count]` and one piece of a message too big
//! for a single Noise frame; the receiver buffers pieces by message and
//! decodes once all have arrived. The session's own frames (`SEQ`, `ACK`,
//! `RESUME`, `PING`, `PONG`) share the tag space; see [`link_tag`].
//!
//! Chunking lives here, at the seal, because this is where both the exact
//! encoded size and Noise's limit of 65535 bytes per message (including the
//! 16-byte tag) are known. Sealing is a separate step from chunking so a
//! scheduler can queue plaintext pieces and seal them one at a time in send
//! order: Noise nonces must advance in exactly the order frames reach the
//! wire, so nothing may be sealed until the drainer has decided what goes
//! next.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use snow::TransportState;

use crate::ChannelError;

/// Max plaintext per Noise message: the spec's 65535 bytes minus the AEAD tag.
const MAX_NOISE_PLAINTEXT: usize = 65535 - 16;

pub(crate) mod frame_tag {
    pub const WHOLE: u8 = 0x00;
    pub const PART: u8 = 0x01;
}

/// Tags only a session writes (see [`crate::session`]); a link over one
/// connection never sees them.
pub(crate) mod link_tag {
    /// `[u8 lane][u64 seq][u8 flags][tagged frame]`: one frame of the session,
    /// numbered so the receiver drops the copies a resend produces.
    pub const SEQ: u8 = 0x02;
    /// `[u64 delivered; lanes][u8 flags]`: the highest sequence number this
    /// side has delivered on each lane. Doubles as the path's keepalive.
    pub const ACK: u8 = 0x03;
    /// `[u64 delivered; lanes][u8 lane][u8 session_tag; 16]`: the first frame
    /// on a path joining a session, after which each side resends whatever
    /// the other has not delivered.
    pub const RESUME: u8 = 0x04;
    pub const PING: u8 = 0x05;
    pub const PONG: u8 = 0x06;
    /// What `SEQ` puts in front of a tagged frame: the tag, the lane, the
    /// number and the flags.
    pub const SEQ_HEADER_LEN: usize = 1 + 1 + 8 + 1;
}

/// The most of a Noise message a tagged frame may fill. A session wraps every
/// frame in a `SEQ` header before sealing it, so a frame cut to the whole
/// Noise budget no longer fits once wrapped: the seal fails, the path drops,
/// and the same frame waits at the head of the resend buffer to end every
/// path that joins after it.
const MAX_TAGGED_PLAINTEXT: usize = MAX_NOISE_PLAINTEXT - link_tag::SEQ_HEADER_LEN;

/// Header on every `PART` frame: which message, which piece, how many total.
const PART_HEADER_LEN: usize = 4 + 2 + 2;

/// Usable payload per `PART` frame.
const MAX_CHUNK_PAYLOAD: usize = MAX_TAGGED_PLAINTEXT - 1 - PART_HEADER_LEN;
// A link over one connection carries no SEQ header, and Cursio's frozen
// update lane still receives such frames from builds whose chunks filled the
// whole Noise budget.
const MAX_ACCEPTED_CHUNK_PAYLOAD: usize = MAX_NOISE_PLAINTEXT - 1 - PART_HEADER_LEN;

/// Largest message the channel carries.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PARTS: usize = MAX_MESSAGE_BYTES.div_ceil(MAX_CHUNK_PAYLOAD);
/// Scheduling can interleave several chunked messages, but a peer may not
/// create unbounded half-finished allocations.
const MAX_IN_FLIGHT_MESSAGES: usize = 8;

/// The smallest frame [`chunk_within`] cuts to.
pub const MIN_FRAME: usize = 1024;

/// One connection's Noise transport state. Both directions share it (snow
/// keeps separate send and receive nonces), so the halves of a split carrier
/// share it behind a mutex, held only for the CPU-bound call — never across
/// an `.await` — so it never serialises network I/O.
#[derive(Clone)]
pub struct Seal(Arc<Mutex<TransportState>>);

impl Seal {
    pub(crate) fn new(state: TransportState) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    /// Seal one tagged plaintext frame.
    pub fn seal(&self, tagged: &[u8]) -> Result<Vec<u8>, ChannelError> {
        let mut ciphertext = vec![0u8; tagged.len() + 16];
        let len = self
            .0
            .lock()
            .unwrap()
            .write_message(tagged, &mut ciphertext)
            .map_err(noise_error)?;
        ciphertext.truncate(len);
        Ok(ciphertext)
    }

    /// Open one sealed frame to its tagged plaintext.
    pub fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, ChannelError> {
        let mut plaintext = vec![0u8; ciphertext.len()];
        let len = self
            .0
            .lock()
            .unwrap()
            .read_message(ciphertext, &mut plaintext)
            .map_err(noise_error)?;
        plaintext.truncate(len);
        Ok(plaintext)
    }
}

pub(crate) fn noise_error(e: snow::Error) -> ChannelError {
    ChannelError::Noise(format!("{e:?}"))
}

/// Next message id for chunked sends. Only meaningful within one sender;
/// wraps harmlessly, since a wrapped id cannot collide with one still in
/// flight at any realistic rate.
static NEXT_MSG_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Cut an encoded message into one or more tagged plaintext frames.
pub fn chunk(body: &[u8]) -> Result<Vec<Vec<u8>>, ChannelError> {
    chunk_within(body, || MAX_TAGGED_PLAINTEXT)
}

/// As [`chunk`], with each tagged frame at most `frame` bytes, which is
/// asked only of a message too large for [`MIN_FRAME`].
///
/// A frame on the wire cannot be interrupted, so its length is how long
/// whatever is queued behind it waits: on a slow path that bounds how long a
/// pointer move waits behind a file or an image. Receivers accept any piece
/// up to the full budget, and the piece grows past `frame` only where a
/// message would otherwise need more pieces than a receiver accepts.
pub fn chunk_within(
    body: &[u8],
    frame: impl FnOnce() -> usize,
) -> Result<Vec<Vec<u8>>, ChannelError> {
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(ChannelError::MessageTooLarge(body.len()));
    }
    let frame = if body.len() < MIN_FRAME {
        MAX_TAGGED_PLAINTEXT
    } else {
        frame().clamp(MIN_FRAME, MAX_TAGGED_PLAINTEXT)
    };

    if body.len() < frame {
        let mut tagged = Vec::with_capacity(body.len() + 1);
        tagged.push(frame_tag::WHOLE);
        tagged.extend_from_slice(body);
        return Ok(vec![tagged]);
    }

    let payload = (frame - 1 - PART_HEADER_LEN)
        .max(body.len().div_ceil(MAX_PARTS))
        .min(MAX_CHUNK_PAYLOAD);
    let part_count = body.len().div_ceil(payload);
    if part_count > MAX_PARTS {
        return Err(ChannelError::MessageTooLarge(body.len()));
    }
    let msg_id = NEXT_MSG_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    Ok(body
        .chunks(payload)
        .enumerate()
        .map(|(idx, chunk)| {
            let mut tagged = Vec::with_capacity(1 + PART_HEADER_LEN + chunk.len());
            tagged.push(frame_tag::PART);
            tagged.extend_from_slice(&msg_id.to_be_bytes());
            tagged.extend_from_slice(&(idx as u16).to_be_bytes());
            tagged.extend_from_slice(&(part_count as u16).to_be_bytes());
            tagged.extend_from_slice(chunk);
            tagged
        })
        .collect())
}

/// Whether a tagged frame has more of its message queued behind it, which is
/// what lets a drainer release a whole message as a unit.
///
/// The tag alone cannot answer this: the *last* piece of a chunked message is
/// tagged `PART` like every other, so a drainer keying on the tag never sees a
/// message end while chunked messages keep arriving back to back.
pub(crate) fn has_more_parts(tagged: &[u8]) -> bool {
    if tagged.first() != Some(&frame_tag::PART) || tagged.len() < 1 + PART_HEADER_LEN {
        return false;
    }
    let idx = u16::from_be_bytes([tagged[5], tagged[6]]);
    let count = u16::from_be_bytes([tagged[7], tagged[8]]);
    idx.saturating_add(1) < count
}

/// The chunked message id a tagged `PART` frame belongs to, if it is one.
pub(crate) fn part_message_id(tagged: &[u8]) -> Option<u32> {
    if tagged.first() != Some(&frame_tag::PART) || tagged.len() < 1 + PART_HEADER_LEN {
        return None;
    }
    Some(u32::from_be_bytes([
        tagged[1], tagged[2], tagged[3], tagged[4],
    ]))
}

/// Reassembles chunked messages on the receive side.
///
/// One per receiver. A `WHOLE` frame yields its body at once; `PART` frames
/// accumulate by message id until every piece has arrived. Scheduling
/// deliberately interleaves urgent frames between the pieces of large
/// messages, so partials are tracked independently by id.
#[derive(Default)]
pub struct Reassembler {
    in_progress: HashMap<u32, Partial>,
    buffered_bytes: usize,
}

struct Partial {
    part_count: u16,
    parts: Vec<Option<Vec<u8>>>,
    received: u16,
}

impl Reassembler {
    /// Give up on a chunked message whose remaining pieces will never come —
    /// a fresh one cut off by a connection that dropped.
    pub fn forget(&mut self, msg_id: u32) {
        if let Some(partial) = self.in_progress.remove(&msg_id) {
            let held: usize = partial.parts.iter().flatten().map(Vec::len).sum();
            self.buffered_bytes -= held;
        }
    }

    /// Feed one tagged plaintext frame: the encoded body of a message it
    /// completes, or `None` while pieces are still missing.
    pub fn accept(&mut self, tagged: &[u8]) -> Result<Option<Vec<u8>>, ChannelError> {
        let (&tag, rest) = tagged
            .split_first()
            .ok_or_else(|| ChannelError::Noise("empty frame".into()))?;
        match tag {
            frame_tag::WHOLE => Ok(Some(rest.to_vec())),
            frame_tag::PART => self.accept_part(rest),
            other => Err(ChannelError::Noise(format!("unknown frame tag {other}"))),
        }
    }

    fn accept_part(&mut self, rest: &[u8]) -> Result<Option<Vec<u8>>, ChannelError> {
        if rest.len() < PART_HEADER_LEN {
            return Err(ChannelError::Noise("truncated chunk header".into()));
        }
        let msg_id = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let part_idx = u16::from_be_bytes([rest[4], rest[5]]);
        let part_count = u16::from_be_bytes([rest[6], rest[7]]);
        let chunk = &rest[PART_HEADER_LEN..];

        if part_count == 0 || part_idx >= part_count {
            return Err(ChannelError::Noise("bad chunk indices".into()));
        }
        if usize::from(part_count) > MAX_PARTS || chunk.len() > MAX_ACCEPTED_CHUNK_PAYLOAD {
            return Err(ChannelError::MessageTooLarge(
                usize::from(part_count) * MAX_CHUNK_PAYLOAD,
            ));
        }
        if !self.in_progress.contains_key(&msg_id) {
            if self.in_progress.len() >= MAX_IN_FLIGHT_MESSAGES {
                return Err(ChannelError::Noise(
                    "too many chunked messages in flight".into(),
                ));
            }
            self.in_progress.insert(
                msg_id,
                Partial {
                    part_count,
                    parts: (0..part_count).map(|_| None).collect(),
                    received: 0,
                },
            );
        }
        let partial = self
            .in_progress
            .get_mut(&msg_id)
            .expect("partial was inserted above");

        if partial.part_count != part_count {
            return Err(ChannelError::Noise("inconsistent chunk count".into()));
        }
        // A duplicate piece (a resend, a reorder) is ignored rather than
        // counted twice.
        if partial.parts[part_idx as usize].is_none() {
            if self.buffered_bytes.saturating_add(chunk.len()) > MAX_MESSAGE_BYTES {
                return Err(ChannelError::MessageTooLarge(
                    self.buffered_bytes + chunk.len(),
                ));
            }
            partial.parts[part_idx as usize] = Some(chunk.to_vec());
            partial.received += 1;
            self.buffered_bytes += chunk.len();
        }

        if partial.received < partial.part_count {
            return Ok(None);
        }

        let partial = self
            .in_progress
            .remove(&msg_id)
            .expect("complete partial is still tracked");
        let total: usize = partial.parts.iter().flatten().map(Vec::len).sum();
        self.buffered_bytes -= total;
        let mut body = Vec::with_capacity(total);
        for part in partial.parts {
            body.extend_from_slice(&part.expect("all parts present when received == count"));
        }
        Ok(Some(body))
    }
}

/// The send half of a link over one sealed connection, unscheduled: each
/// message is chunked, sealed and written before `send` returns. What a
/// handshake-time exchange or a frozen lane wants; scheduled traffic goes
/// through [`crate::PrioritizedTx`].
pub struct SealedTx {
    tx: Box<dyn crate::CarrierTx>,
    seal: Seal,
}

impl SealedTx {
    pub fn new(tx: Box<dyn crate::CarrierTx>, seal: Seal) -> Self {
        Self { tx, seal }
    }

    /// Send one encoded message.
    pub async fn send_body(&mut self, body: &[u8]) -> Result<(), ChannelError> {
        for tagged in chunk(body)? {
            self.tx.send(self.seal.seal(&tagged)?).await?;
        }
        Ok(())
    }

    pub async fn send<E: crate::Envelope>(&mut self, msg: &E) -> Result<(), ChannelError> {
        self.send_body(&msg.encode()?).await
    }
}

/// The receive half of a link over one sealed connection: reads, opens and
/// reassembles, however the peer scheduled its sends.
pub struct SealedRx {
    rx: Box<dyn crate::CarrierRx>,
    seal: Seal,
    reasm: Reassembler,
}

impl SealedRx {
    pub fn new(rx: Box<dyn crate::CarrierRx>, seal: Seal) -> Self {
        Self {
            rx,
            seal,
            reasm: Reassembler::default(),
        }
    }

    /// The next whole encoded message. Pieces of a chunked one yield nothing,
    /// so this reads on until one completes.
    pub async fn recv_body(&mut self) -> Result<Vec<u8>, ChannelError> {
        loop {
            let tagged = self.seal.open(&self.rx.recv().await?)?;
            if let Some(body) = self.reasm.accept(&tagged)? {
                return Ok(body);
            }
        }
    }

    pub async fn recv<E: crate::Envelope>(&mut self) -> Result<E, ChannelError> {
        E::decode(&self.recv_body().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(reassembler: &mut Reassembler, frames: &[Vec<u8>]) -> Option<Vec<u8>> {
        let mut done = None;
        for frame in frames {
            done = reassembler.accept(frame).unwrap().or(done);
        }
        done
    }

    #[test]
    fn a_link_over_one_connection_accepts_chunks_cut_to_the_whole_noise_budget() {
        let body = vec![7u8; 200_000];
        let old_chunk_size = MAX_CHUNK_PAYLOAD + 1;
        let count = body.len().div_ceil(old_chunk_size);
        let mut reassembler = Reassembler::default();
        let mut received = None;
        for (index, chunk) in body.chunks(old_chunk_size).enumerate() {
            let mut tagged = vec![frame_tag::PART];
            tagged.extend_from_slice(&1u32.to_be_bytes());
            tagged.extend_from_slice(&(index as u16).to_be_bytes());
            tagged.extend_from_slice(&(count as u16).to_be_bytes());
            tagged.extend_from_slice(chunk);
            received = reassembler.accept(&tagged).unwrap().or(received);
        }
        assert_eq!(received, Some(body));
    }

    #[test]
    fn a_message_far_larger_than_one_frame_reassembles_in_order() {
        let body: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        let frames = chunk(&body).unwrap();
        assert!(frames.len() > 4);
        assert_eq!(feed(&mut Reassembler::default(), &frames), Some(body));
    }

    #[test]
    fn a_body_one_byte_over_a_whole_frame_is_chunked_and_reassembles() {
        let body = vec![0xAB; MAX_TAGGED_PLAINTEXT];
        let frames = chunk(&body).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(feed(&mut Reassembler::default(), &frames), Some(body));
        assert_eq!(chunk(&vec![0; MAX_TAGGED_PLAINTEXT - 1]).unwrap().len(), 1);
    }

    #[test]
    fn interleaved_chunked_messages_reassemble_independently() {
        let first = vec![0x11; 220_000];
        let second = vec![0x22; 150_000];
        let first_frames = chunk(&first).unwrap();
        let second_frames = chunk(&second).unwrap();
        let whole = chunk(b"heartbeat").unwrap();
        let mut reassembler = Reassembler::default();
        let mut completed = Vec::new();

        assert!(reassembler.accept(&first_frames[0]).unwrap().is_none());
        assert!(reassembler.accept(&second_frames[0]).unwrap().is_none());
        completed.push(reassembler.accept(&whole[0]).unwrap().unwrap());

        for index in 1..first_frames.len().max(second_frames.len()) {
            for frames in [&first_frames, &second_frames] {
                if let Some(frame) = frames.get(index) {
                    if let Some(body) = reassembler.accept(frame).unwrap() {
                        completed.push(body);
                    }
                }
            }
        }

        assert!(completed.contains(&b"heartbeat".to_vec()));
        assert!(completed.contains(&first));
        assert!(completed.contains(&second));
        assert!(reassembler.in_progress.is_empty());
        assert_eq!(reassembler.buffered_bytes, 0);
    }

    #[test]
    fn oversized_chunk_count_is_rejected_before_allocation() {
        let mut tagged = vec![frame_tag::PART];
        tagged.extend_from_slice(&1_u32.to_be_bytes());
        tagged.extend_from_slice(&0_u16.to_be_bytes());
        tagged.extend_from_slice(&((MAX_PARTS + 1) as u16).to_be_bytes());

        let mut reassembler = Reassembler::default();
        assert!(reassembler.accept(&tagged).is_err());
        assert!(reassembler.in_progress.is_empty());
        assert_eq!(reassembler.buffered_bytes, 0);
    }

    #[test]
    fn only_the_last_part_of_a_chunked_message_ends_it() {
        let frames = chunk(&vec![0u8; 3 * MAX_CHUNK_PAYLOAD]).unwrap();
        let (last, rest) = frames.split_last().unwrap();
        assert!(rest.iter().all(|f| has_more_parts(f)));
        assert!(!has_more_parts(last), "the last part is tagged PART too");

        let whole = chunk(&[0u8; 16]).unwrap();
        assert!(!has_more_parts(&whole[0]));
    }

    #[test]
    fn small_frames_reassemble_into_the_same_message() {
        let body: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let frames = chunk_within(&body, || 4096).unwrap();
        assert!(frames.len() > 20);
        assert!(frames.iter().all(|f| f.len() <= 4096));
        assert_eq!(feed(&mut Reassembler::default(), &frames), Some(body));
    }

    #[test]
    fn small_frames_grow_rather_than_exceed_what_a_receiver_accepts() {
        let body = vec![7u8; 16 * 1024 * 1024];
        let frames = chunk_within(&body, || 4096).unwrap();
        assert!(frames.len() <= MAX_PARTS);
        assert_eq!(feed(&mut Reassembler::default(), &frames), Some(body));
    }

    #[test]
    fn a_message_below_the_smallest_frame_is_never_asked_about() {
        let frames =
            chunk_within(b"tiny", || panic!("a small message needs no frame limit")).unwrap();
        assert_eq!(frames.len(), 1);
    }
}
