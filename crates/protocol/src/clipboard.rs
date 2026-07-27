//! Clipboard-sharing wire types.
//!
//! Cursio shares the *system clipboard* between machines: copy on one, paste on
//! the other, with images and every other representation preserved. The unit is
//! the same format-plural [`ClipboardContent`](clipboard_core::ClipboardContent)
//! the history manager and the platform backends already speak — a copy is a
//! *set* of representations (HTML + RTF + text; PNG + a bitmap), and the paster
//! picks the richest one it understands. Collapsing that set on the wire would
//! destroy formatting irreversibly, so these types carry it whole. See
//! `docs/clipboard-design.md`.
//!
//! ## The exchange: offer, then request
//!
//! Copies are frequent and *mostly never pasted on the peer*. Pushing every
//! copy's bytes eagerly would waste bandwidth and stall the shared-cursor input
//! stream that rides the same link (and images are megabytes). So the owner only
//! **announces** what it has — [`ClipboardOffer`], a list of formats and their
//! sizes, a few bytes — and sends bytes only when the peer decides it needs them
//! ([`ClipboardRequest`]). The native clipboards are lazy in exactly this way;
//! this mirrors the OS model rather than fighting it.
//!
//! ## Chunked, because a paste can be huge
//!
//! A requested representation streams as a sequence of [`ClipboardData`] chunks,
//! for two reasons: a payload can exceed [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN)
//! (16 MiB — a 4K screenshot alone approaches it), and chunking keeps the link
//! responsive, since input events interleave between chunks instead of one large
//! paste freezing the session. This is the same discipline
//! [`AudioChunk`](crate::AudioChunk) and [`WindowFrame`](crate::WindowFrame) use.
//!
//! ## Generation ids, because copies race
//!
//! Copying twice in quick succession means the peer may still be pulling
//! generation *n* when *n+1* is offered. Every offer, request, chunk and error
//! carries a [`ClipboardGeneration`], and a receiver drops anything that is not
//! the newest it has been offered — so stale bytes are never pasted as current
//! content.
//!
//! ## Explicit failures
//!
//! A request that cannot be served — the generation moved on, the format is gone,
//! the read failed, the payload is too large — is answered with
//! [`ClipboardFailed`] rather than silence, so the requester can fall back to a
//! different format instead of hanging forever.

use serde::{Deserialize, Serialize};

pub use clipboard_core::ClipboardFormat;

/// Identifies one clipboard copy within a session, monotonic per owner.
///
/// Bumped by the owner on every local clipboard change. Echoed on every message
/// about that copy so the receiver can tell a fresh copy from a stale one it is
/// still pulling (see the module docs). Session-scoped and opaque; not tied to
/// the history store's `seq`.
pub type ClipboardGeneration = u64;

/// One representation on offer: its format and how many bytes it is.
///
/// Deliberately payload-free — the whole point of the offer is to advertise
/// *without* transferring, so the peer only pulls what a paste actually needs.
/// `size` lets the receiver decide up front (e.g. refuse a 500 MB payload, or
/// prefer PNG over a larger bitmap) before asking for a single byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardFormatOffer {
    /// Which representation this is (`text/html`, `image/png`, an app-specific
    /// MIME type carried verbatim, …).
    pub format: ClipboardFormat,
    /// Size of this representation's payload in bytes.
    pub size: u64,
}

/// Owner -> peer: a new copy is available, here is what it offers.
///
/// Sent when the local clipboard changes (in
/// [`Message::ClipboardOffer`](crate::Message::ClipboardOffer)). Carries the
/// generation and the set of representations with their sizes — but no bytes.
/// The peer records the offer and, if and when it wants to paste, asks for the
/// one format it needs with a [`ClipboardRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardOffer {
    /// Which copy this is; bumped on every local change.
    pub generation: ClipboardGeneration,
    /// The representations available, each with its size. Order is not
    /// significant; the peer chooses by richness/size when it requests.
    pub formats: Vec<ClipboardFormatOffer>,
}

impl ClipboardOffer {
    /// Whether a given format is among those offered.
    pub fn has(&self, format: &ClipboardFormat) -> bool {
        self.formats.iter().any(|f| &f.format == format)
    }

    /// The offered format the given consumer would prefer, by richness.
    ///
    /// The negotiation half of the format-plural model: the consumer states what
    /// it accepts, and the richest offered representation among those wins (an
    /// image over its textual description, HTML over plain text). `None` means no
    /// overlap — nothing here the consumer can use.
    pub fn best_for(&self, accepted: &[ClipboardFormat]) -> Option<&ClipboardFormat> {
        self.formats
            .iter()
            .map(|f| &f.format)
            .filter(|f| accepted.contains(f))
            .max_by_key(|f| f.richness())
    }
}

/// Peer -> owner: send me the bytes of this format from this copy.
///
/// Emitted when the peer actually needs the content — its user pasted, or the
/// receiver eagerly pulls the best representation so a later paste is instant.
/// The owner replies with a stream of [`ClipboardData`] for the same
/// `generation`/`format`, or a [`ClipboardFailed`] if it cannot serve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardRequest {
    /// The copy being requested. If the owner has since moved on, it answers
    /// [`ClipboardFailed`] with [`StaleGeneration`](ClipboardFailReason::StaleGeneration).
    pub generation: ClipboardGeneration,
    /// Which representation to send.
    pub format: ClipboardFormat,
}

