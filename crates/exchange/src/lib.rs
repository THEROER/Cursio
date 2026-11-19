//! The request-and-answer modules: messages, the gallery and diagnostics'
//! hub, and the engine and hubs they share.
//!
//! What these have in common is the shape, not the subject: a window or the
//! control socket asks a named peer through a [`request_hub::RequestHub`],
//! the module on that peer's link carries the question
//! ([`exchange_session`]), and the answer comes back to whoever waits.
//! Nothing is pushed; see `docs/messages-design.md` and
//! `docs/gallery-design.md`.
//!
//! Split out of `cursio-core`, which re-exports every module under its old
//! path. The diagnostics module itself stays in the core, since what it
//! answers with is the rest of the node; only its hub is here.

/// The diagnostics module's hub, and the sessions it reports the paths of.
pub mod diag_hub;
/// The engine a request-and-answer module (messages, the gallery) runs on a link.
pub mod exchange_session;
/// This computer's Pictures and Videos, answered to peers browsing them.
#[cfg(feature = "gallery")]
pub mod gallery_folder;
/// Where this machine's gallery is answered and a window's requests leave.
pub mod gallery_hub;
/// The gallery module on one link.
pub mod gallery_session;
/// Where a phone's messages are answered and a desktop's requests leave.
pub mod messages_hub;
/// The messages module on one link.
pub mod messages_session;
/// Where a request-and-answer module meets the rest of the node.
pub mod request_hub;

use cursio_core_base::module;
#[cfg(test)]
use cursio_core_base::test_support;
