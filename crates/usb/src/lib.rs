//! Sharing a USB device with the rest of the mesh.
//!
//! Mechanism only: enumerating what is plugged in, and handing a device to the
//! kernel's USB/IP driver so a peer can attach it. *Which* peer may have it is
//! policy and lives in `cursio-core`'s `usb_session` — the same split the files
//! module uses.
//!
//! ## Nothing here speaks USB
//!
//! There is no libusb and no descriptor parsing beyond what sysfs has already
//! done. Enumeration is ordinary file reads under `/sys/bus/usb/devices`, and
//! the transfers themselves belong to `usbip-host` in the kernel. A crate that
//! opened devices itself would be a second USB stack competing with the one
//! already running, and would need privileges enumeration does not.
//!
//! See `docs/usb-design.md`.

pub mod arbiter;
#[cfg(target_os = "linux")]
pub mod export;
#[cfg(target_os = "linux")]
pub mod import;
#[cfg(target_os = "linux")]
mod linux;
mod offer;
#[cfg(target_os = "linux")]
pub mod pipe;
#[cfg(target_os = "linux")]
pub mod setup;
pub mod urb;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub use export::Exporter;
#[cfg(target_os = "linux")]
pub use import::{Importer, Speed};
#[cfg(target_os = "linux")]
pub use linux::LinuxUsb;
#[cfg(target_os = "linux")]
pub use pipe::{KernelPipe, PipeEnded};

pub use offer::DeviceOffers;
#[cfg(target_os = "windows")]
pub use windows::WindowsUsb;

use thiserror::Error;

/// What went wrong locally, before anything reached the wire.
///
/// Distinct from `cursio_protocol::usb::UsbError`, which is what a *peer* is
/// told: this type names things only the local machine can see, and the session
/// converts. Messages say what to install or load, because "permission denied"
/// without "run modprobe" is a dead end for the user.
#[derive(Debug, Error)]
pub enum UsbBackendError {
    #[error("usb/ip support is missing: {0}")]
    DriverMissing(String),
    #[error("binding a device needs root: {0}")]
    NeedsPrivilege(String),
    #[error("no such device: {0}")]
    NoSuchDevice(String),
    #[error("usb enumeration failed: {0}")]
    Enumerate(String),
    /// The USB/IP stream stopped making sense. Framing errors end a stream
    /// rather than being skipped: once sync is lost every later message is
    /// misread, so continuing would corrupt transfers rather than drop one.
    #[error("malformed usb/ip stream: {0}")]
    Framing(String),
}

/// A device as the local machine sees it, before it is offered to anyone.
///
/// Carries the bus id — the kernel's own handle, `1-5` — which the protocol
/// type deliberately does not: a bus id is meaningless on another machine and
/// changes when the cable moves, so only the host may hold one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDevice {
    /// The kernel's handle for this device, e.g. `1-5`.
    pub busid: String,
    pub vendor: u16,
    pub product: u16,
    pub manufacturer: Option<String>,
    pub product_name: Option<String>,
    /// Device class, or the first interface's class when the device defers to
    /// its interfaces (class 0, which most composite devices report).
    pub class: u8,
    /// Whether any interface has an isochronous endpoint. Those are the
    /// transfers that do not survive a network hop, so the user is told before
    /// they attach rather than left to discover it as stutter.
    pub has_isochronous: bool,
    /// Whether this is a root hub. Sharing one would export the whole
    /// controller, which is never what anybody means.
    pub is_root_hub: bool,
}

impl LocalDevice {
    /// Whether this device may be offered to the mesh at all.
    ///
    /// Root hubs are excluded because exporting one takes every port on that
    /// controller with it — including, on a laptop, the keyboard the user would
    /// need to undo it.
    pub fn is_shareable(&self) -> bool {
        !self.is_root_hub
    }
}
