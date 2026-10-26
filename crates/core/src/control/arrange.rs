//! Arranging the shared desk: this machine's own place and seams, written to
//! its config like any setting, and a peer's, asked of that peer.
//!
//! Each machine is the only writer of its own place, so two apps dragging
//! the same machine at once settle on whichever request it heard last, and
//! every peer learns the result from that machine's own announcement rather
//! than from whoever asked.

use cursio_protocol::{Arrange, Barrier, PeerId, Place, ScreenOffset};

use crate::control::handle::error;
use crate::control::{NodeState, Reply};

/// Carries an arrangement of a peer to the running mesh, which owns the
/// links. See [`NodeState::set_on_arrange`].
pub type Arranger = std::sync::Arc<dyn Fn(PeerId, Arrange) + Send + Sync>;

impl NodeState {
    /// Where [`NodeState::arrange`] reaches a peer: its link, which only the
    /// mesh holds.
    pub fn set_on_arrange(&self, arranger: Arranger) {
        *self
            .on_arrange
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(arranger);
    }

    /// Stand `machine` (this one when `None`) at `place` on the desk.
    pub fn arrange(&self, machine: Option<PeerId>, place: Place) -> Reply {
        match machine {
            Some(peer) if peer != self.node_id() => self.ask(peer, Arrange::Place(place)),
            _ => self.edit_file(&["layout"], |c| c.layout.place = Some(place)),
        }
    }

    /// Stand one screen of `machine` (this one when `None`) `offset` from
    /// where its pixels put it.
    pub fn arrange_screen(&self, machine: Option<PeerId>, offset: ScreenOffset) -> Reply {
        match machine {
            Some(peer) if peer != self.node_id() => self.ask(peer, Arrange::Screen(offset)),
            _ => self.edit_file(&["layout"], |c| set_offset(&mut c.layout.offsets, offset)),
        }
    }

    /// Close (or open) the seam between this machine's `screen` and `peer`'s
    /// `peer_screen`, on both machines: either side's barrier closes it, so
    /// opening it needs both gone.
    pub fn set_barrier(
        &self,
        screen: String,
        peer: PeerId,
        peer_screen: String,
        closed: bool,
    ) -> Reply {
        let mine = Barrier {
            screen: screen.clone(),
            peer,
            peer_screen: peer_screen.clone(),
        };
        let reply = self.edit_file(&["layout"], |c| {
            toggle(&mut c.layout.barriers, mine, closed)
        });
        if matches!(reply, Reply::Error { .. }) {
            return reply;
        }
        // The peer keeps its own copy, so a seam closed while it was away
        // still holds when it next greets anyone.
        let asked = self.ask(
            peer,
            Arrange::Barrier {
                screen: peer_screen,
                peer_screen: screen,
                closed,
            },
        );
        match asked {
            Reply::Error { .. } => asked,
            _ => reply,
        }
    }

    /// `peer` arranged this machine from its app.
    pub fn arranged_by(&self, peer: PeerId, request: Arrange) -> Reply {
        match request {
            Arrange::Place(place) => self.edit_file(&["layout"], |c| c.layout.place = Some(place)),
            Arrange::Barrier {
                screen,
                peer_screen,
                closed,
            } => self.edit_file(&["layout"], |c| {
                toggle(
                    &mut c.layout.barriers,
                    Barrier {
                        screen,
                        peer,
                        peer_screen,
                    },
                    closed,
                )
            }),
            Arrange::Screen(offset) => {
                self.edit_file(&["layout"], |c| set_offset(&mut c.layout.offsets, offset))
            }
        }
    }

    fn ask(&self, peer: PeerId, request: Arrange) -> Reply {
        let arranger = self
            .on_arrange
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let linked = self.inner().links.contains_key(&peer);
        match arranger {
            Some(arranger) if linked => {
                arranger(peer, request);
                Reply::Done
            }
            _ => error("that machine is not connected; it can be arranged once it is"),
        }
    }
}

/// Replace `offset.screen`'s offset; a zero one is dropped, so a screen put
/// back where its pixels say leaves nothing in the file.
fn set_offset(offsets: &mut Vec<ScreenOffset>, offset: ScreenOffset) {
    offsets.retain(|o| o.screen != offset.screen);
    if offset.dx_mm != 0 || offset.dy_mm != 0 {
        offsets.push(offset);
    }
}

fn toggle(barriers: &mut Vec<Barrier>, barrier: Barrier, closed: bool) {
    barriers.retain(|b| *b != barrier);
    if closed {
        barriers.push(barrier);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn barrier() -> Barrier {
        Barrier {
            screen: "PHL".into(),
            peer: PeerId::from_u128(2),
            peer_screen: "BOE".into(),
        }
    }

    #[test]
    fn a_screen_moved_back_where_its_pixels_put_it_leaves_no_offset() {
        let offset = |dy_mm| ScreenOffset {
            screen: "LG".into(),
            dx_mm: 0,
            dy_mm,
        };
        let mut offsets = Vec::new();
        set_offset(&mut offsets, offset(26));
        set_offset(&mut offsets, offset(30));
        assert_eq!(offsets, vec![offset(30)]);
        set_offset(&mut offsets, offset(0));
        assert!(offsets.is_empty());
    }

    #[test]
    fn closing_a_seam_twice_keeps_one_barrier_and_opening_removes_it() {
        let mut barriers = Vec::new();
        toggle(&mut barriers, barrier(), true);
        toggle(&mut barriers, barrier(), true);
        assert_eq!(barriers, vec![barrier()]);
        toggle(&mut barriers, barrier(), false);
        assert!(barriers.is_empty());
    }
}
