//! What a proven peer may do, apart from who it is.
//!
//! A user's console and a service's bulk lane may share one channel's
//! declaration, and proving an identity says nothing about which of them it
//! may use. A session bound to an identity can carry a policy from that
//! identity to the lanes and classes it may send in, asked the first time
//! the peer uses each class — the receiving end is the one that can enforce
//! it, since the sender's own restraint is not something to trust.
//!
//! A policy starts as a report: violations are told to the application
//! ([`crate::SessionEvent::Unauthorized`]) and the message is delivered, so a
//! policy can be rolled out and watched before it refuses anyone, the way
//! Leave's toolkit checks a caller against a method. Enforcing, a violation
//! ends the session and the message is not delivered.

use std::sync::Arc;

use crate::trust::PeerIdentity;
use crate::{ClassId, LaneId};

type Rule = dyn Fn(&PeerIdentity, LaneId, ClassId) -> bool + Send + Sync;

/// A policy from identity to the lanes and classes it may use.
#[derive(Clone)]
pub struct Authorization {
    pub(crate) enforce: bool,
    rule: Arc<Rule>,
}

impl Authorization {
    /// `allows` answers whether `identity` may send in `class`, which rides
    /// `lane`. With `enforce` false a refusal is only reported.
    pub fn new(
        enforce: bool,
        allows: impl Fn(&PeerIdentity, LaneId, ClassId) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            enforce,
            rule: Arc::new(allows),
        }
    }

    pub(crate) fn allows(&self, identity: &PeerIdentity, lane: LaneId, class: ClassId) -> bool {
        (self.rule)(identity, lane, class)
    }
}

/// What the receiving end has decided about each class so far.
pub(super) struct Verdicts(Vec<Option<bool>>);

impl Verdicts {
    pub(super) fn new(classes: usize) -> Self {
        Self(vec![None; classes])
    }

    /// The verdict on `class`, asking `decide` only the first time for a
    /// declared class, and every time for one past the declaration.
    pub(super) fn get(&mut self, class: ClassId, decide: impl FnOnce() -> bool) -> bool {
        match self.0.get_mut(class) {
            Some(Some(verdict)) => *verdict,
            Some(slot) => *slot.insert(decide()),
            None => decide(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_declared_class_is_decided_once() {
        let mut verdicts = Verdicts::new(2);
        let mut asked = 0;
        for _ in 0..3 {
            assert!(verdicts.get(1, || {
                asked += 1;
                true
            }));
        }
        assert_eq!(asked, 1);
        assert!(!verdicts.get(5, || false));
    }

    #[test]
    fn a_policy_sees_the_identity_lane_and_class() {
        let policy = Authorization::new(true, |who, lane, class| {
            matches!(who, PeerIdentity::User { .. }) && lane == LaneId::INTERACTIVE && class == 0
        });
        let user = PeerIdentity::User {
            sub: "u".into(),
            aud: "panel".into(),
        };
        assert!(policy.allows(&user, LaneId::INTERACTIVE, 0));
        assert!(!policy.allows(&user, LaneId::new(1), 2));
        assert!(!policy.allows(
            &PeerIdentity::Host { id: "h".into() },
            LaneId::INTERACTIVE,
            0
        ));
    }
}
