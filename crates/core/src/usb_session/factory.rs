//! Building the USB module for a link, with the backend this platform has.

use crate::module::SessionModule;
#[cfg(target_os = "linux")]
use crate::usb_session::linux::LinuxUsbBackend;
#[cfg(target_os = "windows")]
use crate::usb_session::windows::{report_usbipd_exposure, WindowsUsbBackend};
use crate::usb_session::UsbBackend;
use crate::usb_session::UsbEngine;
use crate::usb_session::VERSION;
use cursio_protocol::ModuleId;
use cursio_protocol::PeerId;

/// Builds the USB module for a link, and holds what every link shares.
///
/// The factory outlives each session — a node builds one registry at startup —
/// which is what makes it the right place for [`UsbNode`]. Built on the first
/// link rather than in the constructor, so a machine that never negotiates the
/// module never touches its kernel.
///
/// [`UsbNode`]: crate::usb_node::UsbNode
pub struct UsbFactory {
    pub enabled: bool,
    /// This machine's ports. One per node, shared by every session.
    node: std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<crate::usb_node::UsbNode>>>>,
}

impl UsbFactory {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            node: std::sync::Mutex::new(None),
        }
    }

    /// The node, created on first use.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn node(
        &self,
        host: PeerId,
        make: impl FnOnce() -> Box<dyn UsbBackend>,
    ) -> std::sync::Arc<std::sync::Mutex<crate::usb_node::UsbNode>> {
        let mut slot = self.node.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert_with(|| {
            std::sync::Arc::new(std::sync::Mutex::new(crate::usb_node::UsbNode::new(
                make(),
                host,
            )))
        })
        .clone()
    }
}

/// Why this process's one request to set the machine up came to nothing.
#[cfg(target_os = "linux")]
static SETUP_REFUSED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Whether this machine is set up to share USB, asking for privilege at most
/// once per process.
///
/// One prompt for the machine, not one per device: loading the modules and
/// making the control files group-writable is a property of this host, and
/// everything after it runs unprivileged. Does nothing when the machine is
/// already set up, so this is free on every start but the first.
///
/// Remembered because adverts are built for every link: a machine where the
/// prompt was declined, or where pkexec has no agent to show one, asked again
/// for each — three times within a second on one laptop, and on a desktop that
/// would have been a dialog per link. A machine set up by hand after the
/// refusal is noticed without asking.
#[cfg(target_os = "linux")]
fn set_up_once() -> bool {
    if SETUP_REFUSED.get().is_some() {
        return cursio_usb::setup::readiness().is_complete();
    }
    let refused = match cursio_usb::setup::ensure_ready() {
        Ok(true) => return true,
        Ok(false) => "setting this machine up needs privilege and the prompt was declined \
                      (or no polkit agent could show it); restart cursio to be asked again"
            .to_string(),
        Err(e) => e.to_string(),
    };
    tracing::warn!(reason = %refused, "usb sharing is off");
    let _ = SETUP_REFUSED.set(refused);
    false
}

impl crate::module::ModuleFactory for UsbFactory {
    fn id(&self) -> ModuleId {
        ModuleId::USB
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        if !self.enabled {
            return None;
        }
        // Absence is advertised, not simulated: a machine whose kernel cannot
        // export says nothing, so a peer is never offered a device that would
        // fail on claim. The log line is what turns "nothing happened" into a
        // `modprobe` the user can run.
        #[cfg(target_os = "linux")]
        {
            if !set_up_once() {
                return None;
            }
            if !LinuxUsbBackend::new().is_available() {
                tracing::warn!("usb sharing is off: the usbip-host module did not load");
                return None;
            }
            Some(cursio_protocol::ModuleAdvert::symmetric(
                ModuleId::USB,
                VERSION,
            ))
        }
        // Windows shares through `usbipd-win`, which is an install rather than
        // a module to load — so the absent case names the installer instead.
        #[cfg(target_os = "windows")]
        {
            if !WindowsUsbBackend::new().is_available() {
                tracing::warn!(
                    "usb sharing is off: usbipd-win is not installed \
                     (winget install usbipd)"
                );
                return None;
            }
            Some(cursio_protocol::ModuleAdvert::symmetric(
                ModuleId::USB,
                VERSION,
            ))
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            tracing::warn!("usb sharing is not implemented on this platform yet");
            None
        }
    }

    #[cfg(target_os = "linux")]
    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(UsbEngine::new(
            ctx.peer_name().to_string(),
            ctx.peer.peer_id,
            self.node(ctx.local.peer_id, || Box::new(LinuxUsbBackend::new())),
        )))
    }

    #[cfg(target_os = "windows")]
    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(UsbEngine::new(
            ctx.peer_name().to_string(),
            ctx.peer.peer_id,
            self.node(ctx.local.peer_id, || {
                report_usbipd_exposure();
                Box::new(WindowsUsbBackend::new())
            }),
        )))
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    fn build(&self, _ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        anyhow::bail!("usb sharing is not implemented on this platform")
    }
}
