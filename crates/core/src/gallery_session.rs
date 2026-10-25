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

use std::sync::Arc;

use cursio_protocol::gallery::{
    GalleryMsg, GalleryRequest, GalleryResponse, MAX_PAGE, MAX_SLICE, MAX_THUMBNAIL_PX,
};
use cursio_protocol::{ModuleAdvert, ModuleId, ModuleRole, Priority};

use crate::exchange_session::{Exchange, ExchangeEngine};
use crate::gallery_hub::{hub, GalleryHub, MediaStore};
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

/// The gallery as a request-and-answer module.
pub struct Gallery;

impl Exchange for Gallery {
    type Request = GalleryRequest;
    type Response = GalleryResponse;
    type Store = dyn MediaStore;

    const MODULE: ModuleId = ModuleId::GALLERY;
    const NAME: &'static str = "gallery";
    const NOT_HERE: &'static str = "a peer asked for a gallery this machine does not share";
    const MAX_IN_FLIGHT: Option<usize> = Some(MAX_IN_FLIGHT);

    fn hub() -> &'static GalleryHub {
        hub()
    }

    fn answer(store: &dyn MediaStore, request: &GalleryRequest) -> Result<GalleryResponse, String> {
        store.handle(request)
    }

    fn check(request: GalleryRequest) -> Result<GalleryRequest, String> {
        check(request)
    }

    fn priority(msg: &GalleryMsg) -> Priority {
        msg.priority()
    }
}

/// The gallery module on one link.
pub type GalleryEngine = ExchangeEngine<Gallery>;

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
    use async_trait::async_trait;
    use cursio_protocol::Message;
    use cursio_protocol::PeerId;
    use cursio_transport::TransportError;
    use cursio_transport::TransportTx;

    type Answer = Result<GalleryResponse, String>;
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