/// Owner -> peer: one chunk of a requested representation.
///
/// Chunks for one `(generation, format)` arrive in order (`seq` starting at 0),
/// the final one flagged `last`. The receiver concatenates `data` in `seq`
/// order and, on `last`, has the complete payload — which it then writes to its
/// local clipboard. A chunk whose `generation` is not the newest the receiver
/// has been offered is dropped (see the module docs on generations).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardData {
    /// Which copy this belongs to.
    pub generation: ClipboardGeneration,
    /// Which representation this is a chunk of; a copy's formats stream
    /// independently, so this disambiguates concurrent transfers.
    pub format: ClipboardFormat,
    /// 0-based position of this chunk within the format's byte stream, for
    /// ordering and gap detection.
    pub seq: u32,
    /// Whether this is the final chunk. On `true`, the payload is complete.
    pub last: bool,
    /// This chunk's bytes. Sized so a chunk stays well under
    /// [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN) with framing overhead to spare.
    pub data: Vec<u8>,
}

/// Why a [`ClipboardRequest`] could not be served.
///
/// Enumerated (not a free string) so the requester can react to the *kind* of
/// failure — retry a different format on [`FormatGone`](ClipboardFailReason::FormatGone),
/// give up on [`StaleGeneration`](ClipboardFailReason::StaleGeneration) and wait
/// for the next offer, surface [`TooLarge`](ClipboardFailReason::TooLarge) to
/// the user — rather than treating every failure the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardFailReason {
    /// A newer copy has replaced the requested generation; the bytes no longer
    /// exist. The requester should wait for (or use) the newer offer.
    StaleGeneration,
    /// The requested format is no longer on the clipboard. Some sources render
    /// representations on demand and can fail at that point; the requester may
    /// retry with another format from the same offer.
    FormatGone,
    /// The owner could not read the content (its serving application quit, or a
    /// backend error). Carries a short human-readable detail for logs.
    ReadFailed(String),
    /// The payload exceeds this side's transfer limit. Not an error to retry;
    /// the copy is simply too big to share.
    TooLarge,
}

/// Owner -> peer: a requested representation cannot be delivered.
///
/// Answers a [`ClipboardRequest`] that failed, so the requester falls back or
/// gives up instead of hanging. Carries the same `generation`/`format` the
/// request named, plus the [`ClipboardFailReason`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardFailed {
    /// The copy the failed request named.
    pub generation: ClipboardGeneration,
    /// The representation that could not be served.
    pub format: ClipboardFormat,
    /// Why it could not be served.
    pub reason: ClipboardFailReason,
}

/// Everything the clipboard module says on the wire.
///
/// This is the module's *own* vocabulary, carried inside
/// [`Message::Module`](crate::Message::Module) rather than as variants of
/// `Message`. Changing it — adding a format, a new failure reason, a
/// cancellation — versions this module alone via
/// [`ModuleAdvert::version`](crate::ModuleAdvert), so a peer that only wanted
/// input and audio is unaffected. See `docs/platform-design.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardMsg {
    /// Owner -> peer: a new local copy exists; here are its formats and sizes,
    /// *without* bytes.
    Offer(ClipboardOffer),
    /// Peer -> owner: send the bytes of one format from an offered copy.
    Request(ClipboardRequest),
    /// Owner -> peer: one chunk of a requested representation.
    Data(ClipboardData),
    /// Owner -> peer: a requested representation cannot be delivered, so the
    /// requester can fall back instead of hanging.
    Failed(ClipboardFailed),
}

impl ClipboardMsg {
    /// Scheduling class for clipboard traffic: all of it is bulk.
    ///
    /// Offers are tiny and requests tinier, but they are still latency-tolerant,
    /// and keeping the whole module in one class means a paste can never be
    /// reordered around its own offer. The payload chunks are the reason `Bulk`
    /// exists at all — a 4K screenshot must never delay the mouse.
    pub const PRIORITY: crate::Priority = crate::Priority::Bulk;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> ClipboardOffer {
        ClipboardOffer {
            generation: 3,
            formats: vec![
                ClipboardFormatOffer { format: ClipboardFormat::Text, size: 5 },
                ClipboardFormatOffer { format: ClipboardFormat::Html, size: 20 },
                ClipboardFormatOffer { format: ClipboardFormat::ImagePng, size: 4096 },
            ],
        }
    }

    #[test]
    fn best_for_prefers_richer_offered_format() {
        let o = offer();
        // A consumer taking text+html should be told to pull the richer html.
        assert_eq!(
            o.best_for(&[ClipboardFormat::Text, ClipboardFormat::Html]),
            Some(&ClipboardFormat::Html)
        );
        // One that only takes text still gets a usable answer.
        assert_eq!(o.best_for(&[ClipboardFormat::Text]), Some(&ClipboardFormat::Text));
        // The image wins over everything textual when accepted.
        assert_eq!(
            o.best_for(&[ClipboardFormat::Text, ClipboardFormat::ImagePng]),
            Some(&ClipboardFormat::ImagePng)
        );
    }

    #[test]
    fn best_for_is_none_without_overlap() {
        let o = offer();
        assert_eq!(o.best_for(&[ClipboardFormat::Rtf]), None);
    }

    #[test]
    fn has_reflects_offered_formats() {
        let o = offer();
        assert!(o.has(&ClipboardFormat::ImagePng));
        assert!(!o.has(&ClipboardFormat::Rtf));
    }
}
