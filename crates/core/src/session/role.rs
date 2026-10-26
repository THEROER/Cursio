//! Who holds the shared cursor: the two-machine ownership state, how a
//! link decides it at the start, and what a change tells the machine.

use cursio_protocol::{DeviceKind, PeerId};

/// A request to move the shared cursor to the other machine, delivered to a
/// running session from outside (e.g. a hotkey handler in the binary).
///
/// A request to change ownership, fed to [`run_session`](super::run_session) on its command channel.
///
/// [`Toggle`](OwnershipCmd::Toggle) is the symmetric hotkey handoff (flip
/// whichever side we're on). [`HandoffToPeer`](OwnershipCmd::HandoffToPeer) is
/// the *directional* edge-crossing handoff: it only makes sense from the active
/// side (the cursor left us across the peer-facing edge), and it carries the
/// crossing geometry so the peer knows where the cursor enters. The caller
/// (which owns the screen topology) is responsible for only emitting
/// `HandoffToPeer` for a crossing on the edge that actually borders the peer.
#[derive(Debug, Clone, Copy)]
pub enum OwnershipCmd {
    /// Flip which machine is active (hand the shared cursor to the other side).
    Toggle,
    /// Hand the cursor to the peer because it crossed the peer-facing edge.
    /// `edge` is *our* edge that was crossed; `pos_norm` (`0.0`..=`1.0`) is where
    /// along it. Ignored unless we're currently active.
    HandoffToPeer {
        edge: cursio_protocol::Edge,
        pos_norm: f32,
        /// The point it left by, in capture's pixels, which decides whose
        /// screen it went to when the desk has several along one edge.
        at: Option<(i32, i32)>,
        /// Where it comes in on the peer, in the peer's pixels; filled by
        /// the input hub from the desk ([`crate::desk::Seam::entry`]).
        entry: Option<(i32, i32)>,
    },
    /// Emergency release: unconditionally take control back to THIS machine —
    /// become active, tell the peer to go dormant, and let the caller drop any
    /// grab/held keys and restore the cursor. The panic hotkey. Meant to work
    /// even if the peer is wedged, so its own grab release happens directly in
    /// the capture path (not only here); this variant handles the session-side
    /// cleanup and best-effort notifies the peer.
    Panic,
}

/// Which machine holds the shared cursor.
///
/// There is one cursor and every machine can move it, so there are two states
/// and not three: the machine holding it acts on itself, and every other
/// machine grabs its own input and streams it to the holder. Whichever mouse
/// the user reaches for drives the cursor, wherever it happens to be.
///
/// A third state was tried — "holds nothing and drives nobody", entered by a
/// machine that had handed the cursor back — and it is what made a mouse stop
/// working: that machine's pointer went local, so one mouse drove both screens
/// and the other drove neither. Which machine the user is sitting at cannot be
/// known here, and does not need to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRole {
    /// This machine holds the shared cursor.
    active: bool,
}

impl CursorRole {
    /// The state a session starts in. Nobody has handed anything anywhere.
    pub fn new(start_active: bool) -> Self {
        Self {
            active: start_active,
        }
    }

    pub fn active(self) -> bool {
        self.active
    }

    /// Whether this machine's own input is streamed to the peer.
    ///
    /// Exactly "does somebody else hold the cursor". A machine that is not
    /// holding it is the one whose mouse and keyboard have somewhere else to
    /// go.
    pub fn driving(self) -> bool {
        !self.active
    }

    /// Send the cursor to the peer, by the hotkey or across an edge.
    ///
    /// Refused unless we hold it: you cannot send away what you do not have,
    /// and without the check a pointer merely drifting into the edge of an
    /// unheld screen would drag the cursor off the screen the user is actually
    /// looking at.
    pub fn hand_over(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
    }

    /// Record that the peer sent its cursor here.
    pub fn take_over(&mut self) {
        self.active = true;
    }

    /// Take the cursor because the user asked for it here (the hotkey).
    ///
    /// Same destination as [`Self::take_over`], kept separate because the two
    /// read differently at the call sites and one of them is allowed from a
    /// machine that holds nothing.
    pub fn seize(&mut self) {
        self.active = true;
    }

    /// The hotkey, which flips whichever side we are on.
    pub fn toggle(&mut self) {
        if self.active {
            self.hand_over();
        } else {
            self.seize();
        }
    }

    /// Adopt the state the peer dictates with [`Message::Grab`](cursio_protocol::Message::Grab).
    pub fn adopt(&mut self, active: bool) {
        self.active = active;
    }

    /// Emergency: take everything back to this machine.
    pub fn panic(&mut self) {
        self.active = true;
    }
}

