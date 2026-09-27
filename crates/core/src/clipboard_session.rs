//! Clipboard sharing as a [`SessionModule`], not a session of its own.
//!
//! Copy on one machine, paste on the other — text, images, and every other
//! representation the platform can carry. Like audio and window transfer, this
//! runs multiplexed onto the one encrypted channel that also carries input,
//! rather than owning a loop of its own.
//!
//! ## The exchange (see `docs/clipboard-design.md`)
//!
//! - **Offer, then request.** A local copy is *announced* — formats and sizes,
//!   a few bytes — and the peer pulls a representation only when it needs one.
//!   Most copies are never pasted on the peer; pushing every one's bytes eagerly
//!   would waste the link that input rides on.
//! - **Chunked transfer.** A requested format streams as
//!   [`ClipboardData`](cursio_protocol::ClipboardData) chunks, so a multi-MB
//!   image neither exceeds the frame limit nor freezes the session while it
//!   moves — input interleaves between chunks.
//! - **Generation ids.** Copying twice while the peer is still pulling the first
//!   copy is normal; every message carries the generation and the receiver drops
//!   anything but the newest it was offered.
//! - **Explicit failures.** An unservable request is answered, not dropped, so
//!   the requester falls back instead of hanging.
//!
//! ## Why a worker thread
//!
//! `clipboard-core` is synchronous and its Linux backend blocks (an `arboard`
//! connection that must live on one thread; `write` detaches its own serving
//! thread). A [`SessionModule`] is `Send` and must not block the session loop.
//! So the blocking clipboard lives on a dedicated worker thread, and this module
//! holds only channels to it — exactly how [`AudioEngine`](crate::audio_session)
//! keeps its captures behind channels.
//!
//! ## How it composes with `clipd`
//!
//! The receiving side *writes what it pulls onto the local system clipboard*.
//! That is the whole integration: a running `clipd` sees it as an ordinary copy
//! and records it, with no code shared between the two. Copying locally and
//! receiving from the peer are indistinguishable to everything downstream.

use std::collections::HashMap;

use async_trait::async_trait;
use clipboard_core::{ClipboardContent, ClipboardEntry, ClipboardError, ClipboardFormat};
use cursio_protocol::{
    ClipboardData, ClipboardFailReason, ClipboardFailed, ClipboardFormatOffer, ClipboardGeneration,
    ClipboardMsg, ClipboardOffer, ClipboardRequest, Message, ModuleId,
};
use cursio_transport::{TransportError, TransportTx};

use crate::module::SessionModule;

/// Put one clipboard message on the wire, enveloped for this module.
///
/// Every send in here goes through this rather than constructing the envelope
/// inline: the module id and priority are properties of the *module*, not of the
/// individual message, and stating them once is what keeps a stray `Priority`
/// from creeping into a single call site (rule 2 in `docs/platform-design.md`).
async fn send(tx: &mut dyn TransportTx, msg: ClipboardMsg) -> Result<(), TransportError> {
    let framed = Message::module(ModuleId::CLIPBOARD, ClipboardMsg::PRIORITY, &msg)?;
    tx.send(&framed).await
}

/// Which formats this side is willing to *paste* — i.e. will pull from a peer's
/// offer and write to the local clipboard.
///
/// This is just the accept-set; richness ordering lives in
/// [`ClipboardOffer::best_for`]. It is deliberately limited to what the local
/// backend can actually *write back*: writing a representation we can't put on
/// the clipboard would pull the bytes only to drop them. The Linux (`arboard`)
/// backend writes an image (PNG/JPEG, decoded to pixels), HTML, or plain text —
/// so those are pullable, and RTF and app-specific `Other` types are not, since
/// there is no write path for them yet. When a backend gains one, adding the
/// format here is the whole change.
const PULLABLE: &[ClipboardFormat] = &[
    ClipboardFormat::ImagePng,
    ClipboardFormat::ImageJpeg,
    ClipboardFormat::Html,
    ClipboardFormat::Text,
];

