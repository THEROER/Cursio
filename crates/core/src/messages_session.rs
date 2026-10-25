//! The messages module on one link.
//!
//! Asymmetric, unlike most modules: a phone has the messages and a desktop
//! has the screen to read them on. The phone advertises `Source` and answers
//! requests from its [`MessageStore`]; a desktop advertises `Sink`, carries
//! requests from [`hub`] to the phone and their answers back. Two desktops
//! have nothing to say to each other here, and negotiation leaves the module
//! off between them.
//!
//! A request off the wire is untrusted input like a file path: the peer is
//! trusted to drive this machine's cursor, not to make the phone send a
//! thousand texts or read a whole history in one page. [`check`] bounds it
//! before the store sees it.

use std::sync::Arc;

use cursio_protocol::messages::{
    MessagesMsg, MessagesRequest, MessagesResponse, MAX_BODY_BYTES, MAX_PAGE,
};
use cursio_protocol::{ModuleAdvert, ModuleId, ModuleRole, Priority};

use crate::exchange_session::{Exchange, ExchangeEngine};
use crate::messages_hub::{hub, MessageStore, MessagesHub};
use crate::module::SessionModule;

/// Version of [`MessagesMsg`]; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

/// Most recipients one message may go to. A group text to more is a mailing
/// list, and a request for one is more likely a mistake than a message.
const MAX_RECIPIENTS: usize = 20;

/// Bound a request from the peer before the store sees it.
pub fn check(request: MessagesRequest) -> Result<MessagesRequest, String> {
    match request {
        MessagesRequest::ListConversations { limit, before_ms } => {
            Ok(MessagesRequest::ListConversations {
                limit: limit.clamp(1, MAX_PAGE),
                before_ms,
            })
        }
        MessagesRequest::ListMessages {
            thread_id,
            limit,
            before_ms,
        } => Ok(MessagesRequest::ListMessages {
            thread_id,
            limit: limit.clamp(1, MAX_PAGE),
            before_ms,
        }),
        MessagesRequest::Send { addresses, body } => {
            if body.trim().is_empty() {
                return Err("the message is empty".into());
            }
            if body.len() > MAX_BODY_BYTES {
                return Err(format!(
                    "the message is longer than {} KiB",
                    MAX_BODY_BYTES / 1024
                ));
            }
            if addresses.is_empty() {
                return Err("the message has no recipient".into());
            }
            if addresses.len() > MAX_RECIPIENTS {
                return Err(format!(
                    "a message goes to at most {MAX_RECIPIENTS} recipients"
                ));
            }
            let mut clean = Vec::with_capacity(addresses.len());
            for address in addresses {
                let address = address.trim().to_string();
                let valid = !address.is_empty()
                    && address.len() <= 64
                    && address
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || " +-().#*".contains(c));
                if !valid {
                    return Err(format!("{address:?} is not a phone number"));
                }
                clean.push(address);
            }
            Ok(MessagesRequest::Send {
                addresses: clean,
                body,
            })
        }
        request @ MessagesRequest::MarkRead { .. } => Ok(request),
    }
}

/// Messages as a request-and-answer module.
pub struct Messages;

impl Exchange for Messages {
    type Request = MessagesRequest;
    type Response = MessagesResponse;
    type Store = dyn MessageStore;

    const MODULE: ModuleId = ModuleId::MESSAGES;
    const NAME: &'static str = "messages";
    const NOT_HERE: &'static str = "a peer asked for messages this machine does not have";
    // A provider query or a radio call each: every one is handed to the pool
    // as it comes, so a slow one does not hold the others behind it.
    const MAX_IN_FLIGHT: Option<usize> = None;

    fn hub() -> &'static MessagesHub {
        hub()
    }

    fn answer(
        store: &dyn MessageStore,
        request: &MessagesRequest,
    ) -> Result<MessagesResponse, String> {
        store.handle(request)
    }

    fn check(request: MessagesRequest) -> Result<MessagesRequest, String> {
        check(request)
    }

    fn priority(_msg: &MessagesMsg) -> Priority {
        MessagesMsg::PRIORITY
    }

    fn asking_started(peer_name: &str) {
        tracing::info!(peer = %peer_name, "this phone's messages can be read here");
    }
}

/// The messages module on one link.
pub type MessagesEngine = ExchangeEngine<Messages>;

/// Builds [`MessagesEngine`]: a phone hands it its store, a desktop asks.
pub struct MessagesFactory {
    pub store: Option<Arc<dyn MessageStore>>,
    /// This machine reads a phone's messages.
    pub view: bool,
}

impl crate::module::ModuleFactory for MessagesFactory {
    fn id(&self) -> ModuleId {
        ModuleId::MESSAGES
    }

    fn advert(&self) -> Option<ModuleAdvert> {
        let role = match (self.store.is_some(), self.view) {
            (true, true) => ModuleRole::Both,
            (true, false) => ModuleRole::Source,
            (false, true) => ModuleRole::Sink,
            (false, false) => return None,
        };
        Some(ModuleAdvert::new(ModuleId::MESSAGES, VERSION, role))
    }

