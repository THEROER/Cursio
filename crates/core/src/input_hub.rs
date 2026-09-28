//! Where this machine's one mouse and keyboard go when it has several peers.
//!
//! Capture is a property of the machine: one portal permission, one hook, one
//! grab. It used to be lent whole to whichever peer session came up first, and
//! every later session ran without local input — so with a neighbour to the
//! right and another below, pushing the cursor down still sent it right, and
//! the one watched edge belonged to whichever peer had greeted last. Neither
//! was a bug in a backend; there was simply nothing that knew more than one
//! peer existed.
//!
//! The hub is that thing. Capture's forwarders hand every event and every
//! ownership request to it, and it passes each to exactly one session:
//!
//! - **An edge crossing** goes to the nearest peer beyond that edge — the
//!   rule [`cursio_protocol::neighbour_across`] states, with distances taken
//!   when each link came up. A crossing no peer sits beyond goes nowhere.
//! - **Captured input** goes to the peer this machine is streaming to: the
//!   one whose session last reported it is driving. Only one, ever. Two
//!   sessions can both believe they drive — each settles the start of its link
//!   by comparing ids, knowing nothing of the other — and sending input to
//!   both would move two cursors with one hand.
//! - **The hotkey** goes to the peer being driven, which is how the cursor is
//!   taken back; with none, to the peer the cursor last went to, and failing
//!   that to the first one linked.
//! - **Panic** goes to every session: it promises everything comes home.
//!
//! What stays per session is the two-machine ownership exchange
//! ([`crate::session::CursorRole`]); the hub adds no wire vocabulary. What
//! becomes machine-wide is what was always machine-wide and was being written
//! by each session in turn: whether local input is grabbed, and which edges are
//! armed. [`MachineInput`] is that answer, recomputed on every change so no
//! session's report can overwrite another's.
//!
//! A `Mutex` rather than the `Rc<RefCell>` the sessions share, because the
//! capture forwarders that route through it are ordinary tokio tasks on the
//! multi-threaded runtime. It guards a short table and is held for a lookup
//! and a channel send, never across an await.

use std::sync::{Arc, Mutex};

use cursio_protocol::{Edge, EdgeSet, PeerId};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::session::{CapturedInput, OwnershipCmd};

/// The two channels a session reads its local input and ownership requests
/// from — what it used to borrow from capture directly.
pub struct LinkPorts {
    pub events: UnboundedReceiver<CapturedInput>,
    pub commands: UnboundedReceiver<OwnershipCmd>,
    /// Names this attachment in every later call.
    pub token: LinkToken,
}

/// One attachment of one peer.
///
/// A peer, not a link, is what the table is keyed by, but a reconnect can
/// attach the same peer before the old session has finished tearing down. The
/// generation is what stops that old session's late detach — or a late report
/// of its ownership — from taking the new one away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkToken {
    pub peer: PeerId,
    generation: u64,
}

/// Where one link sits relative to this machine, as far as the cursor cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Which of this machine's edges borders the peer, if any.
    pub edge: Option<Edge>,
    /// Grid distance to the peer, to pick between several beyond one edge.
    pub distance: u32,
    /// Whether the peer has a screen the cursor can enter at all. A phone used
    /// as a touchpad borders nothing, whatever cell it was placed in.
    pub takes_cursor: bool,
}

/// What this machine's own input should be doing, across all its links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineInput {
    /// Some session is streaming this machine's input to its peer, so local
    /// input is grabbed and the pointer hidden.
    pub driving: bool,
    /// Any peer is linked at all. Without one, capture rests idle with no
    /// barriers rather than holding a cursor nobody can be handed.
    pub linked: bool,
    /// The edges to arm.
    pub edges: EdgeSet,
}

struct Link {
    token: LinkToken,
    placement: Placement,
    events: UnboundedSender<CapturedInput>,
    commands: UnboundedSender<OwnershipCmd>,
}

#[derive(Default)]
struct Routes {
    /// In the order the links came up, which is the hotkey's last resort.
    links: Vec<Link>,
    /// Links whose session reported it is driving, oldest first. The last is
    /// where input goes; the rest are the degenerate case described in the
    /// module notes, kept so that one standing down hands input to the next
    /// rather than to nobody.
    driving: Vec<LinkToken>,
    next_generation: u64,
    /// The peer the cursor was last sent to, for the hotkey.
    last: Option<PeerId>,
    /// Set by panic: every edge is down until a link changes, because panic
    /// promises the barriers are gone and a mouse still moving toward an edge
    /// would otherwise hand control straight back out.
    disarmed: bool,
}

