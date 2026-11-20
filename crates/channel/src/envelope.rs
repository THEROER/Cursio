//! What a message is to the channel: the class it is scheduled in, what a
//! late copy of it is worth, and its bytes.
//!
//! The two declarations are independent and both belong to the message's
//! owner, not to the channel: sound is urgent and worthless once late, a
//! directory listing urgent and needed however late, a file chunk neither.

use crate::{ChannelError, ClassId};

/// How much a link must do to deliver a message: the other half of what a
/// message's kind declares, beside its class.
///
/// The class says what goes *first*; this says what a late copy is *worth*.
///
/// Not on the wire. Each end decides for what it sends, which is why a peer
/// of any version can be sent to, and why a message's choice can change
/// without a protocol change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Arrives exactly once and in order within its lane, resent after a
    /// dropped connection. A key press, a release, a file request: lost, it
    /// leaves a key held or a process asleep in a filesystem call forever.
    Reliable,
    /// Describes the present, so a copy that missed its moment is worth less
    /// than nothing: pointer motion, sound, pixels.
    ///
    /// Never kept for resending after a dropped connection. Replayed, five
    /// seconds of buffered motion jump the cursor by all of it at once and old
    /// sound plays over new — Mosh's lesson, that a mobile link should deliver
    /// the latest state rather than the history.
    ///
    /// With `within`, a sender that has not started the message that long
    /// after it was queued drops it whole instead, so a link coming back from
    /// a stall spends itself on what is current rather than on a backlog the
    /// receiver would throw away on arrival. The receiver sees a gap, as it
    /// would after a dropped connection, and must already cope with one.
    Fresh { within: Option<std::time::Duration> },
}

/// A message the channel can carry.
pub trait Envelope: Clone + Send + 'static {
    /// Index into the channel's [`Classes`](crate::Classes), highest priority
    /// first.
    fn class(&self) -> ClassId;

    fn delivery(&self) -> Delivery {
        Delivery::Reliable
    }

    /// For a [`Delivery::Reliable`] message on a resumable session: the
    /// instant, on the sender's wall clock, past which it must not reach the
    /// peer — a command run late is worse than one refused. Past it the
    /// message is neither sent nor resent after a resume, and is reported
    /// ([`SessionEvent::Expired`](crate::SessionEvent::Expired)) rather than
    /// dropped quietly. Judged only before any of it has reached the peer: a
    /// message already partly delivered is finished.
    ///
    /// Apart from [`Delivery::Fresh`]'s `within`, which is measured against
    /// the sender's own queue and suits samples, not commands; ignored for a
    /// fresh message.
    fn expires_at(&self) -> Option<std::time::SystemTime> {
        None
    }

    fn encode(&self) -> Result<Vec<u8>, ChannelError>;

    fn decode(bytes: &[u8]) -> Result<Self, ChannelError>;

    /// Whether a later message may be folded into this one while it waits
    /// unsent; see [`Self::fold`]. Only such messages are kept decoded beside
    /// their frames, so the answer costs a clone when it is `true`.
    fn folds(&self) -> bool {
        false
    }

    /// Fold `next` into `self` while both wait unsent — pointer motion in
    /// Cursio, a progress counter elsewhere. `false` queues `next` behind.
    ///
    /// Asked only when both [`Self::folds`]. The folded message keeps the
    /// older one's place and age, so it must still say what both did.
    fn fold(&mut self, _next: &Self) -> bool {
        false
    }

    /// A person is acting through this message: background traffic yields
    /// while such messages flow (Cursio's input).
    fn is_activity(&self) -> bool {
        false
    }
}