/// Bytes per [`ClipboardData`] chunk.
///
/// Well under [`MAX_FRAME_LEN`](cursio_protocol::MAX_FRAME_LEN) (16 MiB) once
/// Noise framing and bincode overhead are counted, and small enough that input
/// events interleaving between chunks stay responsive during a large paste. 256
/// KiB is the same order the window and audio paths move per frame.
const CHUNK_BYTES: usize = 256 * 1024;

/// Hard ceiling on a single representation we will serve or accept.
///
/// A runaway copy (a giant RGBA bitmap, a pathological file) must not be able to
/// stream unboundedly over the shared link. 64 MiB comfortably covers a 4K PNG
/// screenshot (~2 MB) with room for large documents, while refusing the absurd.
/// Enforced on both send (answer [`TooLarge`](ClipboardFailReason::TooLarge))
/// and receive (never request past it).
const MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

mod worker;
use worker::{ClipboardWorker, ReadReply};

/// Clipboard-sharing state for one session.
///
/// Holds only channels and bookkeeping; the blocking clipboard lives on the
/// worker thread (see [`ClipboardWorker`]).
pub struct ClipboardEngine {
    /// The blocking clipboard, off the async loop.
    worker: ClipboardWorker,

    /// Generation of the newest *local* copy we have offered the peer. Bumped on
    /// every local change; stamped on offers and on the data we serve.
    local_gen: ClipboardGeneration,
    /// What the current local copy offers, so a peer request can be validated
    /// against the generation it names without re-reading the clipboard.
    local_offer: Option<ClipboardOffer>,

    /// The newest generation the peer has offered *us*. A request we send, and a
    /// chunk we accept, must match this; anything older is stale and dropped.
    peer_gen: Option<ClipboardGeneration>,
    /// In-flight inbound transfer: the format being pulled and the bytes so far.
    /// Keyed nothing else — only one inbound pull runs at a time, since we pull a
    /// single best format per offer.
    inbound: Option<InboundTransfer>,

    /// Outbound requests we've handed the worker but not yet finished streaming,
    /// so `pump` can drain their chunks onto the wire a bounded amount at a time.
    outbound: HashMap<(ClipboardGeneration, ClipboardFormat), OutboundTransfer>,

    /// A request we decided to send on the next `pump`. Only the newest matters —
    /// a later offer overwrites an unsent request, so we never pull a copy the
    /// peer has already superseded. Message handling has no `tx`, so the actual
    /// send is deferred to `pump`.
    pending_request: Option<ClipboardRequest>,
    /// Failures to report to the peer, queued for the same reason `pending_request`
    /// is: `handle_message` can't send. Small and bursty, so a `Vec` drained each
    /// pump is enough.
    pending_failures: Vec<ClipboardFailed>,

    /// The last copy pulled from the peer and written locally. Clipboard APIs
    /// report our own write as a local change; retaining its bytes lets the
    /// worker recognise and suppress that echo. It stays until a genuinely
    /// different local copy appears, which also covers platforms that emit
    /// more than one notification for a single write.
    last_remote_copy: Option<ClipboardContent>,

    /// The peer's display name, for logs.
    peer_name: String,

    offered: u64,
    pulled: u64,
    served: u64,
}

/// An inbound pull in progress: we requested one format and are collecting its
/// chunks until `last`.
struct InboundTransfer {
    generation: ClipboardGeneration,
    format: ClipboardFormat,
    /// Chunks received so far, concatenated in `seq` order as they arrive.
    buf: Vec<u8>,
    /// Next `seq` we expect; a gap means loss and aborts the transfer.
    next_seq: u32,
}

/// An outbound serve in progress: the worker read the bytes, and we chunk them
/// onto the wire across successive `pump` calls.
struct OutboundTransfer {
    generation: ClipboardGeneration,
    format: ClipboardFormat,
    bytes: Vec<u8>,
    /// Byte offset of the next chunk to send.
    offset: usize,
    /// `seq` of the next chunk.
    next_seq: u32,
}

