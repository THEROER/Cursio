//! What a machine knows about its own ports, across every link at once.
//!
//! [`usb_session`](crate::usb_session) is a [`SessionModule`], so there is one
//! of it per peer. That is right for everything it says on the wire and wrong
//! for everything it decides about a device, because a device belongs to the
//! *machine* and the machine has as many links as it has peers.
//!
//! Three things went wrong while that distinction was missing, and all three
//! have the same shape:
//!
//! - **The backend was built per session.** Each link had its own exporter, its
//!   own [`DeviceOffers`] and its own stream numbering over one physical port.
//!   Two peers meant two machines each believing they had exported the device.
//! - **The arbiter had nowhere to live.** It keys a descriptor cache and a
//!   configuration lease by peer — `submit(peer, header)` — which only means
//!   anything if every peer's transfers reach the *same* arbiter. One per link
//!   is one cache per link, which is the very desynchronisation it exists to
//!   prevent.
//! - **The host could not use its own device.** Exporting binds the device away
//!   from its own driver, so the machine with the port lost it the moment it
//!   shared it. There is no session with oneself, so there was no module to put
//!   the fix in.
//!
//! So this is the node-level half: one per machine, holding the backend, the
//! arbiter and the host's own attachment. Rule 4 in `docs/platform-design.md`
//! — one owner per piece of state — where the state is the device and the owner
//! is the machine with the port. The same shape as
//! [`node_status`](crate::node_status): one monitor per machine, one cursor per
//! link.
//!
//! Shared as an `Arc<Mutex<_>>`, the same shape
//! [`service_session`](crate::service_session) uses for the state its links
//! share: `SessionModule` is `Send`, so the node-level half cannot be an `Rc`
//! even though the sessions themselves run on one thread.
//!
//! [`DeviceOffers`]: cursio_usb::DeviceOffers
//! [`SessionModule`]: crate::module::SessionModule

use std::collections::HashMap;

use cursio_protocol::usb::{UsbDevice, UsbDeviceId, UsbError, UsbStreamId};
use cursio_protocol::PeerId;

use crate::usb_session::UsbBackend;

/// One device this machine has exported, and who is using it.
///
/// `holders` is a set rather than one peer because attaching is not a claim:
/// any number of machines enumerate a device, and what they contend for is its
/// configuration, not the device. See "Ownership, restated" in
/// `docs/usb-design.md`.
struct Exported {
    /// The stream each holder's URBs ride, keyed by peer. A stream is per link,
    /// because that is what carries it.
    holders: HashMap<PeerId, UsbStreamId>,
    /// Whether the host has attached the device to itself as well.
    ///
    /// Separate from `holders` because the host is not a peer and has no
    /// stream: its URBs never cross a link.
    local: bool,
}

/// Everything about this machine's ports that outlives a single link.
pub struct UsbNode {
    backend: Box<dyn UsbBackend>,
    /// Devices exported from this machine, keyed by device.
    ///
    /// The host's own record and the only authority on who is using what — the
    /// single writer rule, where the writer is the machine with the port.
    exported: HashMap<UsbDeviceId, Exported>,
    /// Stream ids, minted here rather than per link so two peers can never be
    /// handed the same number for different work.
    next_stream: u64,
}

impl UsbNode {
    pub fn new(backend: Box<dyn UsbBackend>) -> Self {
        Self {
            backend,
            exported: HashMap::new(),
            next_stream: 1,
        }
    }

    /// This machine's devices, each carrying who is using it.
    ///
    /// Read from the backend every time rather than cached: a device unplugged
    /// between two listings simply is not in the second one, which is what lets
    /// an unplug propagate without anything watching for it.
    pub fn listing(&mut self) -> Vec<UsbDevice> {
        let mut devices = self.backend.devices();
        for device in &mut devices {
            // `holder` carries one peer because the wire type predates
            // arbitration, when there could only be one. Any holder is a
            // truthful answer to "is somebody using this"; which one is not
            // something a peer acts on.
            device.holder = self
                .exported
                .get(&device.id)
                .and_then(|e| e.holders.keys().next().copied());
        }
        devices
    }

    /// Whether a device is currently exported at all.
    pub fn is_exported(&self, device: UsbDeviceId) -> bool {
        self.exported.contains_key(&device)
    }

    /// How many devices this machine is currently serving.
    pub fn exported_count(&self) -> usize {
        self.exported.len()
    }

    /// Give `peer` a stream onto `device`, exporting it if this is the first
    /// taker.
    ///
    /// Attaching is not exclusive: a second peer joins an export that is
    /// already running rather than being refused. What the peers then contend
    /// for is the configuration lease, and that is the arbiter's business, not
    /// this one's.
    pub fn attach_peer(
        &mut self,
        device: UsbDeviceId,
        peer: PeerId,
        peer_name: &str,
    ) -> Result<UsbStreamId, UsbError> {
        if let Some(existing) = self.exported.get(&device) {
            if let Some(stream) = existing.holders.get(&peer) {
                // A reconnecting peer re-asks for what it believes it still
                // holds; answering with the stream it already has is the truth,
                // not an error.
                return Ok(*stream);
            }
        }

        if !self.exported.contains_key(&device) {
            if !self.backend.devices().iter().any(|d| d.id == device) {
                return Err(UsbError::Gone);
            }
            self.backend.export(device)?;
            self.exported.insert(
                device,
                Exported {
                    holders: HashMap::new(),
                    local: false,
                },
            );
            tracing::info!(%device, "device exported to the mesh");
        }

        let stream = UsbStreamId(self.next_stream);
        self.next_stream += 1;
        let entry = self
            .exported
            .get_mut(&device)
            .expect("just inserted above when absent");
        entry.holders.insert(peer, stream);
        tracing::info!(
            peer = %peer_name, %device, holders = entry.holders.len(),
            "a peer is now using a device on this machine"
        );
        Ok(stream)
    }

    /// Stop serving `peer`, unexporting once nobody is left.
    ///
    /// The device stays exported while anyone else — including this machine
    /// itself — is still using it: releasing one holder must not detach the
    /// bus from under another.
    pub fn detach_peer(&mut self, device: UsbDeviceId, peer: PeerId) -> Option<UsbStreamId> {
        let entry = self.exported.get_mut(&device)?;
        let stream = entry.holders.remove(&peer)?;
        if entry.holders.is_empty() && !entry.local {
            self.exported.remove(&device);
            self.backend.unexport(device);
            tracing::info!(%device, "nobody is using the device; returned to this machine");
        }
        Some(stream)
    }

    /// Everything `peer` was using, released at once.
    ///
    /// A session ending is not a release per device: the link is simply gone,
    /// and a device left marked as held by a peer that will never speak again
    /// would stay unavailable for the rest of the run.
    pub fn peer_gone(&mut self, peer: PeerId) {
        let devices: Vec<UsbDeviceId> = self
            .exported
            .iter()
            .filter(|(_, e)| e.holders.contains_key(&peer))
            .map(|(id, _)| *id)
            .collect();
        for device in devices {
            self.detach_peer(device, peer);
        }
    }

    /// Which device a stream belongs to, and whose it is.
    pub fn device_of_stream(&self, stream: UsbStreamId) -> Option<UsbDeviceId> {
        self.exported
            .iter()
            .find(|(_, e)| e.holders.values().any(|s| *s == stream))
            .map(|(id, _)| *id)
    }

    /// The backend, for the parts of a session that still drive it directly.
    pub fn backend(&mut self) -> &mut dyn UsbBackend {
        self.backend.as_mut()
    }
}
