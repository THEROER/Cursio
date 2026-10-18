//! The calls module on one link.
//!
//! A phone forwards what its call monitor reports through
//! [`hub`](crate::calls_hub::hub) and carries out what the computer asks
//! ([`CallActor`]); a computer files what the phone reports in the same hub,
//! which decides how to show it and which route a request takes. A module and
//! not node state, like notifications: who is calling is the user's to keep on
//! the phone by switching it off.

use std::sync::Arc;

use async_trait::async_trait;
use cursio_protocol::calls::CallsMsg;
use cursio_protocol::{Message, ModuleId, PeerId};
use cursio_transport::{TransportError, TransportTx};
use tokio::sync::mpsc;

use crate::calls_hub::{hub, CallActor};
use crate::module::SessionModule;

/// Version of [`CallsMsg`]; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

pub struct CallsEngine {
    peer: PeerId,
    actor: Option<Arc<dyn CallActor>>,
    /// Phone: what the monitor reports. Computer: requests for the phone.
    outbox: mpsc::UnboundedReceiver<CallsMsg>,
    token: u64,
    computer: bool,
    queued: Vec<CallsMsg>,
}

impl CallsEngine {
    pub fn new(
        peer: PeerId,
        peer_name: String,
        actor: Option<Arc<dyn CallActor>>,
        computer: bool,
    ) -> Self {
        if let Some(actor) = actor {
            let (now, outbox, token) = hub().register_phone_link();
            CallsEngine {
                peer,
                actor: Some(actor),
                outbox,
                token,
                computer,
                queued: now,
            }
        } else {
            let (tx, outbox) = mpsc::unbounded_channel();
            let token = if computer {
                hub().register_mesh_phone(peer, peer_name, tx)
            } else {
                0
            };
            CallsEngine {
                peer,
                actor: None,
                outbox,
                token,
                computer,
                queued: Vec::new(),
            }
        }
    }

    fn on_message(&mut self, msg: CallsMsg) {
        match (&self.actor, msg) {
            (
                _,
                msg @ (CallsMsg::Status { .. } | CallsMsg::Calls { .. } | CallsMsg::Refused { .. }),
            ) => {
                if self.computer {
                    hub().from_mesh_phone(&self.peer, msg);
                }
            }
            (Some(actor), request) => {
                if let Err(reason) = actor.act(&request) {
                    tracing::warn!(%reason, ?request, "could not carry out a call request");
                    self.queued.push(CallsMsg::Refused { reason });
                }
            }
            (None, request) => {
                tracing::debug!(?request, "a call request reached a machine with no phone");
                self.queued.push(CallsMsg::Refused {
                    reason: "this machine is not a phone".into(),
                });
            }
        }
    }
}

async fn send(tx: &mut dyn TransportTx, msg: &CallsMsg) -> Result<(), TransportError> {
    let message = Message::module(ModuleId::CALLS, CallsMsg::PRIORITY, msg)?;
    tx.send(&message).await
}

#[async_trait]
impl SessionModule for CallsEngine {
    fn name(&self) -> &'static str {
        "calls"
    }

    fn id(&self) -> ModuleId {
        ModuleId::CALLS
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.pump(tx).await
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        while let Ok(msg) = self.outbox.try_recv() {
            self.queued.push(msg);
        }
        for msg in std::mem::take(&mut self.queued) {
            send(tx, &msg).await?;
        }
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<CallsMsg>(ModuleId::CALLS) {
            Ok(Some(msg)) => {
                self.on_message(msg);
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable calls message from peer");
                true
            }
        }
    }
}

impl Drop for CallsEngine {
    fn drop(&mut self) {
        if self.actor.is_some() {
            hub().unregister_phone_link(self.token);
        } else if self.computer {
            hub().unregister_mesh_phone(&self.peer, self.token);
        }
    }
}

/// Builds [`CallsEngine`] for whichever half this machine plays: a phone has
/// an actor, a computer shows calls.
pub struct CallsFactory {
    pub actor: Option<Arc<dyn CallActor>>,
    pub computer: bool,
}

impl crate::module::ModuleFactory for CallsFactory {
    fn id(&self) -> ModuleId {
        ModuleId::CALLS
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        self.supported()
            .then(|| cursio_protocol::ModuleAdvert::symmetric(ModuleId::CALLS, VERSION))
    }

    fn supported(&self) -> bool {
        self.actor.is_some() || self.computer
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(CallsEngine::new(
            ctx.peer.peer_id,
            ctx.peer_name().to_string(),
            self.actor.clone(),
            self.computer,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Refusing(Mutex<Vec<CallsMsg>>);

    impl CallActor for Refusing {
        fn act(&self, msg: &CallsMsg) -> Result<(), String> {
            self.0.lock().unwrap().push(msg.clone());
            Err("answering calls is not allowed on this phone".into())
        }
    }

    #[test]
    fn a_request_the_phone_cannot_carry_out_is_answered_with_why() {
        let actor = Arc::new(Refusing(Mutex::new(Vec::new())));
        let mut engine = CallsEngine::new(
            PeerId::from_u128(9),
            "Nocturne".into(),
            Some(actor.clone()),
            false,
        );
        engine.queued.clear();
        engine.on_message(CallsMsg::Answer { id: 1 });
        assert_eq!(
            actor.0.lock().unwrap().as_slice(),
            &[CallsMsg::Answer { id: 1 }]
        );
        assert_eq!(
            engine.queued,
            vec![CallsMsg::Refused {
                reason: "answering calls is not allowed on this phone".into()
            }]
        );
    }
}