/// How a session treats the shared cursor, decided before the loop starts.
///
/// Two machines with screens pass the cursor back and forth, and which one
/// starts with it is settled by comparing peer ids — deterministic, and each
/// side computes the opposite answer. But not every peer is such a machine: a
/// phone acting as a touchpad has no screen for the cursor to enter and no
/// injector to drive it ([`DeviceKind::takes_cursor`]), so there is nothing to
/// pass back and forth. It only ever *produces* input.
///
/// Handing the cursor to a peer that cannot hold it is not a cosmetic mistake.
/// The machine that gives it up grabs its own input and streams it away, so it
/// stops responding to its own keyboard and mouse — and the only thing that
/// could hand control back is the peer that was never able to take it. The
/// session would have to be killed to recover. That is why this is settled up
/// front and enforced in the loop, rather than left to the id comparison that
/// knows nothing about what a peer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSetup {
    /// Whether this machine begins holding the shared cursor.
    pub start_active: bool,
    /// Whether the peer can ever hold it at all.
    pub peer_takes_cursor: bool,
}

impl CursorSetup {
    /// Work out the arrangement for a link between these two machines.
    ///
    /// Both sides run this and reach complementary answers, which is what keeps
    /// "exactly one machine is active" true without negotiating it over the
    /// wire.
    pub fn negotiate(
        local_kind: DeviceKind,
        local_id: PeerId,
        peer_kind: DeviceKind,
        peer_id: PeerId,
    ) -> Self {
        let peer_takes_cursor = peer_kind.takes_cursor();
        let start_active = match (local_kind.takes_cursor(), peer_takes_cursor) {
            // The ordinary case: two machines with screens. Compare ids, not
            // who dialled whom — the latter flips when the same two machines
            // reconnect the other way round, landing the cursor somewhere
            // different each time.
            (true, true) => local_id < peer_id,
            // Only we can hold it, so we hold it, always.
            (true, false) => true,
            // We cannot hold it: this machine is the input source. Staying
            // dormant is exactly what makes its input stream to the peer.
            (false, _) => false,
        };
        CursorSetup {
            start_active,
            peer_takes_cursor,
        }
    }
}

/// Reports this machine's new **active** state after a change, so the caller can
/// match local input handling to it.
///
/// Exactly one machine is active at a time (a hard invariant). When
/// `active = true` this machine holds the shared cursor: local input acts
/// normally and inbound peer input is injected here. When `active = false` this
/// machine is dormant: its local input is grabbed and streamed to the (active)
/// peer, and nothing inbound is injected.
#[derive(Debug, Clone, Copy)]
pub struct Ownership {
    /// Whether this machine now holds the shared cursor.
    pub active: bool,
    /// Set when this change came from the emergency panic release, not a normal
    /// handoff. The caller should recover to a *safe idle* — ungrab, drop
    /// barriers entirely, show the cursor — rather than the usual active setup
    /// (which on Wayland would re-arm barriers). Normal handoffs leave this
    /// `false`.
    pub panic: bool,
    /// Whether this machine is streaming its input to the peer.
    ///
    /// Not the opposite of [`active`](Self::active), and that is the whole
    /// point. A machine drives the peer only when the user *at this machine*
    /// sent the cursor across — by the hotkey or by shoving it off the peer
    /// edge. A machine that has handed nothing over is one the user may not
    /// even be sitting at, and grabbing its keyboard because it does not hold
    /// the shared cursor is what made a Mac's trackpad drive a Windows cursor
    /// from the moment the link came up, while the Mac's own pointer never
    /// moved again.
    ///
    /// Never set by something the peer said: taking the cursor back tells us to
    /// stop driving, never to start.
    pub driving: bool,
    /// Where the pointer comes in, when the peer sent it across an edge.
    /// `None` for every other handoff, which leaves the pointer where it was.
    pub entry: Option<Entry>,
}

