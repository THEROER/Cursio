//! The gallery module on one link.
//!
//! Symmetric: every machine both answers for its own photos (from its
//! [`MediaStore`]) and asks for its peer's (for [`hub`]). The shape is the
//! messages module's — ids on requests, answers matched back, only a
//! "changed" sent unasked — with a different priority per answer; see
//! [`GalleryMsg::priority`].
//!
//! A request off the wire is untrusted: an id is a string the peer could have
//! made up, and the store resolves it only within its own gallery. Sizes are
//! bounded here by [`check`] before the store sees them.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use cursio_protocol::gallery::{
    GalleryMsg, GalleryRequest, GalleryResponse, MAX_PAGE, MAX_SLICE, MAX_THUMBNAIL_PX,
};
use cursio_protocol::{Message, ModuleAdvert, ModuleId, ModuleRole, PeerId};
use cursio_transport::{TransportError, TransportTx};
use tokio::sync::{mpsc, oneshot};

use crate::gallery_hub::{hub, MediaStore, Outgoing};
use crate::module::SessionModule;

/// Version of [`GalleryMsg`]; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

/// Longest an id may be. A MediaStore URI or a path inside Pictures is well
/// under this; a longer one is not an id this machine issued.
const MAX_ID_BYTES: usize = 1024;

/// Most requests the store works on at once. A grid asks for every visible
/// thumbnail together, and each is a photo decode; this keeps a phone from
/// decoding fifty at once while the link can carry a few at a time anyway.
const MAX_IN_FLIGHT: usize = 4;

type Answer = Result<GalleryResponse, String>;

struct Answering {
    store: Arc<dyn MediaStore>,
    told: u64,
    incoming: std::collections::VecDeque<(u32, GalleryRequest)>,
    in_flight: usize,
    done_tx: mpsc::UnboundedSender<(u32, Answer)>,
    done_rx: mpsc::UnboundedReceiver<(u32, Answer)>,
}

struct Asking {
    outbox: mpsc::UnboundedReceiver<Outgoing>,
    token: u64,
    next_id: u32,
    waiting: HashMap<u32, oneshot::Sender<Answer>>,
}

pub struct GalleryEngine {
    peer: PeerId,
    peer_name: String,
    answering: Option<Answering>,
    asking: Option<Asking>,
}

impl GalleryEngine {
    pub fn new(
        peer: PeerId,
        peer_name: String,
        store: Option<Arc<dyn MediaStore>>,
        asks: bool,
    ) -> Self {
        let answering = store.map(|store| {
            let (done_tx, done_rx) = mpsc::unbounded_channel();
            Answering {
                store,
                told: hub().generation(),
                incoming: Default::default(),
                in_flight: 0,
                done_tx,
                done_rx,
            }
        });
        let asking = asks.then(|| {
            let (outbox, token) = hub().register(peer, peer_name.clone());
            Asking {
                outbox,
                token,
                next_id: 1,
                waiting: HashMap::new(),
            }
        });
        GalleryEngine {
            peer,
            peer_name,
            answering,
            asking,
        }
    }

    fn on_message(&mut self, msg: GalleryMsg) {
        match msg {
            GalleryMsg::Request { id, request } => match &mut self.answering {
                Some(answering) => answering.incoming.push_back((id, request)),
                None => {
                    tracing::debug!(peer = %self.peer_name, "a peer asked for a gallery this machine does not share")
                }
            },
            GalleryMsg::Response { id, response } => {
                if let Some(reply) = self.asking.as_mut().and_then(|a| a.waiting.remove(&id)) {
                    // The window may have scrolled past; not an error.
                    let _ = reply.send(response);
                }
            }
            GalleryMsg::Changed => {
                if self.asking.is_some() {
                    hub().bump(&self.peer);
                }
            }
        }
    }
}

