//! Where a phone's calls, notifications and players meet this machine.
//!
//! Each hub is process-wide: the link that carries a phone's module writes to
//! it, and the control socket, the desktop's notification server, MPRIS or
//! the Windows media controls read from it. The calls, notify and media
//! modules that feed them live here too, since each is the hub's other half
//! on one link; [`presence`] is what all three ask before choosing which
//! machine rings, shows or plays.
//!
//! Split out of `cursio-core`, which re-exports every module under its old
//! path. Nothing here knows about links, the mesh or the control socket.

/// Where a phone's calls meet this machine, over the mesh or Bluetooth.
pub mod calls_hub;
/// The calls module on one link.
pub mod calls_session;
/// Where this machine's players meet the mesh and peers' are shown.
pub mod media_hub;
/// This desktop's MPRIS players, and peers' players offered as MPRIS ones.
#[cfg(target_os = "linux")]
pub mod media_mpris;
/// The media module on one link.
pub mod media_session;
/// This Windows machine's players, from the system media controls.
#[cfg(target_os = "windows")]
pub mod media_smtc;
/// Not showing a phone's copy of what a desktop client already showed.
pub mod notify_dedup;
/// A phone's notifications shown through this desktop's notification server.
#[cfg(target_os = "linux")]
pub mod notify_display;
/// Where a phone's notifications enter the mesh and a desktop's answers leave.
pub mod notify_hub;
/// The notification module on one link.
pub mod notify_session;
/// A phone's notifications shown as Windows toasts.
pub mod notify_toast;
/// Whether the user is at this machine: where a phone's notification goes.
pub mod presence;

#[cfg(target_os = "linux")]
use cursio_core_base::session_bus;
#[cfg(test)]
use cursio_core_base::test_support;
use cursio_core_base::{at_phone, config, module, quiet};