impl ClipboardEngine {
    /// Create the clipboard module. Nothing touches the clipboard or the wire
    /// until [`SessionModule::start`] spins up the worker.
    pub fn new(peer_name: String) -> Self {
        Self {
            worker: ClipboardWorker::spawn(),
            local_gen: 0,
            local_offer: None,
            peer_gen: None,
            inbound: None,
            outbound: HashMap::new(),
            pending_request: None,
            pending_failures: Vec::new(),
            last_remote_copy: None,
            peer_name,
            offered: 0,
            pulled: 0,
            served: 0,
        }
    }
}

#[async_trait]
impl SessionModule for ClipboardEngine {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn id(&self) -> ModuleId {
        ModuleId::CLIPBOARD
    }

    /// Begin watching the local clipboard. No offer is sent yet — the first one
    /// goes out from `pump` when the watcher reports the current content (or the
    /// next copy). Starting the watch is all that's needed up front.
    async fn start(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.worker.start_watch();
        tracing::info!(
            peer = %self.peer_name,
            "clipboard sharing up — copies here offer to the peer, and vice versa"
        );
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        // Deferred sends from `handle_message` first: a pull request should go
        // out promptly, and a failure lets the peer stop waiting.
        if let Some(req) = self.pending_request.take() {
            send(tx, ClipboardMsg::Request(req)).await?;
        }
        for failed in self.pending_failures.drain(..) {
            send(tx, ClipboardMsg::Failed(failed)).await?;
        }
        self.pump_local_change(tx).await?;
        self.pump_read_replies(tx).await?;
        self.pump_outbound(tx).await?;
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        // Claim only envelopes addressed to this module. A decode failure for a
        // frame that *is* ours means the peer's vocabulary differs despite
        // negotiation agreeing on a version — worth a loud log, but the frame is
        // still ours, so it is claimed rather than offered to other modules.
        match msg.decode_module::<ClipboardMsg>(ModuleId::CLIPBOARD) {
            Ok(Some(msg)) => {
                match msg {
                    ClipboardMsg::Offer(offer) => self.on_offer(&offer),
                    ClipboardMsg::Request(req) => self.on_request(&req),
                    ClipboardMsg::Data(data) => self.on_data(&data),
                    ClipboardMsg::Failed(failed) => self.on_failed(&failed),
                }
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable clipboard message from peer");
                true
            }
        }
    }

    async fn shutdown(&mut self, _tx: &mut dyn TransportTx) {
        // Dropping the worker stops the watch thread and closes its channels.
        self.worker.stop();
    }
}

impl ClipboardEngine {
    /// If the local clipboard changed, read what it now offers and send a fresh
    /// [`ClipboardOffer`]. The offer is payload-free — sizes only — so this stays
    /// cheap even when a 4K image was just copied.
    async fn pump_local_change(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        // At most one change per pump: the watcher coalesces, and a burst of
        // copies only means the newest offer matters. Draining the channel here
        // would starve the session's inbound branch, same discipline as audio.
        if !self.worker.clipboard_changed() {
            return Ok(());
        }

        if let Some(remote) = &self.last_remote_copy {
            match self.worker.matches(remote) {
                Ok(true) => {
                    tracing::debug!("ignoring clipboard change caused by peer copy");
                    return Ok(());
                }
                Ok(false) => self.last_remote_copy = None,
                Err(error) => {
                    // Do not let an optional echo check disable ordinary local
                    // sharing. A normal list/read below will report any real
                    // backend failure in its existing path.
                    tracing::debug!(%error, "could not compare clipboard with peer copy");
                    self.last_remote_copy = None;
                }
            }
        }

        let formats = match self.worker.list_formats() {
            Ok(formats) => formats,
            Err(ClipboardError::Empty) => {
                // Cleared clipboard: nothing to offer. Leave the last offer
                // standing rather than announce emptiness — the peer keeps
                // whatever it already pulled, which matches local paste (an
                // empty clipboard doesn't wipe what you last copied elsewhere).
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(error = %e, "reading local clipboard formats failed");
                return Ok(());
            }
        };

        // A representation past the ceiling is not offered at all: offering it
        // only to answer TooLarge on request wastes a round-trip. Others in the
        // same copy still go.
        let formats: Vec<ClipboardFormatOffer> = formats
            .into_iter()
            .filter(|f| f.size <= MAX_PAYLOAD_BYTES)
            .collect();
        if formats.is_empty() {
            return Ok(());
        }

        self.local_gen += 1;
        let offer = ClipboardOffer {
            generation: self.local_gen,
            formats,
        };
        self.local_offer = Some(offer.clone());
        self.offered += 1;
        tracing::info!(
            generation = self.local_gen,
            formats = offer.formats.len(),
            "offering local copy to peer"
        );
        send(tx, ClipboardMsg::Offer(offer)).await
    }