/// Bound a request from the peer before the store sees it.
pub fn check(request: GalleryRequest) -> Result<GalleryRequest, String> {
    let id_ok = |id: &str| !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.contains('\0');
    match request {
        GalleryRequest::List {
            limit,
            after,
            filter,
        } => {
            if after.as_ref().is_some_and(|mark| !id_ok(&mark.id)) {
                return Err("not an item of this gallery".into());
            }
            Ok(GalleryRequest::List {
                limit: limit.clamp(1, MAX_PAGE),
                after,
                filter,
            })
        }
        GalleryRequest::Thumbnail { id, max_px } if id_ok(&id) => Ok(GalleryRequest::Thumbnail {
            id,
            max_px: max_px.clamp(32, MAX_THUMBNAIL_PX),
        }),
        GalleryRequest::Read { id, offset, len } if id_ok(&id) => Ok(GalleryRequest::Read {
            id,
            offset,
            len: len.clamp(1, MAX_SLICE),
        }),
        _ => Err("not an item of this gallery".into()),
    }
}

async fn send(tx: &mut dyn TransportTx, msg: &GalleryMsg) -> Result<(), TransportError> {
    let message = Message::module(ModuleId::GALLERY, msg.priority(), msg)?;
    tx.send(&message).await
}

#[async_trait]
impl SessionModule for GalleryEngine {
    fn name(&self) -> &'static str {
        "gallery"
    }

    fn id(&self) -> ModuleId {
        ModuleId::GALLERY
    }

    async fn start(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if let Some(answering) = &mut self.answering {
            while answering.in_flight < MAX_IN_FLIGHT {
                let Some((id, request)) = answering.incoming.pop_front() else {
                    break;
                };
                let request = match check(request) {
                    Ok(request) => request,
                    Err(reason) => {
                        let _ = answering.done_tx.send((id, Err(reason)));
                        continue;
                    }
                };
                let store = answering.store.clone();
                let done = answering.done_tx.clone();
                answering.in_flight += 1;
                tokio::task::spawn_blocking(move || {
                    let _ = done.send((id, store.handle(&request)));
                });
            }
            while let Ok((id, response)) = answering.done_rx.try_recv() {
                answering.in_flight = answering.in_flight.saturating_sub(1);
                if let Err(reason) = &response {
                    tracing::debug!(id, %reason, "a gallery request was refused");
                }
                send(tx, &GalleryMsg::Response { id, response }).await?;
            }
            let generation = hub().generation();
            if generation != answering.told {
                answering.told = generation;
                send(tx, &GalleryMsg::Changed).await?;
            }
        }
        if let Some(asking) = &mut self.asking {
            asking.waiting.retain(|_, reply| !reply.is_closed());
            while let Ok(Outgoing { request, reply }) = asking.outbox.try_recv() {
                let id = asking.next_id;
                asking.next_id = asking.next_id.wrapping_add(1).max(1);
                asking.waiting.insert(id, reply);
                send(tx, &GalleryMsg::Request { id, request }).await?;
            }
        }
        Ok(())
    }

    fn has_pending_work(&self) -> bool {
        self.answering
            .as_ref()
            .is_some_and(|a| a.in_flight > 0 || !a.incoming.is_empty())
            || self.asking.as_ref().is_some_and(|a| !a.waiting.is_empty())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<GalleryMsg>(ModuleId::GALLERY) {
            Ok(Some(msg)) => {
                self.on_message(msg);
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable gallery message from peer");
                true
            }
        }
    }
}

impl Drop for GalleryEngine {
    fn drop(&mut self) {
        if let Some(asking) = &self.asking {
            hub().unregister(&self.peer, asking.token);
        }
    }
}

/// Builds [`GalleryEngine`]: `store` answers for this machine's photos, `view`
/// lets a window here browse the peer's.
pub struct GalleryFactory {
    pub store: Option<Arc<dyn MediaStore>>,
    pub view: bool,
}

impl crate::module::ModuleFactory for GalleryFactory {
    fn id(&self) -> ModuleId {
        ModuleId::GALLERY
    }

    fn advert(&self) -> Option<ModuleAdvert> {
        let role = match (self.store.is_some(), self.view) {
            (true, true) => ModuleRole::Both,
            (true, false) => ModuleRole::Source,
            (false, true) => ModuleRole::Sink,
            (false, false) => return None,
        };
        Some(ModuleAdvert::new(ModuleId::GALLERY, VERSION, role))
    }

