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
#[cfg(feature = "audio")]
pub mod audio_gate;
/// Sound made on this machine — a call's voice, a phone's music — sent to a
/// peer as one of its application streams.
#[cfg(feature = "audio")]
pub mod audio_relay;
#[cfg(feature = "audio")]
pub mod audio_session;
/// Handing a paired phone's headset and speaker roles from PipeWire to Cursio.
/// The mesh's channel over Bluetooth, for a peer the network does not reach.
#[cfg(feature = "bluetooth")]
pub mod bluetooth_link;
#[cfg(all(target_os = "linux", feature = "calls"))]
pub mod bluetooth_setup;
/// A phone's call voice, played and recorded on this computer's audio.
#[cfg(feature = "calls")]
pub mod call_audio;
/// A phone's calls as notifications here, with buttons that act on them.
pub mod calls_display;
pub mod clipboard_session;
/// The local socket the app, the tray and `cursio status` talk to.
pub mod control;
/// The engine a request-and-answer module (messages, the gallery) runs on a link.
pub mod crash;
/// This machine's current problems, each with the fix where there is one.
pub mod desk;
pub mod desktop_window;
pub mod diag_session;
pub mod direct_session;
pub mod exchange_session;
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
/// Where a phone's messages are answered and a desktop's requests leave.
pub mod messages_hub;
/// The messages module on one link.
pub mod messages_session;
pub mod metrics;
pub mod mixer;
/// The unelevated process that keeps a peer's drives where Explorer sees them.
#[cfg(feature = "files")]
pub mod mount_helper;
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
/// Where a request-and-answer module meets the rest of the node.
pub mod request_hub;
/// Where a capture app's request for a peer's screen meets that peer's link.
#[cfg(feature = "window")]
pub mod screen_hub;
pub mod screen_views;
pub mod service_session;
pub mod session;
mod session_outbox;
pub mod share_session;
pub mod share_sink;
/// Versions side by side, a `current` pointer, and the helper that switches it.
pub mod update;
#[cfg(feature = "usb")]
pub mod usb_node;
#[cfg(feature = "usb")]
pub mod usb_session;
#[cfg(feature = "window")]
mod video_budget;
/// Waking a paired machine that is asleep, by the address learned while awake.
pub mod wake;
#[cfg(target_os = "linux")]
pub mod wifi_setup;
#[cfg(feature = "window")]
pub mod window_session;

#[cfg(target_os = "linux")]
pub use cursio_core_base::session_bus;
pub use cursio_hubs::{
    calls_hub, calls_session, media_hub, media_session, notify_dedup, notify_hub, notify_session,
    notify_toast, presence,
};
#[cfg(target_os = "linux")]
pub use cursio_hubs::{media_mpris, notify_display};
#[cfg(target_os = "windows")]
pub use cursio_hubs::media_smtc;
#[cfg(test)]
use cursio_core_base::test_support;
pub use cursio_core_base::{
    at_phone, config, data_dir, diagnostics, elevate, health, identity, live, module, module_host,
    modules_busy, node_status, queue, NodeIdentity,
};
