//! What every part of a Cursio node stands on: who this machine is, what a
//! session module is and the host that runs one, the node's configuration
//! and its health, and the few process-wide registers the modules report to.
//!
//! Split out of `cursio-core` so a module can live in a crate of its own: a
//! module needs [`module::SessionModule`], the config it reads, the problems
//! it raises and the activity it reports, and none of the mesh, the links or
//! the control socket. `cursio-core` re-exports every module here under its
//! old path, so `cursio_core::config` and the like are what they always were.
//!
//! Like the rest of the node this is policy-free plumbing for the `!Send`
//! session loop: nothing here is spawned, and the module host runs each
//! module on its own OS thread exactly as it did inside the core.

pub mod at_phone;
pub mod config;
pub mod data_dir;
pub mod diagnostics;
/// Windows self-elevation, so injected input isn't blocked by UIPI.
pub mod elevate;
/// Link, module and platform problems, each raised and cleared by name.
pub mod health;
pub mod identity;
/// Applying a changed config to running links instead of restarting.
pub mod live;
/// The listener's own volume for each peer stream playing on this machine.
pub mod mixer;
pub mod module;
/// Running each session module on a thread of its own, off the loop the
/// shared cursor rides.
pub mod module_host;
pub mod node_status;
/// A named pipe only this user can open.
#[cfg(windows)]
pub mod owner_pipe;
mod peer_hello;
pub mod queue;
/// Whether now is a moment the node can be restarted without anyone noticing.
pub mod quiet;
/// The session bus, from a thread outside the runtime.
#[cfg(target_os = "linux")]
pub mod session_bus;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

mod error;

pub use error::IdentityError;
pub use identity::NodeIdentity;
pub use module_host::modules_busy;
pub use peer_hello::PeerHello;