    /// Turn any read the worker has finished into either the first
    /// [`ClipboardData`] chunk (queued for streaming) or a [`ClipboardFailed`].
    async fn pump_read_replies(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        while let Some(reply) = self.worker.next_read_reply() {
            match reply {
                ReadReply::Ok {
                    generation,
                    format,
                    bytes,
                } => {
                    // The copy may have moved on while the worker read: if this
                    // generation is no longer the current local one, the bytes
                    // are stale — tell the requester rather than serving them.
                    if generation != self.local_gen {
                        tracing::debug!(
                            generation,
                            "read finished for a superseded copy; dropping"
                        );
                        send(
                            tx,
                            ClipboardMsg::Failed(ClipboardFailed {
                                generation,
                                format,
                                reason: ClipboardFailReason::StaleGeneration,
                            }),
                        )
                        .await?;
                        continue;
                    }
                    if bytes.len() as u64 > MAX_PAYLOAD_BYTES {
                        send(
                            tx,
                            ClipboardMsg::Failed(ClipboardFailed {
                                generation,
                                format,
                                reason: ClipboardFailReason::TooLarge,
                            }),
                        )
                        .await?;
                        continue;
                    }
                    // Queue for chunked streaming from `pump_outbound`.
                    self.outbound.insert(
                        (generation, format.clone()),
                        OutboundTransfer {
                            generation,
                            format,
                            bytes,
                            offset: 0,
                            next_seq: 0,
                        },
                    );
                }
                ReadReply::Failed {
                    generation,
                    format,
                    error,
                } => {
                    let reason = match error {
                        ClipboardError::FormatUnavailable(_) => ClipboardFailReason::FormatGone,
                        ClipboardError::Empty | ClipboardError::OwnerGone => {
                            ClipboardFailReason::FormatGone
                        }
                        other => ClipboardFailReason::ReadFailed(other.to_string()),
                    };
                    tracing::debug!(generation, format = %format.mime(), ?reason, "serve failed");
                    send(
                        tx,
                        ClipboardMsg::Failed(ClipboardFailed {
                            generation,
                            format,
                            reason,
                        }),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// Push a bounded number of chunks for in-flight outbound transfers. Bounded
    /// so a large image doesn't monopolise the loop while it streams: input and
    /// everything else keeps flowing between chunks.
    async fn pump_outbound(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        // Nothing to serve is the overwhelmingly common case (a serve only runs
        // right after the peer requests a paste), so return before allocating.
        // This runs every loop iteration; the fast path must cost nothing.
        if self.outbound.is_empty() {
            return Ok(());
        }

        // A few chunks per pump: with CHUNK_BYTES at 256 KiB, four is 1 MiB per
        // loop iteration — enough to move an image promptly without blocking the
        // session for long.
        let mut budget = 4usize;

        // Collect keys first to avoid holding a borrow across the async send.
        let keys: Vec<(ClipboardGeneration, ClipboardFormat)> =
            self.outbound.keys().cloned().collect();
        for key in keys {
            if budget == 0 {
                break;
            }
            // If a newer local copy appeared, this outbound serve is for a copy
            // the peer will now ignore; drop it rather than finishing the stream.
            if key.0 != self.local_gen {
                self.outbound.remove(&key);
                continue;
            }
            while budget > 0 {
                let done;
                let msg;
                {
                    let t = match self.outbound.get_mut(&key) {
                        Some(t) => t,
                        None => break,
                    };
                    let end = (t.offset + CHUNK_BYTES).min(t.bytes.len());
                    let chunk = t.bytes[t.offset..end].to_vec();
                    let last = end == t.bytes.len();
                    msg = ClipboardMsg::Data(ClipboardData {
                        generation: t.generation,
                        format: t.format.clone(),
                        seq: t.next_seq,
                        last,
                        data: chunk,
                    });
                    t.offset = end;
                    t.next_seq += 1;
                    done = last;
                }
                send(tx, msg).await?;
                budget -= 1;
                if done {
                    self.served += 1;
                    self.outbound.remove(&key);
                    tracing::debug!(?key, "finished serving a representation");
                    break;
                }
            }
        }
        Ok(())
    }
}

impl ClipboardEngine {
    /// The peer announced a new copy. Record it as the newest peer generation
    /// and, if it offers something we can paste, immediately request the best
    /// representation so a later local paste is instant.
    ///
    /// Eager pull (not on-demand-at-paste) is deliberate: this side has no hook
    /// into "the user is about to paste", and the received content has to already
    /// be on the local clipboard for a paste to find it — that is also what lets
    /// a running `clipd` record it. The offer→request split still saved the bytes
    /// of every copy the *peer* made and never shared further; here we pull one
    /// best format, not all representations.
    fn on_offer(&mut self, offer: &ClipboardOffer) {
        // Ignore an offer older than one we've already seen (reordering).
        if let Some(seen) = self.peer_gen {
            if offer.generation <= seen {
                tracing::trace!(
                    generation = offer.generation,
                    seen,
                    "ignoring stale/rerun offer"
                );
                return;
            }
        }
        self.peer_gen = Some(offer.generation);
        // A new offer supersedes any inbound pull still in progress.
        self.inbound = None;

        let want = match offer.best_for(PULLABLE) {
            Some(f) => f.clone(),
            None => {
                tracing::debug!(
                    generation = offer.generation,
                    "peer copy offers nothing we can paste; not pulling"
                );
                return;
            }
        };
        // Respect the ceiling: don't request a representation we'd refuse.
        if let Some(fo) = offer.formats.iter().find(|f| f.format == want) {
            if fo.size > MAX_PAYLOAD_BYTES {
                tracing::info!(
                    generation = offer.generation,
                    size = fo.size,
                    "peer copy exceeds the transfer ceiling; not pulling"
                );
                return;
            }
        }

        tracing::info!(
            generation = offer.generation,
            format = %want.mime(),
            "pulling peer copy"
        );
        self.inbound = Some(InboundTransfer {
            generation: offer.generation,
            format: want.clone(),
            buf: Vec::new(),
            next_seq: 0,
        });
        // The request goes out from `pump` via the outbound path? No — requests
        // are tiny and one-shot, so send-on-handle would need a tx. Instead we
        // stash it and let `pump` flush it. Keep it simple: queue via worker-free
        // channel is overkill; record intent and send in pump.
        self.pending_request = Some(ClipboardRequest {
            generation: offer.generation,
            format: want,
        });
    }

    /// The peer wants the bytes of one representation of one of *our* copies.
    /// Validate the generation, then hand the read to the worker; the reply is
    /// turned into chunks (or a failure) by `pump_read_replies`.
    fn on_request(&mut self, req: &ClipboardRequest) {
        // Only the current local copy can be served: an older generation's bytes
        // are gone (the clipboard holds one copy at a time).
        if req.generation != self.local_gen {
            tracing::debug!(
                requested = req.generation,
                current = self.local_gen,
                "peer requested a superseded copy"
            );
            self.pending_failures.push(ClipboardFailed {
                generation: req.generation,
                format: req.format.clone(),
                reason: ClipboardFailReason::StaleGeneration,
            });
            return;
        }
        // The format must be one we actually offered.
        let offered = self
            .local_offer
            .as_ref()
            .is_some_and(|o| o.has(&req.format));
        if !offered {
            self.pending_failures.push(ClipboardFailed {
                generation: req.generation,
                format: req.format.clone(),
                reason: ClipboardFailReason::FormatGone,
            });
            return;
        }
        tracing::info!(
            generation = req.generation,
            format = %req.format.mime(),
            "serving peer request"
        );
        self.worker.request_read(req.generation, req.format.clone());
    }

    /// A chunk of the representation we're pulling. Reassemble in `seq` order and,
    /// on the last chunk, write the whole thing to the local clipboard.
    fn on_data(&mut self, data: &ClipboardData) {
        let transfer = match &mut self.inbound {
            Some(t) => t,
            None => {
                tracing::trace!("data with no inbound transfer; dropping");
                return;
            }
        };
        // Drop anything not for the copy/format we're currently pulling — a
        // stale generation, or a different format's stream.
        if data.generation != transfer.generation || data.format != transfer.format {
            tracing::trace!(
                got = data.generation,
                want = transfer.generation,
                "data for a different copy/format; dropping"
            );
            return;
        }
        // A gap means loss; the transfer can't be reassembled correctly, so
        // abandon it rather than write a corrupt payload.
        if data.seq != transfer.next_seq {
            tracing::warn!(
                got = data.seq,
                want = transfer.next_seq,
                "gap in clipboard data; abandoning pull"
            );
            self.inbound = None;
            return;
        }
        // Guard against an oversized stream even if the offer under-reported.
        if transfer.buf.len() as u64 + data.data.len() as u64 > MAX_PAYLOAD_BYTES {
            tracing::warn!("inbound clipboard payload exceeded ceiling mid-stream; abandoning");
            self.inbound = None;
            return;
        }
        transfer.buf.extend_from_slice(&data.data);
        transfer.next_seq += 1;

        if data.last {
            let InboundTransfer {
                format,
                buf,
                generation,
                ..
            } = self.inbound.take().unwrap();
            tracing::info!(
                generation,
                format = %format.mime(),
                bytes = buf.len(),
                "received peer copy; writing to local clipboard"
            );
            let content = ClipboardContent {
                entries: vec![ClipboardEntry::loaded(format, buf)],
            };
            self.last_remote_copy = Some(content.clone());
            self.worker.write(content);
            self.pulled += 1;
        }
    }

    /// The peer couldn't serve what we requested. Give up on this pull; the next
    /// offer will bring a fresh chance. (A richer fallback — try the next-best
    /// format on `FormatGone` — is possible but not worth the complexity yet: the
    /// common failure is `StaleGeneration`, already superseded by a newer offer.)
    fn on_failed(&mut self, failed: &ClipboardFailed) {
        if let Some(t) = &self.inbound {
            if t.generation == failed.generation && t.format == failed.format {
                tracing::info!(
                    generation = failed.generation,
                    format = %failed.format.mime(),
                    reason = ?failed.reason,
                    "peer could not serve the pull; dropping"
                );
                self.inbound = None;
            }
        }
    }
}

/// Builds [`ClipboardEngine`] when clipboard sharing is enabled.
///
/// Available on every platform `clipboard-core` supports, so availability is
/// purely "did the user ask for it".
pub struct ClipboardFactory {
    pub enabled: bool,
}

impl crate::module::ModuleFactory for ClipboardFactory {
    fn id(&self) -> ModuleId {
        ModuleId::CLIPBOARD
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        self.enabled
            .then(|| cursio_protocol::ModuleAdvert::symmetric(ModuleId::CLIPBOARD, VERSION))
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(ClipboardEngine::new(ctx.peer_name().to_string())))
    }
}

/// Version of this module's wire vocabulary ([`ClipboardMsg`]). Bump when those
/// types change shape; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::ClipboardFormatOffer;

    /// A `TransportTx` that records every message sent through it, so tests can
    /// assert on what the engine put on the wire.
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

    fn offer(
        generation: ClipboardGeneration,
        formats: &[(ClipboardFormat, u64)],
    ) -> ClipboardOffer {
        ClipboardOffer {
            generation,
            formats: formats
                .iter()
                .map(|(f, s)| ClipboardFormatOffer {
                    format: f.clone(),
                    size: *s,
                })
                .collect(),
        }
    }

    /// Receiving an offer must queue a request for the richest pullable format —
    /// the whole point of the format-plural model: pull the best we can paste.
    #[tokio::test]
    async fn offer_pulls_the_richest_pullable_format() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.on_offer(&offer(
            1,
            &[(ClipboardFormat::Text, 5), (ClipboardFormat::Html, 20)],
        ));
        let req = eng
            .pending_request
            .take()
            .expect("an offer should queue a pull");
        assert_eq!(req.generation, 1);
        assert_eq!(
            req.format,
            ClipboardFormat::Html,
            "richer html beats plain text"
        );
    }

    /// An offer with nothing we can paste must not request anything.
    #[tokio::test]
    async fn offer_without_pullable_format_pulls_nothing() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.on_offer(&offer(1, &[(ClipboardFormat::Rtf, 10)]));
        assert!(eng.pending_request.is_none());
        assert!(eng.inbound.is_none());
    }

    /// A stale (older-or-equal) offer must be ignored: a copy already superseded
    /// must never overwrite the newer one we're tracking.
    #[tokio::test]
    async fn stale_offer_is_ignored() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.on_offer(&offer(5, &[(ClipboardFormat::Text, 3)]));
        eng.pending_request = None; // pretend we sent it
        eng.on_offer(&offer(4, &[(ClipboardFormat::Text, 3)]));
        assert!(
            eng.pending_request.is_none(),
            "older offer must not re-trigger a pull"
        );
        assert_eq!(eng.peer_gen, Some(5));
    }

