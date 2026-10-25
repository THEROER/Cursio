//! Where a phone's messages meet the rest of the node.
//!
//! Two halves with different owners, as with sharing. On a phone, the
//! platform's message store answers requests ([`MessageStore`]) and says when
//! something changed ([`MessagesHub::changed`]). On a desktop, whoever wants a
//! page — the control socket, for the app and `cursio sms` — reaches the link
//! to that phone through [`MessagesHub::ask`], and reads how often each phone
//! has said "look again" through [`MessagesHub::peers`].
//!
//! Process-wide because both callers are: the JNI observer and the control
//! socket hold no handle to any session.

#[cfg(test)]
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use cursio_protocol::messages::{MessagesRequest, MessagesResponse};

use crate::request_hub::{HubKind, RequestHub};

/// Longest a desktop waits for a phone's answer. A page is a provider query on
/// the phone and a few kilobytes on the link; past this the phone is asleep or
/// the link is about to be declared dead, and the window should say so.
pub const ANSWER_WAIT: Duration = Duration::from_secs(12);

/// How a phone answers for its messages. Called on the module's blocking pool:
/// a provider query may take a moment, and nothing else waits on it.
pub trait MessageStore: Send + Sync {
    fn handle(&self, request: &MessagesRequest) -> Result<MessagesResponse, String>;
}

/// One request on its way to a phone, with where its answer goes.
pub type Outgoing = crate::request_hub::Outgoing<MessagesRequest, MessagesResponse>;

/// A phone whose messages can be read now, as the status shows it.
pub type MessagesPeer = crate::request_hub::HubPeer;

pub type MessagesHub = RequestHub<MessagesRequest, MessagesResponse>;

pub fn hub() -> &'static MessagesHub {
    static HUB: OnceLock<MessagesHub> = OnceLock::new();
    HUB.get_or_init(|| {
        RequestHub::new(HubKind {
            wait: ANSWER_WAIT,
            peer: "phone",
            shares: "messages",
        })
    })
}

/// The store a node without one uses in tests, and nothing else: it answers
/// from memory.
#[cfg(test)]
pub(crate) struct MemoryStore(pub Mutex<Vec<cursio_protocol::messages::Conversation>>);

#[cfg(test)]
impl MessageStore for MemoryStore {
    fn handle(&self, request: &MessagesRequest) -> Result<MessagesResponse, String> {
        match request {
            MessagesRequest::ListConversations { limit, .. } => {
                Ok(MessagesResponse::Conversations(
                    self.0
                        .lock()
                        .unwrap()
                        .iter()
                        .take(*limit as usize)
                        .cloned()
                        .collect(),
                ))
            }
            MessagesRequest::MarkRead { .. } => {
                Err("Android lets only the default SMS app mark messages read".into())
            }
            _ => Ok(MessagesResponse::Done),
        }
    }
}

/// Shared for the factory, which hands every link the same store.
pub type SharedStore = Arc<dyn MessageStore>;
