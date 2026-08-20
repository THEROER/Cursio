//! Turning local devices into what the mesh is offered.
//!
//! Two things happen here, and both exist because the wire type deliberately
//! carries less than the kernel does.
//!
//! ## A bus id never leaves this machine
//!
//! `1-5` is the kernel's handle, and it is meaningless anywhere else: it names
//! a controller and a port on *this* box, and it changes when the user moves
//! the cable. A peer holding one would find its claim pointing at whatever
//! else was plugged into that port later. So the host mints a
//! [`UsbDeviceId`] and keeps the mapping — the peer quotes the id back, and
//! only the host ever resolves it to a port.
//!
//! ## The id has to survive a re-announcement
//!
//! A device that is still plugged in must keep its id across enumerations, or
//! every listing would look to a peer like the old devices vanishing and new
//! ones arriving — and a claim in flight would be answered `Gone`. The id is
//! derived from what the device *is* (bus id, vendor, product), not from the
//! order it was enumerated in.

use std::collections::HashMap;

use cursio_protocol::usb::{UsbDevice, UsbDeviceId, UsbTransport};

use crate::LocalDevice;

/// Maps between the kernel's bus ids and the ids peers are given.
///
/// The host's private table: nothing here crosses the wire, and only the
/// machine with the port ever consults it.
#[derive(Default)]
pub struct DeviceOffers {
    by_id: HashMap<UsbDeviceId, String>,
}

impl DeviceOffers {
    pub fn new() -> Self {
        Self::default()
    }

    /// The shareable devices, as the mesh should see them.
    ///
    /// Root hubs are dropped rather than offered-and-refused: a hub is never
    /// something the user meant, so listing it is noise in every peer's device
    /// picker.
    pub fn offer(&mut self, local: &[LocalDevice]) -> Vec<UsbDevice> {
        self.by_id.clear();
        local
            .iter()
            .filter(|d| d.is_shareable())
            .map(|d| {
                let id = device_id(d);
                self.by_id.insert(id, d.busid.clone());
                UsbDevice {
                    id,
                    vendor: d.vendor,
                    product: d.product,
                    manufacturer: d.manufacturer.clone(),
                    description: d.product_name.clone(),
                    class: d.class,
                    transport: match d.has_isochronous {
                        true => UsbTransport::Degraded,
                        false => UsbTransport::Fine,
                    },
                    // Filled in by the session, which is what knows who holds
                    // what. A backend has no idea a mesh exists.
                    holder: None,
                }
            })
            .collect()
    }

    /// Which port a peer's id refers to, if it still refers to anything.
    ///
    /// `None` for a device that has been unplugged since the listing — which is
    /// exactly the case the session answers with `Gone`.
    pub fn busid(&self, id: UsbDeviceId) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }
}

/// A stable id for a device, derived from what it is.
///
/// Bus id, vendor and product together: the bus id alone would collide across
/// a replug into another port, and vendor/product alone would collide between
/// two identical devices — two of the same USB stick is an ordinary thing, and
/// giving them one id would let a claim on either detach the other.
fn device_id(d: &LocalDevice) -> UsbDeviceId {
    // FNV-1a: the mapping only has to be stable within a run and unlikely to
    // collide. Nothing depends on it being unpredictable, and nothing outside
    // this machine ever computes it.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(d.busid.as_bytes());
    eat(&d.vendor.to_le_bytes());
    eat(&d.product.to_le_bytes());
    UsbDeviceId(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(busid: &str, vendor: u16, product: u16) -> LocalDevice {
        LocalDevice {
            busid: busid.into(),
            vendor,
            product,
            manufacturer: Some("Google".into()),
            product_name: Some("Pixel".into()),
            class: 0xff,
            has_isochronous: false,
            is_root_hub: false,
        }
    }

    /// The whole reason ids exist: a peer must never see a bus id, because it
    /// cannot mean anything on the other machine.
    #[test]
    fn an_offer_carries_no_bus_id() {
        let mut offers = DeviceOffers::new();
        let offered = offers.offer(&[local("1-5", 0x18d1, 0x4ee7)]);

        assert_eq!(offered.len(), 1);
        // The struct has no field for one; this asserts the mapping is kept
        // privately instead.
        assert_eq!(offers.busid(offered[0].id), Some("1-5"));
    }

    /// A device still plugged in keeps its id across enumerations, or every
    /// re-announcement looks like the device was replaced.
    #[test]
    fn a_device_keeps_its_id_across_listings() {
        let mut offers = DeviceOffers::new();
        let devices = [local("1-5", 0x18d1, 0x4ee7)];

        let first = offers.offer(&devices);
        let second = offers.offer(&devices);

        assert_eq!(first[0].id, second[0].id);
    }

    /// Two identical sticks in two ports are two devices. One id between them
    /// would let a claim on one detach the other.
    #[test]
    fn identical_devices_in_different_ports_get_different_ids() {
        let mut offers = DeviceOffers::new();
        let offered = offers.offer(&[local("1-5", 0x0781, 0x5567), local("1-6", 0x0781, 0x5567)]);

        assert_ne!(offered[0].id, offered[1].id);
    }

    /// Different devices in the same port — a replug — must not inherit each
    /// other's id, or a claim in flight lands on the wrong thing.
    #[test]
    fn a_different_device_in_the_same_port_gets_a_different_id() {
        let mut offers = DeviceOffers::new();
        let phone = offers.offer(&[local("1-5", 0x18d1, 0x4ee7)])[0].id;
        let stick = offers.offer(&[local("1-5", 0x0781, 0x5567)])[0].id;

        assert_ne!(phone, stick);
    }

    /// A hub is never what the user meant, so it is not offered at all.
    #[test]
    fn root_hubs_are_left_out_of_the_offer() {
        let mut offers = DeviceOffers::new();
        let mut hub = local("usb1", 0x1d6b, 0x0002);
        hub.is_root_hub = true;

        let offered = offers.offer(&[hub, local("1-5", 0x18d1, 0x4ee7)]);

        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].vendor, 0x18d1);
    }

    /// An isochronous device is offered, but flagged — the user is told before
    /// they attach a webcam, not after it stutters.
    #[test]
    fn an_isochronous_device_is_offered_as_degraded() {
        let mut offers = DeviceOffers::new();
        let mut camera = local("1-5", 0x046d, 0x0825);
        camera.has_isochronous = true;

        let offered = offers.offer(&[camera]);

        assert_eq!(offered[0].transport, UsbTransport::Degraded);
    }

    /// An id from a listing taken before the device was unplugged resolves to
    /// nothing, which is what lets the session answer `Gone` rather than
    /// exporting whatever now sits in that port.
    #[test]
    fn an_id_for_an_unplugged_device_resolves_to_nothing() {
        let mut offers = DeviceOffers::new();
        let id = offers.offer(&[local("1-5", 0x18d1, 0x4ee7)])[0].id;

        offers.offer(&[]);

        assert_eq!(offers.busid(id), None);
    }
}
