//! Where a call's voice goes: brought here when the user answers here, and
//! never moved back against a choice the user made.

use cursio_protocol::calls::Call;
use std::time::{Duration, Instant};

/// How long after a call starts this computer keeps putting its voice where
/// the call was taken. The phone opens and closes the audio link on its own
/// for a moment around answering; past this window a move is the user's.
pub(super) const STEER_FOR: Duration = Duration::from_secs(8);

/// Where the voice of a hands-free phone's call should be, decided once as
/// the call starts.
#[derive(Debug, Default)]
pub(super) struct VoicePolicy {
    /// Where the voice belongs and until when this computer steers it there.
    /// `None` until the call is taken somewhere.
    steer: Option<(bool, Instant)>,
    /// The last move asked of the phone, so a stream of call updates while
    /// the phone gets round to it does not ask again for each.
    asked: Option<bool>,
}

impl VoicePolicy {
    /// Answered or dialled from this computer: the voice comes here.
    pub(super) fn taken_here(&mut self, now: Instant) {
        self.steer = Some((true, now + STEER_FOR));
        self.asked = None;
    }

    /// The user moved the voice: theirs from now on, nothing is steered.
    pub(super) fn moved_by_user(&mut self) {
        self.steer = Some((false, Instant::now()));
        self.asked = None;
    }

    /// The phone's calls changed. A call that is up, or being placed, without
    /// having been taken here was taken at this desk if the user's hand was
    /// on it just then (`at_desk`, asked only at that moment), and on the
    /// phone otherwise, where its voice stays; with no call left everything
    /// starts over. Not `steering`, nothing is put anywhere.
    pub(super) fn calls(
        &mut self,
        calls: &[Call],
        now: Instant,
        steering: bool,
        at_desk: impl FnOnce() -> bool,
    ) {
        if calls.is_empty() {
            *self = VoicePolicy::default();
            return;
        }
        let started = calls.iter().any(|c| !c.state.is_incoming_offer());
        if started && self.steer.is_none() {
            if steering {
                self.steer = Some((at_desk(), now + STEER_FOR));
            } else {
                self.moved_by_user();
            }
        }
    }

    /// The move to ask of the phone now, if the voice is not where it
    /// belongs and this computer is still steering it.
    pub(super) fn correction(&mut self, audio_here: bool, now: Instant) -> Option<bool> {
        let (want, until) = self.steer?;
        if now >= until || audio_here == want || self.asked == Some(want) {
            return None;
        }
        self.asked = Some(want);
        Some(want)
    }
}
