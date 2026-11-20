//! The receive side: delivering each numbered frame once, in order, whichever
//! path it came by.

use crate::seal::{link_tag, part_message_id, Reassembler};
use crate::session::frame::read_u64;
use crate::session::frame::SEQ_LOSSY;
use crate::session::inner::Inner;
use crate::{ChannelError, Envelope};
use std::collections::HashSet;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::task::Poll;
use tokio::sync::mpsc;

/// The session's receiving end: frames from every path, in order, once each.
pub struct ResumableRx<E> {
    pub(super) inner: Arc<Inner>,
    pub(super) inbound: Vec<mpsc::Receiver<(u64, Vec<u8>)>>,
    pub(super) reasm: Vec<Reassembler>,
    /// Chunked fresh messages part-way through reassembly.
    pub(super) lossy_parts: Vec<HashSet<u32>>,
    /// The path the last numbered frame came on.
    pub(super) last_path: Vec<Option<u64>>,
    pub(super) envelope: PhantomData<fn() -> E>,
}

impl<E: Envelope> ResumableRx<E> {
    pub(super) fn new(inner: Arc<Inner>, inbound: Vec<mpsc::Receiver<(u64, Vec<u8>)>>) -> Self {
        let lanes = inbound.len();
        Self {
            inner,
            inbound,
            reasm: (0..lanes).map(|_| Reassembler::default()).collect(),
            lossy_parts: (0..lanes).map(|_| HashSet::new()).collect(),
            last_path: vec![None; lanes],
            envelope: PhantomData,
        }
    }

    /// Handle one frame; `Some` when it completes a message for the session.
    pub(super) fn accept(&mut self, path: u64, tagged: &[u8]) -> Result<Option<E>, ChannelError> {
        let Some((&tag, rest)) = tagged.split_first() else {
            return Err(ChannelError::Other(
                "empty frame on a resumable link".into(),
            ));
        };
        match tag {
            link_tag::SEQ => {
                let lane = rest
                    .first()
                    .and_then(|v| self.inner.classes.decode_lane(*v))
                    .ok_or_else(|| ChannelError::Other("invalid SEQ lane".into()))?;
                if rest.len() < 10 {
                    return Err(ChannelError::Other("truncated SEQ".into()));
                }
                let seq = read_u64(&rest[1..]).expect("length checked");
                let lossy = rest[9] & SEQ_LOSSY != 0;
                let frame = &rest[10..];
                let index = lane.index();
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
                let Some(body) = self.reasm[index].accept(frame)? else {
                    return Ok(None);
                };
                if let Some(id) = part {
                    self.lossy_parts[index].remove(&id);
                }
                if !self.inner.verify_peer() {
                    return Err(ChannelError::Closed);
                }
                E::decode(&body).map(Some)
            }
            link_tag::ACK | link_tag::RESUME => {
                self.inner.acknowledge(rest)?;
                Ok(None)
            }
            other => Err(ChannelError::Other(format!(
                "unknown frame {other:#x} on a resumable link"
            ))),
        }
    }

    /// The next message of the session, from whichever lane has one first,
    /// the most urgent lane winning a tie. [`ChannelError::Closed`] once the
    /// session has ended.
    pub async fn recv(&mut self) -> Result<E, ChannelError> {
        loop {
            let inner = self.inner.clone();
            let changed = inner.changed.notified();
            tokio::pin!(changed);
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
                return Err(ChannelError::Closed);
            }
            let inbound = &mut self.inbound;
            let item = std::future::poll_fn(|cx| {
                for queue in inbound.iter_mut() {
                    if let Poll::Ready(item) = queue.poll_recv(cx) {
                        return Poll::Ready(Some(item));
                    }
                }
                changed.as_mut().poll(cx).map(|()| None)
            })
            .await;
            match item {
                None => continue,
                Some(Some((path, tagged))) => {
                    if let Some(msg) = self.accept(path, &tagged)? {
                        return Ok(msg);
                    }
                }
                Some(None) => return Err(ChannelError::Closed),
            }
        }
    }
}