/// Where a pointer the peer sent across an edge comes in here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    /// Which of this machine's edges.
    pub edge: cursio_protocol::Edge,
    /// How far along it (`0.0..=1.0`) the pointer left the peer's.
    pub pos_norm: f32,
    /// The exact point, in this machine's pixels, when the peer worked it
    /// out from the shared desk.
    pub at: Option<(i32, i32)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::id;

    /// Two machines with screens: the lower id starts with the cursor, and each
    /// side reaches the opposite answer without asking the other.
    ///
    /// This is the invariant the whole ownership model rests on — two actives
    /// means both machines inject each other's input, which is the feedback
    /// loop the design exists to make impossible.
    #[test]
    fn two_desktops_agree_on_exactly_one_holder() {
        let low = id(1);
        let high = id(2);
        let a = CursorSetup::negotiate(DeviceKind::Desktop, low, DeviceKind::Laptop, high);
        let b = CursorSetup::negotiate(DeviceKind::Laptop, high, DeviceKind::Desktop, low);
        assert!(a.start_active);
        assert!(!b.start_active);
        assert!(a.peer_takes_cursor && b.peer_takes_cursor);
    }

    /// A phone peer never gets the cursor, whichever way the ids fall.
    ///
    /// The ids alone decided this before, so half of all phone pairings put the
    /// desktop into the dormant state: it grabs its own keyboard and mouse,
    /// streams them to a device with nothing to inject them into, and the only
    /// thing that could hand control back is that same device. The user's
    /// machine stops responding and the session has to be killed. Whichever id
    /// the phone happens to have must not be able to cause that.
    #[test]
    fn a_phone_peer_never_takes_the_cursor() {
        for phone_id in [id(0), id(255)] {
            let setup =
                CursorSetup::negotiate(DeviceKind::Desktop, id(128), DeviceKind::Phone, phone_id);
            assert!(
                setup.start_active,
                "the desktop must hold the cursor against a phone with id {phone_id:?}"
            );
            assert!(!setup.peer_takes_cursor);
        }
    }

    /// The phone's own view: dormant, which is precisely what makes its input
    /// stream to the peer rather than act locally.
    #[test]
    fn a_phone_stays_dormant_so_its_input_is_forwarded() {
        let setup = CursorSetup::negotiate(DeviceKind::Phone, id(1), DeviceKind::Desktop, id(2));
        assert!(!setup.start_active);
        assert!(setup.peer_takes_cursor, "the desktop can hold the cursor");

        // And the two sides still agree: the desktop holds it.
        let desktop = CursorSetup::negotiate(DeviceKind::Desktop, id(2), DeviceKind::Phone, id(1));
        assert!(desktop.start_active);
    }

    /// A headless box has no screen either, so it is treated like the phone.
    #[test]
    fn a_headless_peer_is_not_a_cursor_destination() {
        let setup = CursorSetup::negotiate(DeviceKind::Desktop, id(9), DeviceKind::Headless, id(1));
        assert!(setup.start_active);
        assert!(!setup.peer_takes_cursor);
    }

    /// Two machines, one user, replayed as the pair of state machines they
    /// really are. Written because the alternative — reading it back off two
    /// logs side by side — is how three separate versions of this shipped
    /// broken.
    struct Pair {
        /// Where the user is sitting.
        here: CursorRole,
        there: CursorRole,
    }

    impl Pair {
        fn new() -> Self {
            Self {
                here: CursorRole::new(true),
                there: CursorRole::new(false),
            }
        }

        /// The cursor leaves `here` across the shared edge.
        fn push_across(&mut self) {
            self.here.hand_over();
            self.there.take_over();
        }

        /// And back the other way.
        fn push_back(&mut self) {
            self.there.hand_over();
            self.here.take_over();
        }
    }

    /// Whichever machine is not holding the cursor is streaming its input to
    /// the one that is — from the very first moment, with nothing handed
    /// anywhere yet.
    ///
    /// This is the whole model: one cursor, and every mouse can move it. The
    /// alternative was a machine that held nothing *and* streamed nothing, and
    /// it is what made a real mouse stop working — see [`CursorRole`].
    #[test]
    fn the_machine_without_the_cursor_streams_its_input() {
        let pair = Pair::new();
        assert!(pair.here.active(), "here starts with the cursor");
        assert!(!pair.here.driving(), "so it acts on itself");
        assert!(!pair.there.active());
        assert!(
            pair.there.driving(),
            "the far machine's mouse must reach the cursor, not its own desktop"
        );
    }

    /// Sending the cursor across swaps the two roles, and nothing else.
    #[test]
    fn sending_the_cursor_across_swaps_who_streams() {
        let mut pair = Pair::new();
        pair.push_across();
        assert!(pair.there.active(), "the far machine holds the cursor");
        assert!(pair.here.driving(), "and this one now streams to it");
        assert!(!pair.there.driving());
    }

    /// The failure this is all for: after a round trip, *both* mice still
    /// work. Sending the cursor back used to leave the returning machine
    /// holding nothing and driving nobody — a mouse that moved its own pointer
    /// and nothing else, with the only way out being to shove the cursor home
    /// by hand from the far end.
    #[test]
    fn a_round_trip_leaves_both_mice_working() {
        let mut pair = Pair::new();
        pair.push_across();
        pair.push_back();
        assert!(pair.here.active(), "the cursor came home");
        assert!(
            pair.there.driving(),
            "and the machine that returned it still reaches the cursor"
        );

        // The second crossing is the one that used to break.
        pair.push_across();
        assert!(pair.there.active());
        assert!(pair.here.driving(), "this machine streams again");
    }

    /// Peer ids choose an initial holder, not which physical mouse the user is
    /// allowed to use — but taking the cursor *back* is the hotkey's job, not
    /// the edge's.
    ///
    /// The edge cannot serve here: the cursor is not on this screen to be
    /// pushed off it, and honouring the crossing anyway meant a pointer merely
    /// drifting into the edge dragged the cursor away from the screen the user
    /// was looking at. Every backend but macOS already refused to report it.
    #[test]
    fn a_machine_without_the_cursor_takes_it_with_the_hotkey_not_the_edge() {
        let mut holder = CursorRole::new(true);
        let mut source = CursorRole::new(false);

        // The edge does nothing when the cursor is elsewhere.
        source.hand_over();
        assert!(!source.active(), "the edge must not pull the cursor over");
        assert!(holder.active(), "and the holder still holds it");

        // The hotkey does.
        source.toggle();
        holder.adopt(false);
        assert!(source.active());
        assert!(!holder.active());
        assert!(
            holder.driving(),
            "the machine that gave it up still reaches the cursor"
        );
    }

    #[test]
    fn many_round_trips_never_leave_both_or_neither_in_charge() {
        let mut pair = Pair::new();
        for trip in 0..10 {
            pair.push_across();
            assert!(pair.there.active(), "trip {trip}: cursor did not cross");
            assert!(pair.here.driving(), "trip {trip}: sender stopped streaming");
            assert!(
                !pair.there.driving(),
                "trip {trip}: holder is streaming too"
            );
            pair.push_back();
            assert!(pair.here.active(), "trip {trip}: cursor did not come home");
            assert!(
                pair.there.driving(),
                "trip {trip}: far mouse went dead after returning the cursor"
            );
        }
    }

    /// Exactly one machine holds the cursor at every point, which is the
    /// invariant an input feedback loop violates.
    #[test]
    fn exactly_one_machine_holds_the_cursor() {
        let mut pair = Pair::new();
        for step in 0..6 {
            assert_ne!(
                pair.here.active(),
                pair.there.active(),
                "step {step}: both or neither hold it"
            );
            if step % 2 == 0 {
                pair.push_across();
            } else {
                pair.push_back();
            }
        }
    }

    /// The hotkey has to work from either side and leave the same state a
    /// crossing would.
    #[test]
    fn the_hotkey_matches_what_an_edge_crossing_does() {
        let mut hotkey = Pair::new();
        hotkey.here.toggle();
        hotkey.there.adopt(true);

        let mut edge = Pair::new();
        edge.push_across();

        assert_eq!(hotkey.here.driving(), edge.here.driving());
        assert_eq!(hotkey.here.active(), edge.here.active());
        assert_eq!(hotkey.there.active(), edge.there.active());
    }

    /// Being told to stand down hands the cursor away and leaves this mouse
    /// pointed at wherever it went.
    #[test]
    fn standing_down_streams_to_the_new_holder() {
        let mut role = CursorRole::new(true);
        role.adopt(false);
        assert!(!role.active());
        assert!(role.driving(), "this mouse must still reach the cursor");
    }

    /// The exact sequence from the 18:23 logs, in the order it happened.
    ///
    /// A pointer drifting into the edge of a screen that does *not* hold the
    /// cursor used to drag it over, and from there the cursor could not come
    /// back — the peer held it and this side dismissed every crossing. The edge
    /// must stay inert until the cursor is actually on this screen to be pushed
    /// off it.
    #[test]
    fn the_edge_is_inert_on_a_screen_the_cursor_has_left() {
        let mut mac = CursorRole::new(false);
        let mut windows = CursorRole::new(true);

        // The user at Windows shoves the cursor across.
        windows.hand_over();
        mac.take_over();
        assert!(mac.active());
        assert!(windows.driving(), "Windows' own mouse still reaches it");

        // And pushes it back.
        mac.hand_over();
        windows.take_over();
        assert!(!mac.active());
        assert!(windows.active());
        assert!(mac.driving(), "the Mac's mouse still reaches the cursor");

        // Now the Mac's pointer wanders into its edge, repeatedly. The cursor
        // is not there to be sent anywhere.
        let handed_back = mac;
        for _ in 0..5 {
            mac.hand_over();
        }
        assert_eq!(
            mac, handed_back,
            "the edge must be inert without the cursor"
        );
        assert!(
            windows.active(),
            "the cursor is still where the user left it"
        );

        // And the ordinary way across still works.
        windows.hand_over();
        mac.take_over();
        assert!(mac.active());
        assert!(windows.driving());
    }

    /// The panic hotkey exists for a wedged link, so it must reach a state this
    /// machine can be used from whatever it was doing.
    #[test]
    fn panic_returns_everything_to_this_machine() {
        let mut role = CursorRole::new(true);
        role.hand_over();
        assert!(role.driving());
        role.panic();
        assert!(role.active());
        assert!(!role.driving());
    }
}
