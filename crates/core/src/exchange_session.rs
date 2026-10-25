//! A request-and-answer module on one link.
//!
//! Messages and the gallery run the same engine. Each side may hold either
//! half, or both: the *answering* half takes the peer's requests, bounds them
//! (a request off the wire is untrusted input, like a file path), answers
//! them from its store on the blocking pool — a provider query or a photo
//! decode must not hold the link's other answers behind it — and says
//! "changed" once per change of that store. The *asking* half carries this
//! machine's requests from the module's [`RequestHub`](crate::request_hub::RequestHub) to the peer, numbered,
//! and hands each answer back to whoever is waiting. Nothing but "changed"
//! is ever sent unasked.
//!
//! What differs between the modules is what is asked, how a request is
//! bounded, which class each message rides in and how many requests the store
//! may work on at once; [`Exchange`](crate::exchange_session::Exchange) names those.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use cursio_protocol::exchange::ExchangeMsg;
use cursio_protocol::{Message, ModuleId, PeerId, Priority};
use cursio_transport::{TransportError, TransportTx};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::module::SessionModule;
use crate::request_hub::{Outgoing, RequestHub};

/// One request-and-answer module, as the shared engine needs to know it.
pub trait Exchange: Send + 'static {
    type Request: Serialize + DeserializeOwned + Send + Sync + 'static;
    type Response: Serialize + DeserializeOwned + Send + Sync + 'static;
    /// What answers this machine's half: a phone's message provider, a
    /// gallery.
    type Store: ?Sized + Send + Sync + 'static;

    const MODULE: ModuleId;
    /// The module's name in logs: "messages", "gallery".
    const NAME: &'static str;
    /// Logged when the peer asks a machine that has no store.
    const NOT_HERE: &'static str;
    /// Most requests the store works on at once; `None` answers each as it
    /// comes.
    const MAX_IN_FLIGHT: Option<usize>;

    fn hub() -> &'static RequestHub<Self::Request, Self::Response>;
    /// Answer one request; runs on the blocking pool.
    fn answer(store: &Self::Store, request: &Self::Request) -> Result<Self::Response, String>;
    /// Bound a request from the peer before the store sees it.
    fn check(request: Self::Request) -> Result<Self::Request, String>;
    /// The class `msg` rides in.
    fn priority(msg: &ExchangeMsg<Self::Request, Self::Response>) -> Priority;
    /// The link came up with this machine asking.
    fn asking_started(_peer_name: &str) {}
}

type Answer<E> = Result<<E as Exchange>::Response, String>;

/// The half that answers the peer from this machine's store.
struct Answering<E: Exchange> {
    store: Arc<E::Store>,
    /// The store's change count last told to the peer.
    told: u64,
    incoming: VecDeque<(u32, E::Request)>,
    in_flight: usize,
    done_tx: mpsc::UnboundedSender<(u32, Answer<E>)>,
    done_rx: mpsc::UnboundedReceiver<(u32, Answer<E>)>,
}

/// The half that asks the peer for this machine.
struct Asking<E: Exchange> {
    outbox: mpsc::UnboundedReceiver<Outgoing<E::Request, E::Response>>,
    token: u64,
    next_id: u32,
    waiting: HashMap<u32, oneshot::Sender<Answer<E>>>,
}

pub struct ExchangeEngine<E: Exchange> {
    peer: PeerId,
    peer_name: String,
    answering: Option<Answering<E>>,
    asking: Option<Asking<E>>,
}

impl<E: Exchange> ExchangeEngine<E> {
    pub fn new(peer: PeerId, peer_name: String, store: Option<Arc<E::Store>>, asks: bool) -> Self {
        let answering = store.map(|store| {
            let (done_tx, done_rx) = mpsc::unbounded_channel();
            Answering {
                store,
                told: E::hub().generation(),
                incoming: VecDeque::new(),
                in_flight: 0,
                done_tx,
                done_rx,
            }
        });
        let asking = asks.then(|| {
            let (outbox, token) = E::hub().register(peer, peer_name.clone());
            Asking {
                outbox,
                token,
                next_id: 1,
                waiting: HashMap::new(),
            }
        });
        ExchangeEngine {
            peer,
            peer_name,
            answering,
            asking,
        }
    }

    pub(crate) fn on_message(&mut self, msg: ExchangeMsg<E::Request, E::Response>) {
        match msg {
            ExchangeMsg::Request { id, request } => match &mut self.answering {
                Some(answering) => answering.incoming.push_back((id, request)),
                None => tracing::debug!(peer = %self.peer_name, "{}", E::NOT_HERE),
            },
            ExchangeMsg::Response { id, response } => {
                let waiting = self.asking.as_mut().and_then(|a| a.waiting.remove(&id));
                match waiting {
                    // The asker may have given up waiting; that is not an error.
                    Some(reply) => drop(reply.send(response)),
                    None => {
                        tracing::debug!(peer = %self.peer_name, id, "an answer nobody is waiting for")
                    }
                }
            }
            ExchangeMsg::Changed => {
                if self.asking.is_some() {
                    E::hub().bump(&self.peer);
                }
            }
        }
    }
}

async fn send<E: Exchange>(
    tx: &mut dyn TransportTx,
    msg: &ExchangeMsg<E::Request, E::Response>,
) -> Result<(), TransportError> {
    let message = Message::module(E::MODULE, E::priority(msg), msg)?;
    tx.send(&message).await
}

#[async_trait]
impl<E: Exchange> SessionModule for ExchangeEngine<E> {
    fn name(&self) -> &'static str {
        E::NAME
    }

    fn id(&self) -> ModuleId {
        E::MODULE
    }

    async fn start(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if self.asking.is_some() {
            E::asking_started(&self.peer_name);
        }
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if let Some(answering) = &mut self.answering {
            while E::MAX_IN_FLIGHT.is_none_or(|most| answering.in_flight < most) {
                let Some((id, request)) = answering.incoming.pop_front() else {
                    break;
                };
                let request = match E::check(request) {
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
                    let _ = done.send((id, E::answer(&store, &request)));
                });
            }
            while let Ok((id, response)) = answering.done_rx.try_recv() {
                answering.in_flight = answering.in_flight.saturating_sub(1);
                if let Err(reason) = &response {
                    tracing::debug!(id, %reason, "a {} request was refused", E::NAME);
                }
                send::<E>(tx, &ExchangeMsg::Response { id, response }).await?;
            }
            let generation = E::hub().generation();
            if generation != answering.told {
                answering.told = generation;
                send::<E>(tx, &ExchangeMsg::Changed).await?;
            }
        }
        if let Some(asking) = &mut self.asking {
            asking.waiting.retain(|_, reply| !reply.is_closed());
            while let Ok(Outgoing { request, reply }) = asking.outbox.try_recv() {
                let id = asking.next_id;
                asking.next_id = asking.next_id.wrapping_add(1).max(1);
                asking.waiting.insert(id, reply);
                send::<E>(tx, &ExchangeMsg::Request { id, request }).await?;
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
        match msg.decode_module::<ExchangeMsg<E::Request, E::Response>>(E::MODULE) {
            Ok(Some(msg)) => {
                self.on_message(msg);
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable {} message from peer", E::NAME);
                true
            }
        }
    }
}

impl<E: Exchange> Drop for ExchangeEngine<E> {
    fn drop(&mut self) {
        if let Some(asking) = &self.asking {
            E::hub().unregister(&self.peer, asking.token);
        }
    }
}
