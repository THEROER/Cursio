//! Deciding what happens to each transfer.
//!
//! The piece that lets several machines hold one device at once. A URB is an
//! operation, not raw material — `crates/files` makes the same move with FUSE,
//! where `read(handle, offset, len)` is a request the server satisfies however
//! it likes, which is why any number of programs read a mounted volume — so a
//! transfer is classified and answered rather than forwarded blindly.
//!
//! Nothing here talks to a device or a socket. It reads a header and returns a
//! verdict, which is what makes every rule below testable without hardware.
//!
//! ## Three verdicts
//!
//! - **Answer from cache.** Enumeration reads data that cannot change while
//!   the device stays plugged in, and every host asks the identical questions.
//!   Serving them locally is what makes the second, third and fourth attach
//!   cost the device nothing and disturb nobody.
//! - **Forward.** Ordinary work. It queues; the device performs one transfer at
//!   a time as it always did, and the reply goes back to whoever asked.
//! - **Intercept.** A request that changes what the device *is* rather than
//!   what it is doing. Honoured for the machine holding the lease, answered
//!   without touching the device when it asks for a state already in force, and
//!   refused otherwise — refusing one host is better than resetting the device
//!   under another mid-transfer.
//!
//! The exclusivity USB really imposes therefore covers a handful of control
//! requests, not the device.

use std::collections::HashMap;

use cursio_protocol::PeerId;

use crate::urb::{Header, Setup};

/// Standard requests, from `linux/usb/ch9.h`.
mod req {
    pub const GET_STATUS: u8 = 0x00;
    pub const CLEAR_FEATURE: u8 = 0x01;
    pub const SET_FEATURE: u8 = 0x03;
    pub const SET_ADDRESS: u8 = 0x05;
    pub const GET_DESCRIPTOR: u8 = 0x06;
    pub const SET_DESCRIPTOR: u8 = 0x07;
    pub const GET_CONFIGURATION: u8 = 0x08;
    pub const SET_CONFIGURATION: u8 = 0x09;
    pub const GET_INTERFACE: u8 = 0x0a;
    pub const SET_INTERFACE: u8 = 0x0b;
}

/// What to do with one transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Answer here, with these bytes, without involving the device.
    FromCache(Vec<u8>),
    /// Send it to the device and route the reply back to the submitter.
    Forward,
    /// Answer success without touching the device: the request asks for a state
    /// that already holds, so performing it would be a no-op on the device and
    /// a disruption to everyone else.
    AlreadyInEffect,
    /// Refuse, because honouring it would change the device under another
    /// machine. Carries which peer holds the lease, so the refusal can say who.
    Refused(PeerId),
}

/// What a control request means for the device's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Reads something that cannot change while the device is plugged in.
    Cacheable,
    /// Reads something that can change, so it must reach the device.
    LiveRead,
    /// Changes what the device is.
    StateChange,
}

/// Classify a standard control request.
///
/// Only standard requests are classified. A class or vendor request may do
/// anything at all — including reconfiguring the device through a private
/// protocol — so it is never cached and always counted as a state change when
/// it writes.
fn classify(setup: Setup) -> Kind {
    if !setup.is_standard() {
        // A vendor read is still a read; a vendor write may be anything.
        return match setup.reads_device() {
            true => Kind::LiveRead,
            false => Kind::StateChange,
        };
    }
    match setup.request {
        // Descriptors are fixed for as long as the device stays plugged in.
        // This is the bulk of enumeration and the whole point of the cache.
        req::GET_DESCRIPTOR => Kind::Cacheable,
        // Which configuration or interface is selected can be read freely, but
        // the answer changes when somebody selects another, so it is not cached.
        req::GET_CONFIGURATION | req::GET_INTERFACE | req::GET_STATUS => Kind::LiveRead,
        // Everything that writes. `SET_ADDRESS` and `SET_DESCRIPTOR` are here
        // for completeness — a host should never send them over USB/IP, and one
        // that does must not have it forwarded.
        req::SET_CONFIGURATION
        | req::SET_INTERFACE
        | req::SET_FEATURE
        | req::CLEAR_FEATURE
        | req::SET_ADDRESS
        | req::SET_DESCRIPTOR => Kind::StateChange,
        // An unrecognised standard request is treated as a write. Guessing the
        // safe direction wrong costs a refusal; guessing it wrong the other way
        // costs another machine's device state.
        _ => Kind::StateChange,
    }
}

