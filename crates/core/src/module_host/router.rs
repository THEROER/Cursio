//! Where the reader sends what arrives: each module's messages to that module's
//! bounded inbox, held briefly for one the peer has started and this side has
//! not yet.

use cursio_protocol::{Message, ModuleId};
use cursio_transport::TransportError;
use std::collections::HashMap;
use std::sync::Arc;

/// What the reader holds for one module: where its messages go.
#[derive(Clone)]
pub(super) struct Inbox {
    pub(super) name: &'static str,
    pub(super) tx: crate::queue::Sender<Message>,
}

/// Most messages held for one module that is not running yet.
///
/// Enough for a module's opening exchange; a peer streaming into a module this
/// side never starts cannot grow it past this.
pub(super) const PARKED_PER_MODULE: usize = 256;

#[derive(Default)]
pub(super) struct Routes {
    inboxes: HashMap<ModuleId, Inbox>,
    /// Messages for a module not running here yet, in arrival order.
    pub(super) parked: HashMap<ModuleId, Vec<Message>>,
    error: Option<String>,
}

/// The reader's routing table: each module's messages to that module alone.
///
/// Shared between the reader task, which routes, and the session loop, which
/// adds and removes modules when the link is reconfigured. The lock is held
/// for one lookup and one channel send; the session takes it only when a
/// module starts or stops.
#[derive(Clone, Default)]
pub(crate) struct Router {
    routes: Arc<std::sync::Mutex<Routes>>,
}

impl Router {
    pub(super) fn routes(&self) -> std::sync::MutexGuard<'_, Routes> {
        self.routes.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Deliver `msg` to its module, or hand it back when it is the session's.
    ///
    /// A message for a module not running here is held, up to a bound. The
    /// peer starts a module right after announcing it in a reconfiguration,
    /// and the reader routes its first message before the session loop has
    /// read the announcement and started the module on this side; dropping it
    /// lost that module's opening exchange.
    pub(crate) fn route(&self, msg: Message) -> Option<Message> {
        let Some(id) = msg.module_id() else {
            return Some(msg);
        };
        let mut routes = self.routes();
        if routes.error.is_some() {
            return None;
        }
        match routes.inboxes.get(&id) {
            Some(inbox) => {
                if let Err(error) = deliver(inbox, msg) {
                    routes.error = Some(error.to_string());
                }
            }
            None => {
                let count: usize = routes.parked.values().map(Vec::len).sum();
                let bytes: usize = routes
                    .parked
                    .values()
                    .flatten()
                    .map(crate::queue::message_bytes)
                    .sum();
                if count >= PARKED_PER_MODULE
                    || bytes.saturating_add(crate::queue::message_bytes(&msg))
                        > 64 * 1024 * 1024 + 256
                {
                    routes.error = Some(format!("parked module queue overloaded for {id}"));
                } else {
                    routes.parked.entry(id).or_default().push(msg);
                }
            }
        }
        None
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.routes().error.clone()
    }

    /// Start delivering `id`'s messages to `inbox`.
    ///
    /// `keep_parked` is whether what arrived before now belongs to it: yes
    /// when the peer announced the module first, since it sends nothing for a
    /// module before announcing it; no when this side started it, since then
    /// anything held is from an instance the peer has since stopped.
    pub(super) fn insert(&self, id: ModuleId, inbox: Inbox, keep_parked: bool) {
        let mut routes = self.routes();
        let parked = routes.parked.remove(&id).unwrap_or_default();
        if keep_parked {
            for msg in parked {
                if let Err(error) = deliver(&inbox, msg) {
                    routes.error = Some(error.to_string());
                    break;
                }
            }
        }
        if routes.inboxes.insert(id, inbox).is_some() {
            tracing::warn!(module = %id, "two modules claim one id; the later one receives");
        }
    }

    pub(super) fn remove(&self, id: ModuleId) {
        let mut routes = self.routes();
        routes.inboxes.remove(&id);
        routes.parked.remove(&id);
    }
}

/// Only an overflowing inbox ends the session. A module that has already
/// ended (its inbox closed, not failed) cannot act on anything sent after it,
/// and ending every other module's link over that would turn one module's
/// exit into a reconnect.
pub(super) fn deliver(inbox: &Inbox, msg: Message) -> Result<(), TransportError> {
    if inbox.tx.send(msg).is_ok() {
        return Ok(());
    }
    if inbox.tx.stats().failed {
        return Err(TransportError::Other(format!(
            "{} receive queue overloaded",
            inbox.name
        )));
    }
    tracing::debug!(module = inbox.name, "message for a module that has ended");
    Ok(())
}

pub(super) fn module_inbox(
    name: &'static str,
) -> (
    crate::queue::Sender<Message>,
    crate::queue::Receiver<Message>,
) {
    crate::queue::channel(
        name,
        4096,
        64 * 1024 * 1024 + 256,
        crate::queue::message_bytes,
        |_, _| false,
    )
}
