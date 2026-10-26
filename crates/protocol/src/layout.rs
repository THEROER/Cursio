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
