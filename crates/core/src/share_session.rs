//! The share module on one link: sends what is handed to this machine's
//! [`outbox`](crate::share_sink::outbox) for the peer, and acts on what the
//! peer sends through this machine's [`ShareSink`].
//!
//! Files are pulled, not pushed (see `cursio_protocol::share`), and are read
//! and written straight from and to disk a chunk at a time: a shared video
//! never sits in memory whole, and a pump sends a bounded number of chunks so
//! this module's own replies are never far behind.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use cursio_protocol::share::{ShareAction, ShareContent, ShareId, ShareMsg, ShareOutcome};
use cursio_protocol::{Message, ModuleId, PeerId};
use cursio_transport::{TransportError, TransportTx};
use tokio::sync::{mpsc, oneshot};

use crate::module::SessionModule;
use crate::share_sink::{outbox, Outgoing, ShareSink};

/// Version of [`ShareMsg`]; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

/// Bytes per [`ShareMsg::Data`]: big enough that a file is not mostly framing,
/// well under the frame limit, and split further by the transport anyway.
const CHUNK: usize = 256 * 1024;
/// Chunks sent per pump, so a long transfer leaves room for this module's
/// own offers and answers.
const CHUNKS_PER_PUMP: usize = 4;

struct Sending {
    file: std::fs::File,
    seq: u32,
}

struct Out {
    path: Option<PathBuf>,
    reply: Option<oneshot::Sender<ShareOutcome>>,
    sending: Option<Sending>,
}

struct In {
    content: ShareContent,
    action: ShareAction,
    path: PathBuf,
    file: std::fs::File,
    next_seq: u32,
    received: u64,
}

pub struct ShareEngine {
    peer: PeerId,
    peer_name: String,
    sink: Arc<dyn ShareSink>,
    shares: mpsc::UnboundedReceiver<Outgoing>,
    token: u64,
    outgoing: HashMap<ShareId, Out>,
    incoming: HashMap<ShareId, In>,
    queued: Vec<ShareMsg>,
}

impl ShareEngine {
    pub fn new(peer: PeerId, peer_name: String, sink: Arc<dyn ShareSink>) -> Self {
        let (shares, token) = outbox().register(peer, peer_name.clone());
        ShareEngine {
            peer,
            peer_name,
            sink,
            shares,
            token,
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            queued: Vec::new(),
        }
    }

    fn offer(&mut self, share: Outgoing) {
        let id: ShareId = rand::random();
        tracing::info!(peer = %self.peer_name, what = %share.content.summary(), action = ?share.action, "sharing");
        self.queued.push(ShareMsg::Offer {
            id,
            content: share.content,
            action: share.action,
        });
        self.outgoing.insert(
            id,
            Out {
                path: share.path,
                reply: share.reply,
                sending: None,
            },
        );
    }

    fn on_offer(&mut self, id: ShareId, content: ShareContent, action: ShareAction) {
        if !matches!(content, ShareContent::File { .. }) {
            let outcome = self.sink.act(&self.peer_name, &content, None, action);
            self.queued.push(ShareMsg::Done { id, outcome });
            return;
        }
        let Some(dir) = self.sink.incoming_dir() else {
            self.queued.push(ShareMsg::Done {
                id,
                outcome: ShareOutcome::Refused("this machine has nowhere to keep files".into()),
            });
            return;
        };
        let path = dir.join(format!(".cursio-share-{id:016x}.part"));
        match std::fs::create_dir_all(&dir).and_then(|()| std::fs::File::create(&path)) {
            Ok(file) => {
                self.incoming.insert(
                    id,
                    In {
                        content,
                        action,
                        path,
                        file,
                        next_seq: 0,
                        received: 0,
                    },
                );
                self.queued.push(ShareMsg::Request { id });
            }
            Err(e) => self.queued.push(ShareMsg::Done {
                id,
                outcome: ShareOutcome::Failed(format!("could not start the file: {e}")),
            }),
        }
    }

    fn on_request(&mut self, id: ShareId) {
        let Some(out) = self.outgoing.get_mut(&id) else {
            return;
        };
        let opened = out
            .path
            .as_ref()
            .ok_or_else(|| "nothing to send".to_string())
            .and_then(|p| std::fs::File::open(p).map_err(|e| e.to_string()));
        match opened {
            Ok(file) => out.sending = Some(Sending { file, seq: 0 }),
            Err(e) => {
                self.queued.push(ShareMsg::Done {
                    id,
                    outcome: ShareOutcome::Failed(format!("the file could not be read: {e}")),
                });
                self.finish_out(id, ShareOutcome::Failed(e));
            }
        }
    }

    fn on_data(&mut self, id: ShareId, seq: u32, last: bool, data: &[u8]) {
        let Some(incoming) = self.incoming.get_mut(&id) else {
            return;
        };
        let written = if seq != incoming.next_seq {
            Err(format!(
                "chunk {seq} arrived where {} was due",
                incoming.next_seq
            ))
        } else {
            incoming.file.write_all(data).map_err(|e| e.to_string())
        };
        incoming.next_seq += 1;
        incoming.received += data.len() as u64;
        if let Err(e) = written {
            self.abandon_in(id);
            self.queued.push(ShareMsg::Done {
                id,
                outcome: ShareOutcome::Failed(format!("the file could not be written: {e}")),
            });
            return;
        }
        if !last {
            return;
        }
        let incoming = self.incoming.remove(&id).expect("looked up above");
        drop(incoming.file);
        let outcome = match &incoming.content {
            ShareContent::File { size, .. } if *size != incoming.received => {
                ShareOutcome::Failed(format!("{} of {size} bytes arrived", incoming.received))
            }
            content => self.sink.act(
                &self.peer_name,
                content,
                Some(&incoming.path),
                incoming.action,
            ),
        };
        // Whatever the sink did not move away is a leftover.
        let _ = std::fs::remove_file(&incoming.path);
        self.queued.push(ShareMsg::Done { id, outcome });
    }

