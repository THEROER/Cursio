//! Where the capture app's request for a peer's screen meets the link that
//! can serve it.
//!
//! On Linux a remote-screen view is not opened by the link — it is asked for
//! by a capture app, through Cursio's ScreenCast portal backend, which runs on
//! its own task and knows nothing of sessions. The links, for their part, are
//! `!Send` session loops that cannot be called into. So they meet here: every
//! link's viewer publishes its peer's source list and drains the requests
//! queued for its peer; the portal reads the lists and queues the requests.
//! A mutex and queues, because both sides only ever poll.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cursio_protocol::{ScreenSource, SourceId, SourceKind};
use tokio::sync::oneshot;

/// A peer source now published as a node here.
#[derive(Debug)]
pub struct Shown {
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    pub kind: SourceKind,
    pub token: u64,
    /// Fires when the showing ends by itself (the source closed on the peer,
    /// the link went down, a newer request replaced it).
    pub ended: oneshot::Receiver<()>,
}

/// What a capture app asked of a peer's link.
#[derive(Debug)]
pub enum HubRequest {
    Show {
        source: SourceId,
        token: u64,
        reply: oneshot::Sender<Result<Shown, String>>,
    },
    Stop {
        token: u64,
    },
}

struct PeerEntry {
    name: String,
    sources: Vec<ScreenSource>,
    requests: Vec<HubRequest>,
}

/// Shared by every link's viewer and the portal backend.
#[derive(Clone, Default)]
pub struct ScreenHub {
    peers: Arc<Mutex<HashMap<String, PeerEntry>>>,
    next_token: Arc<AtomicU64>,
}

impl ScreenHub {
    /// A link's viewer has (new) sources for `peer`.
    pub fn publish(&self, peer: &str, name: &str, sources: &[ScreenSource]) {
        if let Ok(mut peers) = self.peers.lock() {
            let entry = peers.entry(peer.to_string()).or_insert_with(|| PeerEntry {
                name: name.to_string(),
                sources: Vec::new(),
                requests: Vec::new(),
            });
            entry.name = name.to_string();
            entry.sources = sources.to_vec();
        }
    }

    /// The peer's link is gone, or it refuses to serve views. Requests still
    /// queued for it are answered, so no capture app waits on a dead link.
    pub fn withdraw(&self, peer: &str) {
        let entry = self.peers.lock().ok().and_then(|mut p| p.remove(peer));
        for request in entry.map(|e| e.requests).unwrap_or_default() {
            if let HubRequest::Show { reply, .. } = request {
                let _ = reply.send(Err("the peer disconnected".into()));
            }
        }
    }

    /// What is queued for `peer`'s link, oldest first.
    pub fn take_requests(&self, peer: &str) -> Vec<HubRequest> {
        self.peers
            .lock()
            .ok()
            .and_then(|mut p| p.get_mut(peer).map(|e| std::mem::take(&mut e.requests)))
            .unwrap_or_default()
    }

    /// Every peer with sources, as `(key, name, sources)`, in name order.
    pub fn peers(&self) -> Vec<(String, String, Vec<ScreenSource>)> {
        let mut out: Vec<_> = self
            .peers
            .lock()
            .map(|p| {
                p.iter()
                    .filter(|(_, e)| !e.sources.is_empty())
                    .map(|(k, e)| (k.clone(), e.name.clone(), e.sources.clone()))
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// Ask `peer`'s link to show `source`.
    pub fn show(&self, peer: &str, source: SourceId) -> oneshot::Receiver<Result<Shown, String>> {
        let (reply, answer) = oneshot::channel();
        let token = self.next_token.fetch_add(1, Ordering::Relaxed) + 1;
        let request = HubRequest::Show {
            source,
            token,
            reply,
        };
        match self
            .peers
            .lock()
            .ok()
            .and_then(|mut p| p.get_mut(peer).map(|e| e.requests.push(request)))
        {
            Some(()) => {}
            None => {
                let (reply, answer) = oneshot::channel();
                let _ = reply.send(Err("that peer is no longer connected".into()));
                return answer;
            }
        }
        answer
    }

    /// The capture app is done with showing `token`.
    pub fn stop(&self, peer: &str, token: u64) {
        if let Ok(mut peers) = self.peers.lock() {
            if let Some(entry) = peers.get_mut(peer) {
                entry.requests.push(HubRequest::Stop { token });
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl cursio_window::linux_portal::RemoteScreens for ScreenHub {
    fn peers(&self) -> Vec<cursio_window::linux_portal::PeerScreens> {
        ScreenHub::peers(self)
            .into_iter()
            .map(
                |(peer, name, sources)| cursio_window::linux_portal::PeerScreens {
                    peer,
                    name,
                    sources,
                },
            )
            .collect()
    }

    fn show(
        &self,
        peer: &str,
        source: SourceId,
    ) -> oneshot::Receiver<Result<cursio_window::linux_portal::ShownNode, String>> {
        let answer = ScreenHub::show(self, peer, source);
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let mapped = match answer.await {
                Ok(Ok(shown)) => Ok(cursio_window::linux_portal::ShownNode {
                    node_id: shown.node_id,
                    width: shown.width,
                    height: shown.height,
                    kind: shown.kind,
                    token: shown.token,
                    ended: shown.ended,
                }),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("the peer's link ended before answering".into()),
            };
            let _ = tx.send(mapped);
        });
        rx
    }

    fn stop(&self, peer: &str, token: u64) {
        ScreenHub::stop(self, peer, token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> ScreenSource {
        ScreenSource {
            id: 1,
            kind: SourceKind::Monitor,
            title: "Screen 1".into(),
            width: 1920,
            height: 1080,
            picked_on_host: false,
        }
    }

    #[tokio::test]
    async fn a_request_reaches_the_peers_link_with_its_own_token() {
        let hub = ScreenHub::default();
        hub.publish("lm", "Lightmoon", &[screen()]);
        let _answer = hub.show("lm", 1);
        let _second = hub.show("lm", 1);
        let tokens: Vec<u64> = hub
            .take_requests("lm")
            .into_iter()
            .map(|r| match r {
                HubRequest::Show { token, .. } => token,
                HubRequest::Stop { token } => token,
            })
            .collect();
        assert_eq!(tokens.len(), 2);
        assert_ne!(tokens[0], tokens[1]);
        assert!(
            hub.take_requests("lm").is_empty(),
            "requests are taken once"
        );
    }

    #[tokio::test]
    async fn asking_a_peer_that_left_fails_at_once() {
        let hub = ScreenHub::default();
        let answer = hub.show("gone", 1).await.unwrap();
        assert!(answer.is_err());
    }

    #[tokio::test]
    async fn a_peer_leaving_answers_what_was_queued_for_it() {
        let hub = ScreenHub::default();
        hub.publish("lm", "Lightmoon", &[screen()]);
        let answer = hub.show("lm", 1);
        hub.withdraw("lm");
        assert!(answer.await.unwrap().is_err());
        assert!(hub.peers().is_empty());
    }

    #[test]
    fn a_peer_with_nothing_to_offer_is_not_listed() {
        let hub = ScreenHub::default();
        hub.publish("lm", "Lightmoon", &[]);
        assert!(hub.peers().is_empty());
    }
}