    /// An oversized representation must not be pulled — we'd only refuse it.
    #[tokio::test]
    async fn oversized_offer_is_not_pulled() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.on_offer(&offer(
            1,
            &[(ClipboardFormat::ImagePng, MAX_PAYLOAD_BYTES + 1)],
        ));
        assert!(eng.pending_request.is_none());
    }

    /// A request for a copy we no longer hold must be answered with an explicit
    /// StaleGeneration failure, not dropped — the peer must be able to stop waiting.
    #[tokio::test]
    async fn request_for_superseded_copy_fails_stale() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.local_gen = 7;
        eng.on_request(&ClipboardRequest {
            generation: 3,
            format: ClipboardFormat::Text,
        });
        let f = eng
            .pending_failures
            .first()
            .expect("a stale request must be answered");
        assert_eq!(f.reason, ClipboardFailReason::StaleGeneration);
    }

    /// A request for a format the current copy didn't offer fails with FormatGone.
    #[tokio::test]
    async fn request_for_unoffered_format_fails_gone() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.local_gen = 2;
        eng.local_offer = Some(offer(2, &[(ClipboardFormat::Text, 4)]));
        eng.on_request(&ClipboardRequest {
            generation: 2,
            format: ClipboardFormat::ImagePng,
        });
        let f = eng
            .pending_failures
            .first()
            .expect("an unoffered format must be answered");
        assert_eq!(f.reason, ClipboardFailReason::FormatGone);
    }

    /// Chunks arriving in order for the tracked pull must reassemble, and the
    /// final chunk completes the transfer (clearing `inbound`).
    #[tokio::test]
    async fn inbound_chunks_reassemble_in_order() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.peer_gen = Some(1);
        eng.inbound = Some(InboundTransfer {
            generation: 1,
            format: ClipboardFormat::Text,
            buf: Vec::new(),
            next_seq: 0,
        });
        eng.on_data(&ClipboardData {
            generation: 1,
            format: ClipboardFormat::Text,
            seq: 0,
            last: false,
            data: b"hel".to_vec(),
        });
        // Mid-transfer: still collecting.
        assert!(eng.inbound.is_some());
        eng.on_data(&ClipboardData {
            generation: 1,
            format: ClipboardFormat::Text,
            seq: 1,
            last: true,
            data: b"lo".to_vec(),
        });
        // Last chunk consumed the transfer and (best-effort) wrote it out.
        assert!(eng.inbound.is_none(), "a completed pull is cleared");
        assert_eq!(eng.pulled, 1);
    }

    /// A gap in the sequence must abandon the pull rather than write a corrupt
    /// payload — reassembly can't be trusted once a chunk is missing.
    #[tokio::test]
    async fn gap_in_chunks_abandons_pull() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.inbound = Some(InboundTransfer {
            generation: 1,
            format: ClipboardFormat::Text,
            buf: Vec::new(),
            next_seq: 0,
        });
        // seq 1 arrives before seq 0: a hole.
        eng.on_data(&ClipboardData {
            generation: 1,
            format: ClipboardFormat::Text,
            seq: 1,
            last: false,
            data: b"x".to_vec(),
        });
        assert!(eng.inbound.is_none(), "a gap must abandon the transfer");
        assert_eq!(eng.pulled, 0);
    }

    /// Data for a generation other than the one being pulled must be dropped —
    /// the generation guard is what stops stale bytes being pasted as current.
    #[tokio::test]
    async fn data_for_wrong_generation_is_dropped() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.inbound = Some(InboundTransfer {
            generation: 2,
            format: ClipboardFormat::Text,
            buf: Vec::new(),
            next_seq: 0,
        });
        eng.on_data(&ClipboardData {
            generation: 1, // older
            format: ClipboardFormat::Text,
            seq: 0,
            last: true,
            data: b"stale".to_vec(),
        });
        // The tracked transfer is untouched; nothing was completed.
        assert!(eng.inbound.is_some());
        assert_eq!(eng.pulled, 0);
    }

    /// An outbound serve must chunk its bytes across `ClipboardData` messages and
    /// flag the final one, so the receiver knows when it's complete.
    #[tokio::test]
    async fn outbound_serve_chunks_and_flags_last() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.local_gen = 1;
        // Two-and-a-bit chunks worth of bytes.
        let bytes = vec![0u8; CHUNK_BYTES * 2 + 10];
        let total = bytes.len();
        eng.outbound.insert(
            (1, ClipboardFormat::ImagePng),
            OutboundTransfer {
                generation: 1,
                format: ClipboardFormat::ImagePng,
                bytes,
                offset: 0,
                next_seq: 0,
            },
        );

        // Pump until the serve drains (budget is bounded per call, so loop).
        let mut tx = RecordingTx::default();
        for _ in 0..10 {
            eng.pump_outbound(&mut tx).await.unwrap();
            if eng.outbound.is_empty() {
                break;
            }
        }
        assert!(eng.outbound.is_empty(), "serve should finish");

        // Sends are enveloped now, so the test decodes them the same way the
        // peer's module would.
        let chunks: Vec<ClipboardData> = tx
            .sent
            .iter()
            .filter_map(|m| {
                m.decode_module::<ClipboardMsg>(ModuleId::CLIPBOARD)
                    .unwrap()
            })
            .filter_map(|m| match m {
                ClipboardMsg::Data(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(chunks.len(), 3, "two full chunks plus a remainder");
        assert!(chunks.last().unwrap().last, "the final chunk is flagged");
        assert!(!chunks[0].last);
        // Seqs are contiguous from 0.
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.seq, i as u32);
        }
        // Every byte accounted for.
        let sent: usize = chunks.iter().map(|c| c.data.len()).sum();
        assert_eq!(sent, total);
    }

    /// A serve whose generation is no longer current must be dropped rather than
    /// streamed — the peer will ignore a superseded copy anyway.
    #[tokio::test]
    async fn outbound_serve_for_superseded_copy_is_dropped() {
        let mut eng = ClipboardEngine::new("peer".into());
        eng.local_gen = 5; // moved on
        eng.outbound.insert(
            (2, ClipboardFormat::Text),
            OutboundTransfer {
                generation: 2,
                format: ClipboardFormat::Text,
                bytes: vec![1, 2, 3],
                offset: 0,
                next_seq: 0,
            },
        );
        let mut tx = RecordingTx::default();
        eng.pump_outbound(&mut tx).await.unwrap();
        assert!(eng.outbound.is_empty());
        assert!(
            tx.sent.is_empty(),
            "nothing should be sent for a superseded serve"
        );
    }
}