    fn on_done(&mut self, id: ShareId, outcome: ShareOutcome) {
        if self.outgoing.contains_key(&id) {
            tracing::info!(peer = %self.peer_name, ?outcome, "share answered");
            self.finish_out(id, outcome);
        } else if self.incoming.contains_key(&id) {
            tracing::info!(peer = %self.peer_name, ?outcome, "sender gave up on a share");
            self.abandon_in(id);
        }
    }

    fn finish_out(&mut self, id: ShareId, outcome: ShareOutcome) {
        if let Some(out) = self.outgoing.remove(&id) {
            if let Some(reply) = out.reply {
                let _ = reply.send(outcome);
            }
        }
    }

    fn abandon_in(&mut self, id: ShareId) {
        if let Some(incoming) = self.incoming.remove(&id) {
            drop(incoming.file);
            let _ = std::fs::remove_file(&incoming.path);
        }
    }

    /// Send up to [`CHUNKS_PER_PUMP`] chunks of whatever files are requested.
    async fn stream(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let mut budget = CHUNKS_PER_PUMP;
        let ids: Vec<ShareId> = self
            .outgoing
            .iter()
            .filter(|(_, o)| o.sending.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            while budget > 0 {
                let Some(sending) = self.outgoing.get_mut(&id).and_then(|o| o.sending.as_mut())
                else {
                    break;
                };
                let mut buf = vec![0u8; CHUNK];
                let read = match read_full(&mut sending.file, &mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        send(
                            tx,
                            &ShareMsg::Done {
                                id,
                                outcome: ShareOutcome::Failed(format!(
                                    "reading the file failed: {e}"
                                )),
                            },
                        )
                        .await?;
                        self.finish_out(id, ShareOutcome::Failed(e.to_string()));
                        break;
                    }
                };
                buf.truncate(read);
                let last = read < CHUNK;
                let seq = sending.seq;
                sending.seq += 1;
                if last {
                    if let Some(out) = self.outgoing.get_mut(&id) {
                        out.sending = None;
                    }
                }
                send(
                    tx,
                    &ShareMsg::Data {
                        id,
                        seq,
                        last,
                        data: buf,
                    },
                )
                .await?;
                budget -= 1;
                if last {
                    break;
                }
            }
        }
        Ok(())
    }
}

/// Fill `buf` unless the file ends first; a short count means the end.
fn read_full(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

async fn send(tx: &mut dyn TransportTx, msg: &ShareMsg) -> Result<(), TransportError> {
    let message = Message::module(ModuleId::SHARE, ShareMsg::PRIORITY, msg)?;
    tx.send(&message).await
}

#[async_trait]
impl SessionModule for ShareEngine {
    fn name(&self) -> &'static str {
        "share"
    }

    fn id(&self) -> ModuleId {
        ModuleId::SHARE
    }

    async fn start(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        tracing::info!(peer = %self.peer_name, "sharing up — links, text and files can be sent either way");
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        while let Ok(share) = self.shares.try_recv() {
            self.offer(share);
        }
        for msg in std::mem::take(&mut self.queued) {
            send(tx, &msg).await?;
        }
        self.stream(tx).await
    }

    fn has_pending_work(&self) -> bool {
        !self.queued.is_empty() || self.outgoing.values().any(|o| o.sending.is_some())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<ShareMsg>(ModuleId::SHARE) {
            Ok(Some(msg)) => {
                match msg {
                    ShareMsg::Offer {
                        id,
                        content,
                        action,
                    } => self.on_offer(id, content, action),
                    ShareMsg::Request { id } => self.on_request(id),
                    ShareMsg::Data {
                        id,
                        seq,
                        last,
                        data,
                    } => self.on_data(id, seq, last, &data),
                    ShareMsg::Done { id, outcome } => self.on_done(id, outcome),
                }
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable share message from peer");
                true
            }
        }
    }
}

impl Drop for ShareEngine {
    fn drop(&mut self) {
        outbox().unregister(&self.peer, self.token);
        for id in self.incoming.keys().copied().collect::<Vec<_>>() {
            self.abandon_in(id);
        }
        for id in self.outgoing.keys().copied().collect::<Vec<_>>() {
            self.finish_out(id, ShareOutcome::Failed("the link closed first".into()));
        }
    }
}

/// Builds [`ShareEngine`] with this machine's way of acting on shares.
pub struct ShareFactory {
    pub sink: Arc<dyn ShareSink>,
}

impl crate::module::ModuleFactory for ShareFactory {
    fn id(&self) -> ModuleId {
        ModuleId::SHARE
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        Some(cursio_protocol::ModuleAdvert::symmetric(
            ModuleId::SHARE,
            VERSION,
        ))
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(ShareEngine::new(
            ctx.peer.peer_id,
            ctx.peer_name().to_string(),
            Arc::clone(&self.sink),
        )))
    }
}