/// The routing table capture and the sessions share. Cheap to clone.
#[derive(Clone, Default)]
pub struct InputHub {
    routes: Arc<Mutex<Routes>>,
}

impl InputHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn routes(&self) -> std::sync::MutexGuard<'_, Routes> {
        // A panic while holding this lock left a table that is still a valid
        // table: every operation on it is a single push, retain or lookup.
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A link came up. Its session reads from the returned ports.
    ///
    /// A peer already present — a reconnect racing the old session's teardown
    /// — is replaced, so input never goes to a session that is on its way out.
    pub fn attach(&self, peer: PeerId, placement: Placement) -> (LinkPorts, MachineInput) {
        let (events_tx, events) = unbounded_channel();
        let (commands_tx, commands) = unbounded_channel();
        let mut routes = self.routes();
        routes.next_generation += 1;
        let token = LinkToken {
            peer,
            generation: routes.next_generation,
        };
        routes.links.retain(|l| l.token.peer != peer);
        routes.driving.retain(|t| t.peer != peer);
        routes.links.push(Link {
            token,
            placement,
            events: events_tx,
            commands: commands_tx,
        });
        routes.disarmed = false;
        let ports = LinkPorts {
            events,
            commands,
            token,
        };
        (ports, routes.machine())
    }

    /// A link went away. Its ports are closed by dropping their senders.
    pub fn detach(&self, token: LinkToken) -> MachineInput {
        let mut routes = self.routes();
        routes.links.retain(|l| l.token != token);
        routes.driving.retain(|t| *t != token);
        routes.disarmed = false;
        routes.machine()
    }

    /// A link's peer moved, or this machine did.
    pub fn place(&self, token: LinkToken, placement: Placement) -> MachineInput {
        let mut routes = self.routes();
        if let Some(link) = routes.links.iter_mut().find(|l| l.token == token) {
            link.placement = placement;
        }
        routes.disarmed = false;
        routes.machine()
    }

    /// A session's ownership changed: `driving` is whether it now streams
    /// this machine's input to its peer.
    pub fn note_driving(&self, token: LinkToken, driving: bool) -> MachineInput {
        let mut routes = self.routes();
        routes.driving.retain(|t| *t != token);
        if driving && routes.links.iter().any(|l| l.token == token) {
            routes.driving.push(token);
            routes.last = Some(token.peer);
        }
        routes.machine()
    }

    /// Panic: every session is told, and every edge comes down.
    pub fn panic(&self) -> MachineInput {
        let mut routes = self.routes();
        routes.disarmed = true;
        for link in &routes.links {
            let _ = link.commands.send(OwnershipCmd::Panic);
        }
        routes.machine()
    }

    /// Every edge comes down without telling anyone: a session carrying out a
    /// panic it was sent. Asking [`Self::panic`] there would send it again to
    /// every session, each of which would answer the same way.
    pub fn disarm(&self) -> MachineInput {
        let mut routes = self.routes();
        routes.disarmed = true;
        routes.machine()
    }

    /// What this machine's input should be doing now.
    pub fn machine(&self) -> MachineInput {
        self.routes().machine()
    }

    /// Pass one captured event to the session driving a peer, if any. With
    /// none it acts locally, which capture has already let it do.
    pub fn route_event(&self, event: CapturedInput) {
        let routes = self.routes();
        if let Some(link) = routes.driven() {
            let _ = link.events.send(event);
        }
    }

    /// Pass an ownership request to the one session it concerns.
    pub fn route_command(&self, cmd: OwnershipCmd) {
        let routes = self.routes();
        let target = match cmd {
            OwnershipCmd::HandoffToPeer { edge, .. } => routes.beyond(edge),
            OwnershipCmd::Toggle => routes
                .driven()
                .or_else(|| routes.last.and_then(|p| routes.link(p)))
                .or_else(|| routes.links.iter().find(|l| l.placement.takes_cursor)),
            OwnershipCmd::Panic => {
                for link in &routes.links {
                    let _ = link.commands.send(cmd);
                }
                return;
            }
        };
        match target {
            Some(link) => {
                let _ = link.commands.send(cmd);
            }
            None => tracing::trace!(?cmd, "no peer for this ownership request"),
        }
    }
}

