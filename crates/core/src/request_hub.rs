//! A request-and-answer module's meeting point with the rest of the node.
//!
//! Messages and the gallery work the same way: the module on each link
//! registers here when it starts, anything on this machine that wants an
//! answer from a peer — the control socket, a phone's in-process window —
//! [`ask`](RequestHub::ask)s by the peer's name or id, and the link carries the
//! request and hands the answer back. The peer's "look again" is counted per
//! link ([`peers`](RequestHub::peers) shows it as a revision), and this
//! machine's own changes as a generation every link announces once.
//!
//! Process-wide because those callers are: the control socket and the JNI
//! change observers hold no handle to any session. Each kind of module keeps
//! its own hub, with its own request vocabulary and patience.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use cursio_protocol::PeerId;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// One request on its way to a peer, with where its answer goes.
pub struct Outgoing<Req, Resp> {
    pub request: Req,
    pub reply: oneshot::Sender<Result<Resp, String>>,
}

/// A peer that can be asked now, as the status shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubPeer {
    pub peer: PeerId,
    pub name: String,
    /// Bumped each time the peer says something changed; a window showing it
    /// asks again when this moves.
    pub revision: u64,
}

struct Link<Req, Resp> {
    name: String,
    tx: mpsc::UnboundedSender<Outgoing<Req, Resp>>,
    revision: u64,
    /// Which registration this is: a link that reconnected registers again
    /// before the old one has unregistered, and the old one must not then take
    /// the new one away.
    token: u64,
}

/// What a hub is for, in the words its errors use.
pub struct HubKind {
    /// Longest a caller waits for one answer; past it the peer is asleep or
    /// its link is about to be declared dead, and the window should say so.
    pub wait: Duration,
    /// What answers: "phone", "machine".
    pub peer: &'static str,
    /// What it shares: "messages", "gallery".
    pub shares: &'static str,
}

pub struct RequestHub<Req, Resp> {
    kind: HubKind,
    links: Mutex<HashMap<PeerId, Link<Req, Resp>>>,
    next_token: AtomicU64,
    /// How many times this machine's own store has changed.
    generation: AtomicU64,
}

impl<Req, Resp> RequestHub<Req, Resp> {
    pub fn new(kind: HubKind) -> Self {
        Self {
            kind,
            links: Mutex::default(),
            next_token: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        }
    }

    /// This machine's store changed; every link tells its peer once.
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
    ) -> (mpsc::UnboundedReceiver<Outgoing<Req, Resp>>, u64) {
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

    /// Peers that can be asked now, by name.
    pub fn peers(&self) -> Vec<HubPeer> {
        let mut out: Vec<_> = self
            .lock()
            .iter()
            .filter(|(_, link)| !link.tx.is_closed())
            .map(|(peer, link)| HubPeer {
                peer: *peer,
                name: link.name.clone(),
                revision: link.revision,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Ask `peer` (an id or a name) and wait for its answer, at most the
    /// hub's patience.
    pub async fn ask(&self, peer: &str, request: Req) -> Result<Resp, String> {
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
                        "no connected {} called {peer} shares its {}; connected: {}",
                        self.kind.peer,
                        self.kind.shares,
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
        match tokio::time::timeout(self.kind.wait, answer).await {
            Err(_) => Err(format!(
                "{peer} did not answer within {} s",
                self.kind.wait.as_secs()
            )),
            Ok(Err(_)) => Err(format!("the link to {peer} closed before it answered")),
            Ok(Ok(answer)) => answer,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, Link<Req, Resp>>> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> RequestHub<u32, u32> {
        RequestHub::new(HubKind {
            wait: Duration::from_millis(50),
            peer: "phone",
            shares: "messages",
        })
    }

    #[tokio::test]
    async fn asking_a_peer_nobody_registered_names_who_is_connected() {
        let hub = hub();
        let _link = hub.register(PeerId::new_v4(), "Pixel".into());
        let err = hub.ask("dark", 1).await.unwrap_err();
        assert_eq!(
            err,
            "no connected phone called dark shares its messages; connected: Pixel"
        );
    }

    #[tokio::test]
    async fn an_answer_reaches_the_asker_by_name_or_id() {
        let hub = std::sync::Arc::new(hub());
        let id = PeerId::new_v4();
        let (mut rx, _) = hub.register(id, "Pixel".into());
        tokio::spawn(async move {
            while let Some(out) = rx.recv().await {
                let _ = out.reply.send(Ok(out.request * 2));
            }
        });
        assert_eq!(hub.ask("pixel", 21).await, Ok(42));
        assert_eq!(hub.ask(&id.to_string(), 2).await, Ok(4));
    }

    #[test]
    fn a_stale_registration_cannot_unregister_the_link_that_replaced_it() {
        let hub = hub();
        let peer = PeerId::new_v4();
        let (_old, old_token) = hub.register(peer, "Pixel".into());
        let (_new, _) = hub.register(peer, "Pixel".into());
        hub.unregister(&peer, old_token);
        assert_eq!(hub.peers().len(), 1);
    }
}