/// What the device is currently set to.
///
/// Only what a state-changing request can ask for, so a request naming the
/// current value can be answered without disturbing anyone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DeviceState {
    /// Selected configuration, once anyone has selected one.
    configuration: Option<u16>,
    /// Selected alternate setting per interface.
    interfaces: HashMap<u16, u16>,
}

/// Which machine may change the device, and until when.
#[derive(Debug, Clone, Copy)]
struct Lease {
    holder: PeerId,
    /// Transfers this holder still has outstanding. The lease lasts while it
    /// is using the device, not for as long as it stays attached: a machine
    /// that configured a device and went idle must not hold it against the
    /// next one forever.
    outstanding: u32,
}

/// Serves several hosts from one device.
#[derive(Default)]
pub struct Arbiter {
    /// Descriptor replies, keyed by the request that produced them.
    ///
    /// Keyed by `(wValue, wIndex)` rather than by descriptor type alone: string
    /// descriptors differ by index and by language, and configuration
    /// descriptors by index, so collapsing them would serve one host another's
    /// answer.
    descriptors: HashMap<(u16, u16), Vec<u8>>,
    state: DeviceState,
    lease: Option<Lease>,
}

impl Arbiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide what to do with `header`, submitted by `peer`.
    pub fn submit(&mut self, peer: PeerId, header: &Header) -> Verdict {
        // Anything that is not a control transfer is ordinary work: bulk,
        // interrupt and isochronous move data without changing what the device
        // is. They queue and the device performs them one at a time, exactly as
        // a host controller would.
        if !header.is_control() {
            self.charge(peer);
            return Verdict::Forward;
        }

        let setup = header.setup;
        match classify(setup) {
            Kind::Cacheable => match self.descriptors.get(&(setup.value, setup.index)) {
                // The device is not involved at all, so this costs nothing and
                // cannot disturb whoever is using it.
                Some(bytes) => Verdict::FromCache(truncate(bytes, setup.length)),
                // Not seen yet: forward it, and remember the answer.
                None => {
                    self.charge(peer);
                    Verdict::Forward
                }
            },
            Kind::LiveRead => {
                self.charge(peer);
                Verdict::Forward
            }
            Kind::StateChange => self.arbitrate(peer, setup),
        }
    }

    /// Decide a request that would change the device.
    fn arbitrate(&mut self, peer: PeerId, setup: Setup) -> Verdict {
        // Asking for what already holds is the common case, and answering it
        // locally is what lets several machines enumerate and configure the
        // same device: a second host selecting the configuration a first
        // already selected wants exactly the state it will get.
        if self.already_in_effect(setup) {
            return Verdict::AlreadyInEffect;
        }

        match self.lease {
            // Somebody else is using the device. Refusing is the honest answer:
            // performing it would reset the device under them mid-transfer,
            // which is the hazard behind USB's one-host rule.
            Some(lease) if lease.holder != peer && lease.outstanding > 0 => {
                Verdict::Refused(lease.holder)
            }
            // Free, or already ours, or held by a machine that has gone quiet.
            _ => {
                self.record(setup);
                self.lease = Some(Lease {
                    holder: peer,
                    outstanding: 1,
                });
                Verdict::Forward
            }
        }
    }

    /// Whether the device is already in the state this request asks for.
    fn already_in_effect(&self, setup: Setup) -> bool {
        match setup.request {
            req::SET_CONFIGURATION => self.state.configuration == Some(setup.value),
            req::SET_INTERFACE => self.state.interfaces.get(&setup.index) == Some(&setup.value),
            // Everything else genuinely changes something, or is not something
            // this tracks — either way it is not already in effect.
            _ => false,
        }
    }

    /// Remember a state change that is about to be performed.
    fn record(&mut self, setup: Setup) {
        match setup.request {
            req::SET_CONFIGURATION => self.state.configuration = Some(setup.value),
            req::SET_INTERFACE => {
                self.state.interfaces.insert(setup.index, setup.value);
            }
            _ => {}
        }
    }

    /// Note that `peer` has a transfer outstanding, taking the lease if it is
    /// free. Holding it is what stops another machine reconfiguring the device
    /// between this transfer and its reply.
    fn charge(&mut self, peer: PeerId) {
        match &mut self.lease {
            Some(lease) if lease.holder == peer => lease.outstanding += 1,
            Some(lease) if lease.outstanding > 0 => {}
            // Free, or held by a machine with nothing in flight.
            _ => {
                self.lease = Some(Lease {
                    holder: peer,
                    outstanding: 1,
                })
            }
        }
    }

    /// Record the device's answer to a forwarded transfer.
    ///
    /// `setup` is the request that produced it, which the caller kept when it
    /// forwarded: a reply header carries no setup bytes of its own, so the
    /// association has to be held across the round trip.
    pub fn completed(&mut self, peer: PeerId, setup: Option<Setup>, payload: &[u8]) {
        if let Some(lease) = &mut self.lease {
            if lease.holder == peer {
                lease.outstanding = lease.outstanding.saturating_sub(1);
            }
        }

        // Cache a descriptor the moment it comes back, so the next host to
        // enumerate never reaches the device.
        if let Some(setup) = setup {
            if matches!(classify(setup), Kind::Cacheable) && !payload.is_empty() {
                self.descriptors
                    .insert((setup.value, setup.index), payload.to_vec());
            }
        }
    }

    /// Forget everything learned about the device.
    ///
    /// Called when it is unplugged: descriptors belong to *this* device, and a
    /// different one in the same port would otherwise be described by its
    /// predecessor's answers.
    pub fn forget(&mut self) {
        self.descriptors.clear();
        self.state = DeviceState::default();
        self.lease = None;
    }

    /// Release everything a peer holds, because its link is gone.
    ///
    /// A machine that goes away cannot complete its transfers, so its lease
    /// would otherwise block every other machine forever.
    pub fn peer_gone(&mut self, peer: PeerId) {
        if self.lease.is_some_and(|lease| lease.holder == peer) {
            self.lease = None;
        }
    }

    /// How many descriptor replies are being served locally.
    pub fn cached_descriptors(&self) -> usize {
        self.descriptors.len()
    }
}

