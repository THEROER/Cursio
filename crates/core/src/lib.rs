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
//! [`module`], [`node_status`], [`diagnostics`] — depends on no platform crate at all. The
//! `*_session.rs` files are the policy layer on top, and each is gated by the
//! feature naming its subsystem (all on by default). A phone builds the core
//! plus the modules it can implement and never compiles PipeWire, a FUSE mount
//! or an AirPlay receiver, none of which exist there.

#[cfg(feature = "airplay")]
pub mod airplay_window;
pub mod at_phone;
#[cfg(feature = "audio")]
pub mod audio_gate;
/// Sound made on this machine — a call's voice, a phone's music — sent to a
/// peer as one of its application streams.
#[cfg(feature = "audio")]
pub mod audio_relay;
#[cfg(feature = "audio")]
pub mod audio_session;
/// Handing a paired phone's headset and speaker roles from PipeWire to Cursio.
/// The mesh's channel over Bluetooth, for a paired phone off the network.
#[cfg(all(feature = "bluetooth", unix))]
pub mod bluetooth_link;
#[cfg(all(target_os = "linux", feature = "calls"))]
pub mod bluetooth_setup;
/// A phone's call voice, played and recorded on this computer's audio.
#[cfg(feature = "calls")]
pub mod call_audio;
/// A phone's calls as notifications here, with buttons that act on them.
pub mod calls_display;
/// Where a phone's calls meet this machine, over the mesh or Bluetooth.
pub mod calls_hub;
/// The calls module on one link.
pub mod calls_session;
pub mod clipboard_session;
/// What `cursio mesh` runs, read from `config.toml` and overridden by flags.
pub mod config;
/// The local socket the app, the tray and `cursio status` talk to.
pub mod control;
/// This machine's current problems, each with the fix where there is one.
pub mod diagnostics;
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
/// Removing a machine from the mesh: the trust store, builder trust, and what
/// the running node holds about it.
pub mod forget;
/// This computer's Pictures and Videos, answered to peers browsing them.
#[cfg(feature = "gallery")]
pub mod gallery_folder;
/// Where this machine's gallery is answered and a window's requests leave.
pub mod gallery_hub;
/// The gallery module on one link.
pub mod gallery_session;
/// Link, module and platform problems, each raised and cleared by name.
pub mod health;
pub mod identity;
/// Which peer this machine's one mouse and keyboard reach, across all links.
pub mod input_hub;
/// Bringing a link up to the point a session can start: handshake, pairing,
/// encryption, module negotiation. Shared by the CLI and the app, desktop and
/// mobile alike.
pub mod link;
/// Applying a changed config to running links instead of restarting.
pub mod live;
pub mod logging;
/// Where this machine's players meet the mesh and peers' are shown.
pub mod media_hub;
/// This desktop's MPRIS players, and peers' players offered as MPRIS ones.
#[cfg(target_os = "linux")]
pub mod media_mpris;
/// The media module on one link.
pub mod media_session;
pub mod mesh;
/// Where a phone's messages are answered and a desktop's requests leave.
pub mod messages_hub;
/// The messages module on one link.
pub mod messages_session;
pub mod metrics;
pub mod mixer;
pub mod module;
/// Running each session module on a thread of its own, off the loop the
/// shared cursor rides.
pub(crate) mod module_host;
pub use module_host::modules_busy;
/// The unelevated process that keeps a peer's drives where Explorer sees them.
pub mod mount_helper;
pub mod node_status;
/// A phone's notifications shown through this desktop's notification server.
/// Not showing a phone's copy of what a desktop client already showed.
pub mod notify_dedup;
#[cfg(target_os = "linux")]
pub mod notify_display;
/// Where a phone's notifications enter the mesh and a desktop's answers leave.
pub mod notify_hub;
/// The notification module on one link.
pub mod notify_session;
/// A phone's notifications shown as Windows toasts.
pub mod notify_toast;
pub mod opener;
/// Who may pair right now, with what secret, and for how long.
pub mod pairing_window;
/// Raising memory, swap and i/o pressure as a problem while it lasts.
/// A mouse on a computer turned into a finger on a phone's screen.
pub mod phone_gesture;
/// A phone's screen shown on a computer as a window it can be driven from.
#[cfg(feature = "window")]
pub mod phone_screen;
/// Installing Cursio as this user's ScreenCast portal backend.
#[cfg(all(target_os = "linux", feature = "window"))]
pub mod portal_setup;
/// Whether the user is at this machine: where a phone's notification goes.
pub mod presence;
pub mod pressure;
/// Renegotiating a running link when either side's settings change.
pub mod reconfigure;
pub mod registry;
/// Remote-screen views: an app or monitor of a peer, offered to
/// screen-sharing software here without being drawn on this desktop.
#[cfg(feature = "window")]
pub mod remote_screen;
/// Where a capture app's request for a peer's screen meets that peer's link.
#[cfg(feature = "window")]
pub mod screen_hub;
pub mod screen_views;
pub mod service_session;
pub mod session;
/// The session bus, from a thread outside the runtime.
#[cfg(target_os = "linux")]
pub mod session_bus;
pub mod share_session;
pub mod share_sink;
/// Versions side by side, a `current` pointer, and the helper that switches it.
pub mod update;
#[cfg(feature = "usb")]
pub mod usb_node;
#[cfg(feature = "usb")]
pub mod usb_session;
#[cfg(feature = "window")]
pub mod window_session;

pub use identity::NodeIdentity;
