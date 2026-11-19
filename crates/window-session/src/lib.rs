//! Screens across the mesh: window transfer, a phone's screen shown and
//! driven from a computer, a peer's screen offered to capture software here,
//! and the AirPlay window carried to another machine.
//!
//! Policy over `cursio-window`'s mechanism (and `cursio-airplay`'s), split
//! out of `cursio-core`, which re-exports every module under its old path.
//! The features are the core's of the same names. What is built without
//! `window` is only what the control socket names on every build — a
//! phone's actions and gestures, and the tray's list of views — so a phone
//! carries no capture code. See `docs/window-transfer-design.md`.

/// The mirrored iPhone screen, offered to the mesh as a transferable window.
#[cfg(feature = "airplay")]
pub mod airplay_window;
/// A tap, a swipe, a key or text for a phone's screen, as its window input.
pub mod phone_action;
/// A mouse on a computer turned into a finger on a phone's screen.
pub mod phone_gesture;
pub mod phone_hands;
/// A phone's screen shown on a computer as a window it can be driven from.
#[cfg(feature = "window")]
pub mod phone_screen;
/// Installing Cursio as this user's ScreenCast portal backend.
#[cfg(all(target_os = "linux", feature = "window"))]
pub mod portal_setup;
/// Remote-screen views: an app or monitor of a peer, offered to
/// screen-sharing software here without being drawn on this desktop.
#[cfg(feature = "window")]
pub mod remote_screen;
/// Where a capture app's request for a peer's screen meets that peer's link.
#[cfg(feature = "window")]
pub mod screen_hub;
pub mod screen_views;
#[cfg(feature = "window")]
mod video_budget;
#[cfg(feature = "window")]
pub mod window_session;

#[cfg(feature = "phone-control")]
use cursio_core_base::data_dir;
#[cfg(all(test, feature = "window"))]
use cursio_core_base::test_support;
#[cfg(feature = "window")]
use cursio_core_base::{health, live, module};
#[cfg(feature = "window")]
use cursio_hubs::notify_hub;
#[cfg(feature = "airplay")]
use cursio_service_session as service_session;