/// A host may ask for less than the cached answer holds — enumeration reads a
/// descriptor's first bytes to learn its length, then asks again for all of it.
/// Returning more than `wLength` overruns the buffer the host set aside.
fn truncate(bytes: &[u8], wanted: u16) -> Vec<u8> {
    bytes[..bytes.len().min(wanted as usize)].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::urb::HEADER_LEN;

    fn peer(n: u8) -> PeerId {
        PeerId::from_bytes([n; 16])
    }

    /// A control transfer with the given setup bytes.
    fn control(setup: [u8; 8]) -> Header {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0x00..0x04].copy_from_slice(&1u32.to_be_bytes());
        bytes[0x0c..0x10].copy_from_slice(&1u32.to_be_bytes());
        bytes[0x28..0x30].copy_from_slice(&setup);
        Header::parse(&bytes).expect("parses")
    }

    /// A bulk transfer on endpoint 2 — ordinary work, no setup packet.
    fn bulk() -> Header {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0x00..0x04].copy_from_slice(&1u32.to_be_bytes());
        bytes[0x10..0x14].copy_from_slice(&2u32.to_be_bytes());
        Header::parse(&bytes).expect("parses")
    }

    /// `GET_DESCRIPTOR` for the device descriptor.
    fn get_device_descriptor() -> [u8; 8] {
        [
            0x80,
            req::GET_DESCRIPTOR,
            0x00,
            0x01,
            0x00,
            0x00,
            0x12,
            0x00,
        ]
    }

    /// `SET_CONFIGURATION` selecting configuration `n`.
    fn set_configuration(n: u8) -> [u8; 8] {
        [
            0x00,
            req::SET_CONFIGURATION,
            n,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ]
    }

    /// `SET_INTERFACE`, alternate setting `alt` on interface `iface`.
    fn set_interface(iface: u8, alt: u8) -> [u8; 8] {
        [0x01, req::SET_INTERFACE, alt, 0x00, iface, 0x00, 0x00, 0x00]
    }

    /// The first host to ask for a descriptor reaches the device, because
    /// nothing has answered it yet.
    #[test]
    fn the_first_descriptor_read_reaches_the_device() {
        let mut a = Arbiter::new();
        assert_eq!(
            a.submit(peer(1), &control(get_device_descriptor())),
            Verdict::Forward
        );
    }

    /// The whole point: once one host has enumerated, every other host is
    /// served locally and the device is not involved at all.
    #[test]
    fn a_second_host_enumerates_without_touching_the_device() {
        let mut a = Arbiter::new();
        let req = control(get_device_descriptor());
        a.submit(peer(1), &req);
        a.completed(peer(1), Some(req.setup), &[0x12, 0x01, 0x00, 0x02]);

        assert_eq!(
            a.submit(peer(2), &req),
            Verdict::FromCache(vec![0x12, 0x01, 0x00, 0x02]),
            "the second host must not reach the device"
        );
    }

    /// Enumeration reads a descriptor's first bytes to learn its length, then
    /// asks again for all of it. Answering with more than was asked for
    /// overruns the buffer the host set aside.
    #[test]
    fn a_cached_answer_is_cut_to_the_length_asked_for() {
        let mut a = Arbiter::new();
        let full = control(get_device_descriptor());
        a.submit(peer(1), &full);
        a.completed(peer(1), Some(full.setup), &[0u8; 18]);

        // The same descriptor, but only its first 8 bytes.
        let mut short = get_device_descriptor();
        short[6] = 8;
        match a.submit(peer(2), &control(short)) {
            Verdict::FromCache(bytes) => assert_eq!(bytes.len(), 8),
            other => panic!("expected a cached answer, got {other:?}"),
        }
    }

    /// String descriptors differ by index and language, configurations by
    /// index. Keying on the request type alone would serve one host another's
    /// descriptor.
    #[test]
    fn descriptors_are_cached_per_index_not_per_type() {
        let mut a = Arbiter::new();
        let mut first = get_device_descriptor();
        first[2] = 0x01; // wValue low byte: string index 1
        first[3] = 0x03; // descriptor type: string
        let mut second = first;
        second[2] = 0x02; // a different string

        let first = control(first);
        a.submit(peer(1), &first);
        a.completed(peer(1), Some(first.setup), b"one");

        assert_eq!(
            a.submit(peer(1), &control(second)),
            Verdict::Forward,
            "a different string must not be served from the first one's answer"
        );
    }

    /// Bulk and interrupt transfers move data without changing what the device
    /// is, so they queue rather than compete.
    #[test]
    fn ordinary_transfers_are_forwarded_for_everyone() {
        let mut a = Arbiter::new();
        assert_eq!(a.submit(peer(1), &bulk()), Verdict::Forward);
        assert_eq!(a.submit(peer(2), &bulk()), Verdict::Forward);
    }

    /// The case that makes sharing work at all: a second host selecting the
    /// configuration already in force wants exactly the state it will get, so
    /// answering locally disturbs nobody.
    #[test]
    fn selecting_the_configuration_already_in_force_is_answered_locally() {
        let mut a = Arbiter::new();
        assert_eq!(
            a.submit(peer(1), &control(set_configuration(1))),
            Verdict::Forward
        );
        a.completed(peer(1), None, &[]);

        assert_eq!(
            a.submit(peer(2), &control(set_configuration(1))),
            Verdict::AlreadyInEffect,
            "the device is already in this state"
        );
    }

    /// The hazard behind USB's one-host rule: a second host reconfiguring the
    /// device while a first is mid-transfer resets it under them.
    #[test]
    fn reconfiguring_under_a_busy_host_is_refused() {
        let mut a = Arbiter::new();
        a.submit(peer(1), &control(set_configuration(1)));
        // peer(1) still has that transfer outstanding.

        assert_eq!(
            a.submit(peer(2), &control(set_configuration(2))),
            Verdict::Refused(peer(1)),
            "and the refusal names who holds it"
        );
    }

    /// A machine that configured a device and went idle must not hold it
    /// against the next one forever.
    #[test]
    fn an_idle_holder_does_not_block_the_next_machine() {
        let mut a = Arbiter::new();
        a.submit(peer(1), &control(set_configuration(1)));
        a.completed(peer(1), None, &[]);

        assert_eq!(
            a.submit(peer(2), &control(set_configuration(2))),
            Verdict::Forward,
            "nothing is in flight, so the device is free to change"
        );
    }

    /// Alternate settings are per interface: setting interface 1 must not be
    /// mistaken for interface 0 already being set.
    #[test]
    fn alternate_settings_are_tracked_per_interface() {
        let mut a = Arbiter::new();
        a.submit(peer(1), &control(set_interface(0, 1)));
        a.completed(peer(1), None, &[]);

        assert_eq!(
            a.submit(peer(2), &control(set_interface(0, 1))),
            Verdict::AlreadyInEffect
        );
        assert_eq!(
            a.submit(peer(2), &control(set_interface(1, 1))),
            Verdict::Forward,
            "a different interface is a different setting"
        );
    }

    /// A vendor request may reconfigure the device through a private protocol,
    /// so it is never served from cache and never assumed harmless.
    #[test]
    fn a_vendor_write_counts_as_a_state_change() {
        let mut a = Arbiter::new();
        // 0x40: host-to-device, vendor type.
        let vendor = control([0x40, 0x01, 0, 0, 0, 0, 0, 0]);
        assert_eq!(a.submit(peer(1), &vendor), Verdict::Forward);

        assert_eq!(
            a.submit(peer(2), &vendor),
            Verdict::Refused(peer(1)),
            "a vendor write must not slip past arbitration"
        );
    }

    /// Which configuration is selected changes when somebody selects another,
    /// so it cannot be cached — unlike a descriptor.
    #[test]
    fn reading_the_current_configuration_is_never_cached() {
        let mut a = Arbiter::new();
        let read = control([0x80, req::GET_CONFIGURATION, 0, 0, 0, 0, 1, 0]);
        a.submit(peer(1), &read);
        a.completed(peer(1), Some(read.setup), &[1]);

        assert_eq!(
            a.submit(peer(2), &read),
            Verdict::Forward,
            "the answer can change, so it must reach the device"
        );
    }

    /// A machine that goes away cannot complete its transfers, so its lease
    /// would block every other machine forever.
    #[test]
    fn a_peer_going_away_releases_what_it_held() {
        let mut a = Arbiter::new();
        a.submit(peer(1), &control(set_configuration(1)));

        a.peer_gone(peer(1));

        assert_eq!(
            a.submit(peer(2), &control(set_configuration(2))),
            Verdict::Forward
        );
    }

    /// Descriptors describe *this* device. A different one in the same port
    /// would otherwise be described by its predecessor's answers.
    #[test]
    fn unplugging_forgets_the_device_it_described() {
        let mut a = Arbiter::new();
        let req = control(get_device_descriptor());
        a.submit(peer(1), &req);
        a.completed(peer(1), Some(req.setup), &[0x12, 0x01]);
        assert_eq!(a.cached_descriptors(), 1);

        a.forget();

        assert_eq!(a.cached_descriptors(), 0);
        assert_eq!(
            a.submit(peer(2), &req),
            Verdict::Forward,
            "the new device must describe itself"
        );
    }

    /// An empty reply is not an answer worth keeping: a device that failed the
    /// request would otherwise have its failure served to every later host.
    #[test]
    fn an_empty_reply_is_not_cached() {
        let mut a = Arbiter::new();
        let req = control(get_device_descriptor());
        a.submit(peer(1), &req);
        a.completed(peer(1), Some(req.setup), &[]);

        assert_eq!(a.cached_descriptors(), 0);
        assert_eq!(a.submit(peer(2), &req), Verdict::Forward);
    }
}
