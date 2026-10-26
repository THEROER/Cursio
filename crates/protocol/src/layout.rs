//! Where the machines' screens stand relative to one another.
//!
//! A cell on a grid ([`crate::Position`]) says that one machine is right of
//! another; it cannot say that a laptop stands under the right-hand one of a
//! desktop's two monitors and not the left. With the bottom edge of the whole
//! desktop as the unit, pushing down on either monitor reached the laptop. So
//! each machine also says what its screens are, and where its desktop stands
//! in one layout every machine shares, measured in millimetres on the desk.
//!
//! Millimetres rather than pixels because the layout is the desk: a 27-inch
//! 4K panel and a 14-inch laptop at 1080p are told apart by their size, not
//! their resolution, and a pointer leaving one should come in at the spot
//! physically beside it. Each screen still carries its pixels, in the
//! coordinates its own machine's capture measures a crossing in, so a point on
//! the desk is turned into a point a backend can place the pointer at.

use serde::{Deserialize, Serialize};

use crate::PeerId;

/// What a machine says about its own part of the desk: its screens, where
/// it stands, and the seams it has closed. In the greeting, and again in
/// [`crate::Reconfigure`] whenever any of it changes — a monitor plugged in,
/// the person moving it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Layout {
    /// Empty when the desktop cannot say (a compositor without a layout
    /// interface, a phone): then the grid decides its edges, as before.
    pub screens: Vec<Screen>,
    /// `None` until someone arranges this machine on the desk; the grid
    /// decides until then, so a mesh that never opened the arrangement
    /// behaves exactly as it did.
    pub place: Option<Place>,
    pub barriers: Vec<Barrier>,
    /// Screens the person moved on the desk away from where the operating
    /// system's pixels put them: two monitors aligned at the top in the
    /// display settings may stand bottom-aligned on the desk.
    pub offsets: Vec<ScreenOffset>,
}

/// How far one screen stands from where its machine's pixels place it, in
/// millimetres. Only the desk moves: the pointer between a machine's own
/// screens still goes where its operating system sends it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScreenOffset {
    pub screen: String,
    pub dx_mm: i32,
    pub dy_mm: i32,
}

/// Where a machine's desktop stands: the top-left corner of its screens laid
/// out in millimetres, from the desk's origin. Each machine holds its own and
/// is the only one that writes it, so there is nothing to reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Place {
    pub x_mm: i32,
    pub y_mm: i32,
}

/// A seam the person closed: the cursor does not cross between this
/// machine's `screen` and `peer`'s `peer_screen`, though they touch on the
/// desk. Either side's barrier closes it both ways, so a machine that was
/// offline when it was closed still honours it once it hears the other's.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Barrier {
    pub screen: String,
    pub peer: PeerId,
    pub peer_screen: String,
}

/// A peer asking this machine to change its own part of the desk: the person
/// arranged it from the peer's app. The machine writes it into its own
/// settings and announces the result like any other change, so every peer
/// hears it from the machine it concerns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arrange {
    Place(Place),
    /// Close (or open) the seam between this machine's `screen` and the
    /// sender's `peer_screen`.
    Barrier {
        screen: String,
        peer_screen: String,
        closed: bool,
    },
    /// Stand one of this machine's screens this far from where its pixels
    /// put it.
    Screen(ScreenOffset),
}

/// One monitor of a machine, as that machine's own desktop places it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Screen {
    /// Stable across restarts and across moving the cable to another port:
    /// the panel's own identity from EDID (maker and serial) where it has
    /// one, else its connector.
    pub id: String,
    /// What the person calls it: the panel's model, else the connector.
    pub name: String,
    /// Where it sits on its machine's desktop, in the pixels that machine's
    /// capture reports and places the pointer in: logical pixels on Wayland
    /// and macOS, physical ones on Windows.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// The visible picture's size as the panel reports it, `None` when it
    /// cannot say: a projector or a television often reports nothing, or a
    /// size of zero. Then the layout estimates one from the pixels.
    pub size_mm: Option<(u32, u32)>,
}
