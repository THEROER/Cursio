//! Asking a peer to take on a duty this machine cannot perform.
//!
//! Some things exactly one machine in the mesh should be doing, and which one
//! is not always this one's choice. An AirPlay receiver is the first: it must
//! hold port 7000, and on a Mac that port belongs to Apple's own receiver, so
//! the machine the user is sitting at may be the one machine that *cannot*
//! host it.
//!
//! Placement uses capabilities carried in the greeting, including whether the
//! port was available when the node started. The request path remains as the
//! runtime fallback when that fact changes after the greeting. The vocabulary
//! names a [`ServiceId`] rather than AirPlay because the shape — "I cannot do
//! this, will you" — is the same for every duty that has to live on exactly one
//! machine.
//!
//! ## Refusal is an answer
//!
//! A peer that also cannot take the duty says so, with a reason. Silence would
//! be indistinguishable from a peer that never got the message, and the
//! requester would wait forever for a receiver nobody is running. Asking
//! several peers and collecting refusals is how a node learns that the answer
//! is "nowhere" and can say so to the user.

use serde::{Deserialize, Serialize};

/// A picture size and rate, in the vocabulary two machines can compare.
///
/// Deliberately not the AirPlay crate's `VideoProfile`: `protocol` names the
/// wire and depends on nothing, and a quality is a number a peer weighs against
/// its own rather than an enum whose variants would have to match on both
/// sides forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quality {
    pub width: u16,
    pub height: u16,
    pub fps: u16,
}

impl Quality {
    pub const fn new(width: u16, height: u16, fps: u16) -> Self {
        Self { width, height, fps }
    }

    /// Ordering for competing mesh requests: resolution first, then frame
    /// rate. A user asking for 1440p30 explicitly asked for more detail than
    /// 1080p60; comparing decoder throughput would silently ignore that
    /// request because the latter happens to process more pixels per second.
    pub fn selection_key(self) -> (u64, u16) {
        (u64::from(self.width) * u64::from(self.height), self.fps)
    }
}

impl std::fmt::Display for Quality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}@{}", self.width, self.height, self.fps)
    }
}

/// How well a machine could carry a duty, if asked.
///
/// Ranked, because placing a duty is a comparison. What it deliberately does
/// *not* claim is a maximum resolution: the only honest signals available are
/// whether the user let this machine take part and whether a hardware decoder
/// is installed, and inventing a pixel ceiling from a GPU's name is how a
/// machine that would have coped fine gets ruled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Fitness {
    /// Not taking part at all — the user said so.
    No,
    /// Willing, but with no hardware video decoder. Fine for answering an
    /// iPhone; a poor place to decode a picture, since this codebase refuses
    /// to fall back to software rather than ship something unwatchable.
    ControlOnly,
    /// Willing, and able to decode video on the GPU.
    Full,
}

impl Fitness {
    /// Whether this machine will take the duty on at all.
    pub fn takes_part(self) -> bool {
        self != Self::No
    }
}

/// What a node brings to, and wants from, a duty the mesh places.
///
/// Travels in the greeting rather than being asked for, because placement has
/// to be decided the moment two machines meet — before anything is bound, and
/// without a round trip that would leave a gap where both or neither host it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceOffer {
    /// How well this machine could hold the AirPlay receiver.
    pub fitness: Fitness,
    /// The quality this machine's user asked for, if they asked. The mesh runs
    /// the receiver at the best any node wants, so setting it on one machine
    /// raises it for all of them — which is the point: the request is about the
    /// picture, not about where it is decoded.
    pub wanted: Option<Quality>,
    /// Whether this node wants the mirrored screen shown locally. The receiver
    /// may live on another machine; that host uses this bit to offer the stream
    /// back over the ordinary window module.
    pub show_window: bool,
}

impl ServiceOffer {
    /// A node that is not taking part.
    pub fn none() -> Self {
        Self {
            fitness: Fitness::No,
            wanted: None,
            show_window: false,
        }
    }
}

impl Default for ServiceOffer {
    fn default() -> Self {
        Self::none()
    }
}

/// A duty exactly one machine in the mesh should carry.
///
/// A plain number so a peer built before a duty existed can carry its requests
/// through untouched, and refuse the ones it does not recognise, rather than
/// failing to decode the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServiceId(pub u16);

impl ServiceId {
    /// Receiving AirPlay from an iPhone: holding port 7000 and being the one
    /// name in the Screen Mirroring list.
    pub const AIRPLAY: ServiceId = ServiceId(1);

    /// Whether this build knows what the duty is. An unknown id is refused
    /// rather than accepted-and-ignored, so the requester keeps looking.
    pub fn is_known(self) -> bool {
        self == Self::AIRPLAY
    }

    /// Short name for logs and for the reason strings a refusal carries.
    pub fn name(self) -> &'static str {
        match self {
            Self::AIRPLAY => "airplay",
            _ => "unknown",
        }
    }
}

impl std::fmt::Display for ServiceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.is_known() {
            true => f.write_str(self.name()),
            false => write!(f, "service {}", self.0),
        }
    }
}

