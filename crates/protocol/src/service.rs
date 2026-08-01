//! Asking a peer to take on a duty this machine cannot perform.
//!
//! Some things exactly one machine in the mesh should be doing, and which one
//! is not always this one's choice. An AirPlay receiver is the first: it must
//! hold port 7000, and on a Mac that port belongs to Apple's own receiver, so
//! the machine the user is sitting at may be the one machine that *cannot*
//! host it.
//!
//! Deciding this by computation alone does not work. A node can rank itself
//! against its peers without exchanging a word — lowest id wins — but ranking
//! answers "who should" and the question is "who *can*". A machine that is
//! switched on, reachable, and simply unable to bind the port looks identical
//! to one that is ready, so the ranking hands the duty to a node that then
//! silently does nothing and the mesh ends up with no receiver at all.
//!
//! So it is a request, and it is generic on purpose. The vocabulary names a
//! [`ServiceId`] rather than AirPlay, because the shape — "I cannot do this,
//! will you" — is the same for every duty that has to live on exactly one
//! machine, and the next one should not need a second protocol.
//!
//! ## Refusal is an answer
//!
//! A peer that also cannot take the duty says so, with a reason. Silence would
//! be indistinguishable from a peer that never got the message, and the
//! requester would wait forever for a receiver nobody is running. Asking
//! several peers and collecting refusals is how a node learns that the answer
//! is "nowhere" and can say so to the user.

use serde::{Deserialize, Serialize};

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
