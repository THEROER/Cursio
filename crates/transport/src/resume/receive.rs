//! The receive side: delivering each numbered frame once, in order, whichever
//! path it came by.

use crate::crypto::{link_tag, part_message_id, Reassembler};
use crate::lane::Lane;
use crate::resume::frame::read_u64;
use crate::resume::frame::SEQ_LOSSY;
use crate::resume::inner::Inner;
use crate::{TransportError, TransportRx};
use async_trait::async_trait;
use cursio_protocol::Message;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

/// The session's receiving end: frames from every path, in order, once each.
pub struct ResumableRx {
    pub(super) inner: Arc<Inner>,
    pub(super) inbound: [mpsc::Receiver<(u64, Vec<u8>)>; Lane::COUNT],
    pub(super) reasm: [Reassembler; Lane::COUNT],
    /// Chunked lossy messages part-way through reassembly.
    pub(super) lossy_parts: [HashSet<u32>; Lane::COUNT],
    /// The path the last numbered frame came on.
    pub(super) last_path: [Option<u64>; Lane::COUNT],
}

impl ResumableRx {
    /// Handle one frame; `Some` when it completes a message for the session.
    pub(super) fn accept(
        &mut self,
        path: u64,
        tagged: &[u8],
    ) -> Result<Option<Message>, TransportError> {
        let Some((&tag, rest)) = tagged.split_first() else {
            return Err(TransportError::Other(
                "empty frame on a resumable link".into(),
            ));
        };
        match tag {
            link_tag::SEQ => {
                let lane = rest
                    .first()
                    .and_then(|v| Lane::decode(*v))
                    .ok_or_else(|| TransportError::Other("invalid SEQ lane".into()))?;
                if rest.len() < 10 {
                    return Err(TransportError::Other("truncated SEQ".into()));
                }
                let seq = read_u64(&rest[1..]).unwrap();
                let lossy = rest[9] & SEQ_LOSSY != 0;
                let frame = &rest[10..];
                let index = lane as usize;
                {
                    let mut state = self.inner.lock();
                    let delivery = &mut state.lanes[index];
                    if seq <= delivery.delivered {
                        return Ok(None);
                    }
                    delivery.delivered = seq;
                    state.ack_due = true;
                }
                if self.last_path[index].is_some_and(|last| last != path) {
                    for id in self.lossy_parts[index].drain() {
                        self.reasm[index].forget(id);
                    }
                }
                self.last_path[index] = Some(path);
                let part = part_message_id(frame);
                if lossy {
                    if let Some(id) = part {
                        self.lossy_parts[index].insert(id);
                    }
                }
                let message = self.reasm[index].accept(frame)?;
                if message.is_some() {
                    if let Some(id) = part {
                        self.lossy_parts[index].remove(&id);
                    }
                }
                Ok(message)
            }
            link_tag::ACK | link_tag::RESUME => {
                self.inner.acknowledge(rest)?;
                Ok(None)
            }
            other => Err(TransportError::Other(format!(
                "unknown frame {other:#x} on a resumable link"
            ))),
        }
    }
}

#[async_trait]
impl TransportRx for ResumableRx {
    async fn recv(&mut self) -> Result<Message, TransportError> {
        loop {
            let inner = self.inner.clone();
            let changed = inner.changed.notified();
            let item = self
                .inbound
                .iter_mut()
                .find_map(|queue| queue.try_recv().ok());
            if let Some((path, tagged)) = item {
                if let Some(msg) = self.accept(path, &tagged)? {
                    return Ok(msg);
                }
                continue;
            }
            if self.inner.lock().ended.is_some() {
                return Err(TransportError::Closed);
            }
            let [interactive, audio, video, bulk] = &mut self.inbound;
            let item = tokio::select! {
                biased;
                item = interactive.recv() => item,
                item = audio.recv() => item,
                item = video.recv() => item,
                item = bulk.recv() => item,
                () = changed => continue,
            };
            if let Some((path, tagged)) = item {
                if let Some(msg) = self.accept(path, &tagged)? {
                    return Ok(msg);
                }
            } else {
                return Err(TransportError::Closed);
            }
        }
    }
}