    fn supported(&self) -> bool {
        self.store.is_some() || self.view
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(MessagesEngine::new(
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

    type Answer = Result<MessagesResponse, String>;
    use crate::messages_hub::MemoryStore;
    use cursio_protocol::messages::Conversation;
    use std::sync::Mutex;

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

    fn conversation(thread_id: i64) -> Conversation {
        Conversation {
            thread_id,
            addresses: vec!["+380501234567".into()],
            names: vec!["Mum".into()],
            snippet: "call me".into(),
            date_ms: 1_700_000_000_000 - thread_id,
            unread: true,
        }
    }

    /// Run `request` from a desktop engine through a phone engine and back.
    async fn round_trip(peer_byte: u8, request: MessagesRequest) -> Answer {
        let phone_id = PeerId::from_bytes([peer_byte; 16]);
        let store = Arc::new(MemoryStore(Mutex::new(vec![
            conversation(1),
            conversation(2),
            conversation(3),
        ])));
        let mut desktop = MessagesEngine::new(phone_id, format!("phone-{peer_byte}"), None, true);
        let mut phone = MessagesEngine::new(
            PeerId::from_bytes([0xEE; 16]),
            "desk".into(),
            Some(store),
            false,
        );
        let ask = tokio::spawn(async move { hub().ask(&phone_id.to_string(), request).await });

        let mut to_phone = RecordingTx::default();
        let mut to_desktop = RecordingTx::default();
        for _ in 0..200 {
            desktop.pump(&mut to_phone).await.unwrap();
            for msg in to_phone.sent.drain(..) {
                assert!(phone.handle_message(&msg));
            }
            phone.pump(&mut to_desktop).await.unwrap();
            for msg in to_desktop.sent.drain(..) {
                assert!(desktop.handle_message(&msg));
            }
            if ask.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        ask.await.unwrap()
    }

    #[tokio::test]
    async fn a_desktop_reads_the_phones_conversations_through_the_link() {
        let answer = round_trip(
            0x51,
            MessagesRequest::ListConversations {
                limit: 2,
                before_ms: None,
            },
        )
        .await;
        match answer {
            Ok(MessagesResponse::Conversations(list)) => {
                assert_eq!(list.len(), 2);
                assert_eq!(list[0].names, vec!["Mum".to_string()]);
            }
            other => panic!("unexpected answer {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_refusal_reaches_the_desktop_with_its_reason() {
        let answer = round_trip(0x52, MessagesRequest::MarkRead { thread_id: 1 }).await;
        assert_eq!(
            answer,
            Err("Android lets only the default SMS app mark messages read".into())
        );
    }

    #[tokio::test]
    async fn a_send_to_something_that_is_not_a_number_never_reaches_the_store() {
        let answer = round_trip(
            0x53,
            MessagesRequest::Send {
                addresses: vec!["+38050; rm -rf".into()],
                body: "hi".into(),
            },
        )
        .await;
        assert!(answer.unwrap_err().contains("is not a phone number"));
    }

    #[tokio::test]
    async fn a_phone_that_is_not_connected_is_named_in_the_refusal() {
        let err = hub()
            .ask(
                "nobody",
                MessagesRequest::ListConversations {
                    limit: 1,
                    before_ms: None,
                },
            )
            .await
            .unwrap_err();
        assert!(err.contains("no connected phone called nobody"));
    }

    #[test]
    fn a_page_request_is_bounded_before_the_store_sees_it() {
        let checked = check(MessagesRequest::ListMessages {
            thread_id: 1,
            limit: 1_000_000,
            before_ms: None,
        })
        .unwrap();
        assert_eq!(
            checked,
            MessagesRequest::ListMessages {
                thread_id: 1,
                limit: MAX_PAGE,
                before_ms: None
            }
        );
        assert!(check(MessagesRequest::Send {
            addresses: vec!["+1 (555) 010-9999".into()],
            body: " ".into()
        })
        .is_err());
        assert!(check(MessagesRequest::Send {
            addresses: vec![],
            body: "hi".into()
        })
        .is_err());
    }

    #[test]
    fn a_change_on_the_phone_is_told_to_the_desktop() {
        let peer = PeerId::from_bytes([0x54; 16]);
        let mut desktop = MessagesEngine::new(peer, "phone-54".into(), None, true);
        let before = hub()
            .peers()
            .into_iter()
            .find(|p| p.peer == peer)
            .unwrap()
            .revision;
        desktop.on_message(MessagesMsg::Changed);
        let after = hub()
            .peers()
            .into_iter()
            .find(|p| p.peer == peer)
            .unwrap()
            .revision;
        assert_eq!(after, before + 1);
    }

    #[test]
    fn only_a_machine_with_a_store_or_a_screen_offers_the_module() {
        use crate::module::ModuleFactory;
        let none = MessagesFactory {
            store: None,
            view: false,
        };
        assert!(none.advert().is_none());
        let desk = MessagesFactory {
            store: None,
            view: true,
        };
        assert_eq!(desk.advert().unwrap().role, ModuleRole::Sink);
    }
}
