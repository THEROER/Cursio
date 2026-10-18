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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use cursio_protocol::messages::{MessagesRequest, MessagesResponse};
use cursio_protocol::PeerId;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

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
pub struct Outgoing {
    pub request: MessagesRequest,
    pub reply: oneshot::Sender<Result<MessagesResponse, String>>,
}

/// A phone whose messages can be read now, as the status shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagesPeer {
    pub peer: PeerId,
    pub name: String,
    /// Bumped each time the phone says something changed; a window showing
    /// its messages asks again when this moves.
    pub revision: u64,
}

struct Link {
    name: String,
    tx: mpsc::UnboundedSender<Outgoing>,
    revision: u64,
    /// Which registration this is; see `ShareOutbox` for why a reconnect needs
    /// it.
    token: u64,
}

#[derive(Default)]
pub struct MessagesHub {
    links: Mutex<HashMap<PeerId, Link>>,
    next_token: AtomicU64,
    /// Phone side: how many times the store has changed.
    generation: AtomicU64,
}

pub fn hub() -> &'static MessagesHub {
    static HUB: OnceLock<MessagesHub> = OnceLock::new();
    HUB.get_or_init(MessagesHub::default)
}

impl MessagesHub {
    /// Phone side: the store changed. Every link tells its desktop once.
    pub fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub(crate) fn register(
        &self,
        peer: PeerId,
        name: String,
    ) -> (mpsc::UnboundedReceiver<Outgoing>, u64) {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(
            peer,
            Link {
                name,
                tx,
                revision: 0,
                token,
            },
        );
        (rx, token)
    }

    pub(crate) fn unregister(&self, peer: &PeerId, token: u64) {
        let mut links = self.lock();
        if links.get(peer).is_some_and(|l| l.token == token) {
            links.remove(peer);
        }
    }

    pub(crate) fn bump(&self, peer: &PeerId) {
        if let Some(link) = self.lock().get_mut(peer) {
            link.revision = link.revision.wrapping_add(1);
        }
    }

    /// Phones whose messages can be read now, by name.
    pub fn peers(&self) -> Vec<MessagesPeer> {
        let mut out: Vec<_> = self
            .lock()
            .iter()
            .filter(|(_, link)| !link.tx.is_closed())
            .map(|(peer, link)| MessagesPeer {
                peer: *peer,
                name: link.name.clone(),
                revision: link.revision,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Ask `peer` (an id or a name) and wait for its answer, at most
    /// [`ANSWER_WAIT`].
    pub async fn ask(
        &self,
        peer: &str,
        request: MessagesRequest,
    ) -> Result<MessagesResponse, String> {
        let (reply, answer) = oneshot::channel();
        {
            let links = self.lock();
            let link = links
                .iter()
                .find(|(id, link)| id.to_string() == peer || link.name.eq_ignore_ascii_case(peer))
                .map(|(_, link)| link)
                .ok_or_else(|| {
                    let names: Vec<_> = links.values().map(|l| l.name.as_str()).collect();
                    format!(
                        "no connected phone called {peer} shares its messages; connected: {}",
                        if names.is_empty() {
                            "none".into()
                        } else {
                            names.join(", ")
                        }
                    )
                })?;
            link.tx
                .send(Outgoing { request, reply })
                .map_err(|_| format!("the link to {} has just closed", link.name))?;
        }
        match tokio::time::timeout(ANSWER_WAIT, answer).await {
            Err(_) => Err(format!(
                "{peer} did not answer within {} s",
                ANSWER_WAIT.as_secs()
            )),
            Ok(Err(_)) => Err(format!("the link to {peer} closed before it answered")),
            Ok(Ok(answer)) => answer,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, Link>> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }
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
