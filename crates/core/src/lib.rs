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
pub mod apps;
/// Handing a paired phone's headset and speaker roles from PipeWire to Cursio.
/// The mesh's channel over Bluetooth, for a peer the network does not reach.
#[cfg(feature = "bluetooth")]
pub mod bluetooth_link;
/// A phone's calls as notifications here, with buttons that act on them.
pub mod calls_display;
/// The local socket the app, the tray and `cursio status` talk to.
pub mod control;
/// The engine a request-and-answer module (messages, the gallery) runs on a link.
pub mod crash;
/// This machine's current problems, each with the fix where there is one.
pub mod desk;
pub mod desktop_window;
pub mod diag_session;
pub mod direct_session;
pub mod firewall;
/// Removing a machine from the mesh: the trust store, builder trust, and what
/// the running node holds about it.
pub mod forget;
/// Which peer this machine's one mouse and keyboard reach, across all links.
pub mod input_hub;
/// Bringing a link up to the point a session can start: handshake, pairing,
/// encryption, module negotiation. Shared by the CLI and the app, desktop and
/// mobile alike.
pub mod link;
pub mod log_report;
pub mod log_tail;
pub mod logging;
/// `cursio mcp`: the mesh as tools for an AI agent.
pub mod mcp;
pub mod mesh;
pub mod metrics;
/// The system telling the sessions their network went away.
pub mod network_watch;
#[cfg(target_os = "linux")]
pub mod node_tray;
pub mod opener;
/// Who may pair right now, with what secret, and for how long.
pub mod pairing_window;
/// A tap, a swipe, a key or text for a phone's screen, as its window input.
pub mod phone_action;
/// Raising memory, swap and i/o pressure as a problem while it lasts.
/// A mouse on a computer turned into a finger on a phone's screen.
pub mod phone_gesture;
pub mod phone_hands;
/// A phone's screen shown on a computer as a window it can be driven from.
#[cfg(feature = "window")]
pub mod phone_screen;
/// Installing Cursio as this user's ScreenCast portal backend.
#[cfg(all(target_os = "linux", feature = "window"))]
pub mod portal_setup;
pub mod pressure;
pub mod probe_history;
pub mod probe_session;
pub mod proc_memory;
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
pub mod session;
mod session_outbox;
pub mod share_session;
pub mod share_sink;
/// Versions side by side, a `current` pointer, and the helper that switches it.
pub mod update;
#[cfg(feature = "window")]
mod video_budget;
/// Waking a paired machine that is asleep, by the address learned while awake.
pub mod wake;
#[cfg(target_os = "linux")]
pub mod wifi_setup;
#[cfg(feature = "window")]
pub mod window_session;

#[cfg(all(target_os = "linux", feature = "calls"))]
pub use cursio_audio_session::bluetooth_setup;
#[cfg(feature = "calls")]
pub use cursio_audio_session::call_audio;
#[cfg(feature = "audio")]
pub use cursio_audio_session::{audio_gate, audio_relay, audio_session};
pub use cursio_clipboard_session::clipboard_session;
#[cfg(target_os = "linux")]
pub use cursio_core_base::session_bus;
#[cfg(test)]
use cursio_core_base::test_support;
pub use cursio_core_base::{
    at_phone, config, data_dir, diagnostics, elevate, health, identity, live, mixer, module,
    module_host, modules_busy, node_status, queue, NodeIdentity,
};
#[cfg(feature = "gallery")]
pub use cursio_exchange::gallery_folder;
pub use cursio_exchange::{
    exchange_session, gallery_hub, gallery_session, messages_hub, messages_session, request_hub,
};
#[cfg(feature = "files")]
pub use cursio_files_session::{files_session, mount_helper};
#[cfg(target_os = "windows")]
pub use cursio_hubs::media_smtc;
pub use cursio_hubs::{
    calls_hub, calls_session, media_hub, media_session, notify_dedup, notify_hub, notify_session,
    notify_toast, presence,
};
#[cfg(target_os = "linux")]
pub use cursio_hubs::{media_mpris, notify_display};
pub use cursio_service_session as service_session;
#[cfg(feature = "usb")]
pub use cursio_usb_session::{usb_node, usb_session};
