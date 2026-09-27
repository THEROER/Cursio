//! Cursio core: node identity and session orchestration.
//!
//! Ties the protocol and transport crates together into the two flows the CLI
//! exposes:
//! - a persistent [`identity::NodeIdentity`] (stable [`PeerId`] + display name),
//! - a [`session`] handshake + heartbeat loop over any [`Transport`].
//!
//! Capture, input proxying, and the Tauri UI build on top of this later; for
//! now this is the layer that proves two machines can find, pair, and hold a
//! live session.

//! ## What is always here, and what is a feature
//!
//! The mesh core — [`identity`], [`link`], [`session`], [`mesh`], [`registry`],
//! [`module`], [`node_status`] — depends on no platform crate at all. The
//! `*_session.rs` files are the policy layer on top, and each is gated by the
//! feature naming its subsystem (all on by default). A phone builds the core
//! plus the modules it can implement and never compiles PipeWire, a FUSE mount
//! or an AirPlay receiver, none of which exist there.

#[cfg(feature = "airplay")]
pub mod airplay_window;
#[cfg(feature = "audio")]
pub mod audio_session;
pub mod clipboard_session;
/// Windows self-elevation, so injected input isn't blocked by UIPI.
pub mod elevate;
/// File sharing. Serving this machine's volumes is portable and always built;
/// *mounting* the peer's needs FUSE on Linux or WinFsp on Windows (and the
/// latter only under MSVC, because `winfsp-sys` rejects every other target
/// outright), so on a platform with neither the module runs one-way and logs
/// why.
#[cfg(feature = "files")]
pub mod files_session;
pub mod firewall;
pub mod identity;
/// Bringing a link up to the point a session can start: handshake, pairing,
/// encryption, module negotiation. Shared by the CLI and the app, desktop and
/// mobile alike.
pub mod link;
pub mod logging;
pub mod mesh;
pub mod module;
pub mod node_status;
/// Installing Cursio as this user's ScreenCast portal backend.
#[cfg(all(target_os = "linux", feature = "window"))]
pub mod portal_setup;
pub mod registry;
/// Remote-screen views: an app or monitor of a peer, offered to
/// screen-sharing software here without being drawn on this desktop.
#[cfg(feature = "window")]
pub mod remote_screen;
/// Where a capture app's request for a peer's screen meets that peer's link.
#[cfg(feature = "window")]
pub mod screen_hub;
pub mod service_session;
pub mod session;
#[cfg(feature = "usb")]
pub mod usb_node;
#[cfg(feature = "usb")]
pub mod usb_session;
#[cfg(feature = "window")]
pub mod window_session;

pub use identity::NodeIdentity;