/// The service module's wire vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceMsg {
    /// "Is anybody already doing this?"
    ///
    /// Asked before taking a duty on rather than after. Two machines that both
    /// decide for themselves — by rank, by who booted first, by anything not
    /// involving the other — both decide yes, and an iPhone is offered two
    /// receivers called Cursio. Asking costs one round trip on a link that is
    /// already up and removes the guess entirely.
    Query { service: ServiceId },

    /// The answer: whether this machine is carrying `service`, and where.
    ///
    /// `addr` is where the duty is actually reachable, not where the mesh link
    /// is: a receiver binds its own port, and a peer that wants to point
    /// something at it needs that one. `None` when not hosting.
    Status {
        service: ServiceId,
        hosting: bool,
        addr: Option<std::net::SocketAddr>,
        /// What the duty is actually running at, when that is a thing it can
        /// run at. Carried so a peer can tell "somebody has it" from "somebody
        /// has it, worse than you asked for" — the second is the case where
        /// moving it is worth the interruption.
        quality: Option<Quality>,
    },

    /// "I cannot carry this duty; will you?"
    ///
    /// `reason` is what the asking machine could not do, in the words the user
    /// would need to understand it — "port 7000 is already in use". It travels
    /// so the accepting machine can log *why* it is now hosting something the
    /// user started elsewhere, which is otherwise a mystery on that machine.
    Request { service: ServiceId, reason: String },

    /// "Yes, I have it." Sent once the duty is actually running, not on
    /// receipt: an acceptance that arrives before the port is bound would let
    /// the requester stop looking for a host that never appears.
    Accepted { service: ServiceId },

    /// "No, and here is why." The requester keeps asking other peers.
    Refused { service: ServiceId, reason: String },

    /// "I am giving it up." Sent when a machine that had accepted can no
    /// longer serve, so whoever wanted it can ask again rather than pointing
    /// at a host that has quietly stopped.
    Released { service: ServiceId },
}

impl ServiceMsg {
    /// Which duty this message concerns, whatever kind it is.
    pub fn service(&self) -> ServiceId {
        match self {
            ServiceMsg::Query { service }
            | ServiceMsg::Status { service, .. }
            | ServiceMsg::Request { service, .. }
            | ServiceMsg::Accepted { service }
            | ServiceMsg::Refused { service, .. }
            | ServiceMsg::Released { service } => *service,
        }
    }
}

/// How much of a reason string is carried.
///
/// Reasons are for a human reading a log, and an unbounded string off a socket
/// is an unbounded allocation. Long enough for a sentence naming a port and a
/// cause; anything past that is noise a truncation does not lose.
pub const MAX_REASON_LEN: usize = 200;

/// Shorten a reason to what the wire carries.
pub fn truncate_reason(reason: &str) -> String {
    if reason.len() <= MAX_REASON_LEN {
        return reason.to_string();
    }
    // Cut on a character boundary: slicing a multi-byte character in half
    // panics, and reasons name real paths and device names.
    let mut end = MAX_REASON_LEN;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_names_the_duty_it_concerns() {
        let service = ServiceId::AIRPLAY;
        for msg in [
            ServiceMsg::Query { service },
            ServiceMsg::Status {
                service,
                hosting: true,
                addr: Some("192.168.1.5:7000".parse().unwrap()),
                quality: Some(Quality::new(1920, 1080, 60)),
            },
            ServiceMsg::Request {
                service,
                reason: "port 7000 is in use".into(),
            },
            ServiceMsg::Accepted { service },
            ServiceMsg::Refused {
                service,
                reason: "no".into(),
            },
            ServiceMsg::Released { service },
        ] {
            assert_eq!(msg.service(), service);
        }
    }

    /// An unrecognised duty has to be refusable, or a requester waits forever
    /// for a peer that accepted something it does not implement.
    #[test]
    fn an_unknown_service_is_not_mistaken_for_a_known_one() {
        assert!(ServiceId::AIRPLAY.is_known());
        assert!(!ServiceId(999).is_known());
        assert_eq!(ServiceId(999).to_string(), "service 999");
        assert_eq!(ServiceId::AIRPLAY.to_string(), "airplay");
    }

    /// A resolution request must win on resolution even when another profile
    /// happens to push more pixels per second.
    #[test]
    fn quality_requests_prioritise_resolution_then_rate() {
        let fhd60 = Quality::new(1920, 1080, 60);
        let qhd30 = Quality::new(2560, 1440, 30);
        let qhd60 = Quality::new(2560, 1440, 60);
        assert!(qhd30.selection_key() > fhd60.selection_key());
        assert!(qhd60.selection_key() > fhd60.selection_key());
        assert!(qhd60.selection_key() > qhd30.selection_key());
        assert_eq!(fhd60.to_string(), "1920x1080@60");
    }

    /// Placement is a comparison, so the ordering is the whole point: a machine
    /// with a hardware decoder must outrank one without, and both must outrank
    /// a machine whose user switched AirPlay off.
    #[test]
    fn fitness_ranks_a_gpu_above_a_bare_receiver_above_nothing() {
        assert!(Fitness::Full > Fitness::ControlOnly);
        assert!(Fitness::ControlOnly > Fitness::No);
        assert!(Fitness::Full.takes_part());
        assert!(Fitness::ControlOnly.takes_part());
        assert!(!Fitness::No.takes_part());
    }

    /// A node that says nothing about AirPlay must not be read as volunteering
    /// for it: an older peer's greeting decodes to the default.
    #[test]
    fn an_offer_defaults_to_taking_no_part() {
        assert_eq!(ServiceOffer::default(), ServiceOffer::none());
        assert!(!ServiceOffer::default().fitness.takes_part());
        assert_eq!(ServiceOffer::default().wanted, None);
        assert!(!ServiceOffer::default().show_window);
    }

    #[test]
    fn a_long_reason_is_shortened_rather_than_carried() {
        let long = "x".repeat(MAX_REASON_LEN * 2);
        assert_eq!(truncate_reason(&long).len(), MAX_REASON_LEN);
        assert_eq!(truncate_reason("short"), "short");
    }

    /// Reasons name real paths and device names, and slicing a multi-byte
    /// character in half panics.
    #[test]
    fn shortening_never_splits_a_character() {
        let long = "я".repeat(MAX_REASON_LEN);
        let short = truncate_reason(&long);
        assert!(short.len() <= MAX_REASON_LEN);
        assert!(long.starts_with(&short));
    }
}
