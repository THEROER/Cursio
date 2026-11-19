//! USB devices shared between machines: the module on one link, and the
//! node-wide owner of each device that every link's module talks to.
//!
//! Policy over `cursio-usb`'s mechanism, split out of `cursio-core` (which
//! re-exports both modules under their old paths, behind its `usb` feature).
//! See `docs/usb-design.md`.

/// One owner per shared device, whichever links want it.
pub mod usb_node;
/// The USB module on one link.
pub mod usb_session;

#[cfg(target_os = "windows")]
use cursio_core_base::elevate;
use cursio_core_base::{diagnostics, module, quiet};