impl Routes {
    fn link(&self, peer: PeerId) -> Option<&Link> {
        self.links.iter().find(|l| l.token.peer == peer)
    }

    fn driven(&self) -> Option<&Link> {
        let token = self.driving.last()?;
        self.links.iter().find(|l| l.token == *token)
    }

    /// The nearest peer beyond `edge` that can take the cursor.
    fn beyond(&self, edge: Edge) -> Option<&Link> {
        self.links
            .iter()
            .filter(|l| l.placement.takes_cursor && l.placement.edge == Some(edge))
            .min_by_key(|l| l.placement.distance)
    }

    fn machine(&self) -> MachineInput {
        let edges = if self.disarmed {
            EdgeSet::EMPTY
        } else {
            self.links
                .iter()
                .filter(|l| l.placement.takes_cursor)
                .filter_map(|l| l.placement.edge)
                .collect()
        };
        MachineInput {
            driving: self.driven().is_some(),
            linked: !self.links.is_empty(),
            edges,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(byte: u8) -> PeerId {
        PeerId::from_bytes([byte; 16])
    }

    fn at(edge: Edge, distance: u32) -> Placement {
        Placement {
            edge: Some(edge),
            distance,
            takes_cursor: true,
        }
    }

    fn motion() -> CapturedInput {
        CapturedInput::from(cursio_protocol::InputEvent::PointerMotion { dx: 1, dy: 0 })
    }

    fn crossing(edge: Edge) -> OwnershipCmd {
        OwnershipCmd::HandoffToPeer {
            edge,
            pos_norm: 0.5,
        }
    }

    /// The layout that exposed the one-edge design: a machine to the right,
    /// another below. Each direction reaches its own peer, and both edges are
    /// armed at once.
    #[test]
    fn each_edge_crossing_reaches_the_peer_beyond_that_edge() {
        let hub = InputHub::new();
        let (mut right, _) = hub.attach(peer(1), at(Edge::Right, 1));
        let (mut below, machine) = hub.attach(peer(2), at(Edge::Bottom, 1));
        assert!(machine.edges.contains(Edge::Right) && machine.edges.contains(Edge::Bottom));

        hub.route_command(crossing(Edge::Bottom));
        assert!(matches!(
            below.commands.try_recv(),
            Ok(OwnershipCmd::HandoffToPeer {
                edge: Edge::Bottom,
                ..
            })
        ));
        assert!(right.commands.try_recv().is_err());

        hub.route_command(crossing(Edge::Right));
        assert!(matches!(
            right.commands.try_recv(),
            Ok(OwnershipCmd::HandoffToPeer {
                edge: Edge::Right,
                ..
            })
        ));
        assert!(below.commands.try_recv().is_err());
    }

    #[test]
    fn a_crossing_with_nobody_beyond_it_goes_nowhere() {
        let hub = InputHub::new();
        let (mut right, _) = hub.attach(peer(1), at(Edge::Right, 1));
        hub.route_command(crossing(Edge::Left));
        assert!(right.commands.try_recv().is_err());
    }

    /// `0,0 — 1,0 — 2,0`, seen from the first: the cursor reaches the middle
    /// machine, not the far one.
    #[test]
    fn the_nearest_peer_beyond_an_edge_takes_the_crossing() {
        let hub = InputHub::new();
        let (mut far, _) = hub.attach(peer(1), at(Edge::Right, 2));
        let (mut near, _) = hub.attach(peer(2), at(Edge::Right, 1));
        hub.route_command(crossing(Edge::Right));
        assert!(near.commands.try_recv().is_ok());
        assert!(far.commands.try_recv().is_err());
    }

    /// Two sessions that each settled their link's start by comparing ids can
    /// both believe they drive. One hand must still move one cursor.
    #[test]
    fn captured_input_reaches_only_the_peer_being_driven() {
        let hub = InputHub::new();
        let (mut first, _) = hub.attach(peer(1), at(Edge::Right, 1));
        let (mut second, _) = hub.attach(peer(2), at(Edge::Bottom, 1));

        hub.route_event(motion());
        assert!(first.events.try_recv().is_err() && second.events.try_recv().is_err());

        hub.note_driving(first.token, true);
        hub.note_driving(second.token, true);
        hub.route_event(motion());
        assert!(second.events.try_recv().is_ok());
        assert!(first.events.try_recv().is_err());

        // The later one standing down hands input to the one still driving
        // rather than to nobody.
        let machine = hub.note_driving(second.token, false);
        assert!(machine.driving);
        hub.route_event(motion());
        assert!(first.events.try_recv().is_ok());
    }

    #[test]
    fn local_input_is_grabbed_only_while_some_peer_is_driven() {
        let hub = InputHub::new();
        let (only, machine) = hub.attach(peer(1), at(Edge::Right, 1));
        assert!(machine.linked && !machine.driving);
        assert!(hub.note_driving(only.token, true).driving);
        assert!(!hub.note_driving(only.token, false).driving);
        assert!(!hub.detach(only.token).linked);
    }

    /// The hotkey takes the cursor back from wherever it is being driven;
    /// with nothing driven it sends it where it last went.
    #[test]
    fn the_hotkey_goes_to_the_driven_peer_then_to_the_last_one() {
        let hub = InputHub::new();
        let (mut first, _) = hub.attach(peer(1), at(Edge::Right, 1));
        let (mut second, _) = hub.attach(peer(2), at(Edge::Bottom, 1));

        hub.route_command(OwnershipCmd::Toggle);
        assert!(matches!(
            first.commands.try_recv(),
            Ok(OwnershipCmd::Toggle)
        ));

        hub.note_driving(second.token, true);
        hub.route_command(OwnershipCmd::Toggle);
        assert!(matches!(
            second.commands.try_recv(),
            Ok(OwnershipCmd::Toggle)
        ));

        hub.note_driving(second.token, false);
        hub.route_command(OwnershipCmd::Toggle);
        assert!(matches!(
            second.commands.try_recv(),
            Ok(OwnershipCmd::Toggle)
        ));
        assert!(first.commands.try_recv().is_err());
    }

    #[test]
    fn panic_reaches_every_session_and_takes_every_edge_down() {
        let hub = InputHub::new();
        let (mut first, _) = hub.attach(peer(1), at(Edge::Right, 1));
        let (mut second, _) = hub.attach(peer(2), at(Edge::Bottom, 1));
        let machine = hub.panic();
        assert!(machine.edges.is_empty());
        assert!(matches!(first.commands.try_recv(), Ok(OwnershipCmd::Panic)));
        assert!(matches!(
            second.commands.try_recv(),
            Ok(OwnershipCmd::Panic)
        ));
        // The next change to the links puts them back.
        assert!(!hub.place(first.token, at(Edge::Right, 1)).edges.is_empty());
    }

    /// A phone used as a touchpad has no screen to enter: its cell arms no
    /// barrier, which on Wayland would grab input with nobody to hand it to.
    #[test]
    fn a_peer_without_a_screen_arms_no_edge() {
        let hub = InputHub::new();
        let (mut phone, machine) = hub.attach(
            peer(1),
            Placement {
                edge: Some(Edge::Left),
                distance: 1,
                takes_cursor: false,
            },
        );
        assert!(machine.edges.is_empty());
        hub.route_command(crossing(Edge::Left));
        assert!(phone.commands.try_recv().is_err());
    }

    #[test]
    fn a_departed_link_stops_receiving_and_unarms_its_edge() {
        let hub = InputHub::new();
        let (_right, _) = hub.attach(peer(1), at(Edge::Right, 1));
        let (mut below, _) = hub.attach(peer(2), at(Edge::Bottom, 1));
        hub.note_driving(below.token, true);
        let machine = hub.detach(below.token);
        assert!(!machine.driving);
        assert!(!machine.edges.contains(Edge::Bottom));
        hub.route_event(motion());
        assert!(below.events.try_recv().is_err());
    }

    /// A reconnect can attach the same peer before the old session has
    /// detached; the new session must be the one that hears from capture, and
    /// the old one's late detach must not take the new one away.
    #[test]
    fn a_reattached_peer_replaces_the_old_session() {
        let hub = InputHub::new();
        let (mut old, _) = hub.attach(peer(1), at(Edge::Right, 1));
        let (mut new, _) = hub.attach(peer(1), at(Edge::Right, 1));
        hub.route_command(crossing(Edge::Right));
        assert!(new.commands.try_recv().is_ok());
        assert!(old.commands.try_recv().is_err());

        hub.note_driving(old.token, true);
        let machine = hub.detach(old.token);
        assert!(!machine.driving && machine.edges.contains(Edge::Right));
        hub.route_command(crossing(Edge::Right));
        assert!(new.commands.try_recv().is_ok());
    }
}