    fn supported(&self) -> bool {
        self.store.is_some() || self.view
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(GalleryEngine::new(
            ctx.peer.peer_id,
            ctx.peer_name().to_string(),
            self.store.clone(),
            self.view,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::gallery::{MediaFilter, MediaItem, MediaKind};

    #[derive(Default)]
    struct RecordingTx {
        sent: Vec<Message>,
    }

    #[async_trait]
    impl TransportTx for RecordingTx {
        async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
            self.sent.push(msg.clone());
            Ok(())
        }
    }

    struct Roll;

    impl MediaStore for Roll {
        fn handle(&self, request: &GalleryRequest) -> Result<GalleryResponse, String> {
            match request {
                GalleryRequest::List { limit, .. } => Ok(GalleryResponse::Items(
                    (0..*limit)
                        .map(|i| MediaItem {
                            id: format!("{i}"),
                            name: format!("IMG_{i}.jpg"),
                            kind: MediaKind::Image,
                            taken_ms: 1_700_000_000_000 - i as i64,
                            size: 10,
                            width: None,
                            height: None,
                            duration_ms: None,
                            mime: "image/jpeg".into(),
                        })
                        .collect(),
                )),
                GalleryRequest::Thumbnail { .. } => Ok(GalleryResponse::Thumbnail(None)),
                GalleryRequest::Read { offset, len, .. } => {
                    let file = b"0123456789";
                    let start = (*offset as usize).min(file.len());
                    let end = (start + *len as usize).min(file.len());
                    Ok(GalleryResponse::Slice(file[start..end].to_vec()))
                }
            }
        }
    }

    async fn round_trip(peer_byte: u8, request: GalleryRequest) -> Answer {
        let other = PeerId::from_bytes([peer_byte; 16]);
        let mut here = GalleryEngine::new(other, format!("pc-{peer_byte}"), None, true);
        let mut there = GalleryEngine::new(
            PeerId::from_bytes([0xEE; 16]),
            "phone".into(),
            Some(Arc::new(Roll)),
            false,
        );
        let ask = tokio::spawn(async move { hub().ask(&other.to_string(), request).await });
        let mut to_there = RecordingTx::default();
        let mut to_here = RecordingTx::default();
        for _ in 0..200 {
            here.pump(&mut to_there).await.unwrap();
            for msg in to_there.sent.drain(..) {
                assert!(there.handle_message(&msg));
            }
            there.pump(&mut to_here).await.unwrap();
            for msg in to_here.sent.drain(..) {
                assert!(here.handle_message(&msg));
            }
            if ask.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        ask.await.unwrap()
    }

    #[tokio::test]
    async fn a_machine_lists_its_peers_photos_newest_first() {
        let answer = round_trip(
            0x61,
            GalleryRequest::List {
                limit: 3,
                after: None,
                filter: MediaFilter::All,
            },
        )
        .await;
        match answer {
            Ok(GalleryResponse::Items(items)) => {
                assert_eq!(items.len(), 3);
                assert!(items[0].taken_ms > items[2].taken_ms);
            }
            other => panic!("unexpected answer {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_file_is_read_in_slices_up_to_its_end() {
        let answer = round_trip(
            0x62,
            GalleryRequest::Read {
                id: "1".into(),
                offset: 8,
                len: 100,
            },
        )
        .await;
        assert_eq!(answer, Ok(GalleryResponse::Slice(b"89".to_vec())));
    }

    #[test]
    fn a_request_asking_for_more_than_a_page_or_slice_is_cut_down() {
        let page = check(GalleryRequest::List {
            limit: 10_000,
            after: None,
            filter: MediaFilter::Images,
        })
        .unwrap();
        assert!(matches!(
            page,
            GalleryRequest::List {
                limit: MAX_PAGE,
                ..
            }
        ));
        let slice = check(GalleryRequest::Read {
            id: "x".into(),
            offset: 0,
            len: u32::MAX,
        })
        .unwrap();
        assert!(matches!(slice, GalleryRequest::Read { len: MAX_SLICE, .. }));
    }

    #[test]
    fn an_empty_or_oversized_id_is_refused_before_the_store_sees_it() {
        assert!(check(GalleryRequest::Thumbnail {
            id: String::new(),
            max_px: 256
        })
        .is_err());
        assert!(check(GalleryRequest::Read {
            id: "a".repeat(MAX_ID_BYTES + 1),
            offset: 0,
            len: 10
        })
        .is_err());
    }
}
