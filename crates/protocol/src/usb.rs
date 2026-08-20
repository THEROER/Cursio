//! USB device sharing: the wire vocabulary.
//!
//! A device is plugged into one machine and used from another — `adb` on the
//! desktop talking to a phone on the laptop's port. See `docs/usb-design.md`.
//!
//! ## Cursio does not speak URB
//!
//! The transfers themselves are USB/IP, which both platforms already implement
//! in the kernel: `usbip-host` exports, `vhci-hcd` imports, and `usbipd-win`
//! is the importing half on Windows. Nothing here describes a USB transfer,
//! and nothing here should ever need to: the payload of [`UsbMsg::Data`] is
//! opaque bytes belonging to a protocol two kernels already agree on.
//!
//! What USB/IP has no answer for is *who may attach* — its daemon listens with
//! no authentication at all, so whoever reaches the port takes the device. That
//! is the half this vocabulary covers: discovery, trust, ownership and
//! revocation, over a channel that is already paired and encrypted.
//!
//! ## One holder at a time, and only the holder lets go
//!
//! Exporting a device *takes it away* from the machine it is plugged into:
//! binding rebinds it from its class driver to the USB/IP one, so while a peer
//! holds the phone, the laptop hosting it has no phone. This is a property of
//! USB — one host per device — not of the transport, so the vocabulary makes
//! the handover explicit rather than pretending a device can be in two places.
//!
//! Two roles, which are not the same thing and are deliberately named apart:
//!
//! - the **host** owns the physical port. It decides what is offerable, and it
//!   may always end an attach with [`UsbMsg::Detached`] — an unplug beats
//!   any protocol state.
//! - the **holder** currently has the device attached. Only the holder may
//!   [`UsbMsg::Release`] it.
//!
//! A claim from a machine that is not the holder is refused, never honoured.
//! The shared cursor learned this the hard way: an idle machine acting on a
//! transition it did not own left the pointer somewhere nobody was sitting,
//! with no way back (see `docs/input-design.md`). A device is worse — a claim
//! honoured from the wrong side detaches a bus a program is mid-transfer on.
//!
//! ## Two priority classes, on purpose
//!
//! Control traffic ([`UsbMsg::CONTROL_PRIORITY`]) is small and rare, and must
//! not queue behind the stream it is trying to end: a release stuck behind a
//! megabyte of pending bulk transfer detaches seconds after the user asked. The
//! URB stream itself is [`UsbMsg::DATA_PRIORITY`] — `Bulk`, the honest class
//! for throughput work with no deadline, so copying off a USB disk is never
//! felt as mouse lag.

use serde::{Deserialize, Serialize};

use crate::PeerId;

/// Identifies one device for as long as it stays plugged in.
///
/// Deliberately not the host's bus id: `3-2` is meaningless on another machine,
/// and it changes when the user moves the cable to a different port — so a
/// holder would find its claim silently pointing at whatever else landed there.
/// The host mints these and maps them back to a bus id itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UsbDeviceId(pub u64);

impl std::fmt::Display for UsbDeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "usb:{}", self.0)
    }
}

/// One attach, from claim to release.
///
/// Separate from [`UsbDeviceId`] because the same device may be claimed,
/// released and claimed again within one session: late data from the first
/// attach must not be mistaken for data belonging to the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbStreamId(pub u64);

/// How well a device is expected to survive being carried over a network.
///
/// Stated in the listing rather than discovered as flakiness. USB/IP carries
/// transfers over TCP, and the transfer types differ in how much they mind:
/// bulk and control retry and have no per-packet deadline, while isochronous
/// endpoints have a deadline and no retransmission at all. A webcam attached
/// over a LAN does not work badly by accident — it cannot work well, and the
/// user deserves to hear that before they try rather than after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbTransport {
    /// Control, bulk and interrupt endpoints only. Phones and `adb`, mass
    /// storage, programmers, printers, security keys.
    Fine,
    /// Has isochronous endpoints — a camera or an audio interface. Attachable,
    /// but the user is told first.
    Degraded,
}

/// A device on some machine's port, as advertised to the mesh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDevice {
    pub id: UsbDeviceId,
    /// USB vendor and product ids, for recognising the device when it has no
    /// strings to offer — plenty of them do not.
    pub vendor: u16,
    pub product: u16,
    /// What the device calls itself, when it says. Never synthesised from the
    /// ids: a made-up name is indistinguishable from a real one to the user.
    pub manufacturer: Option<String>,
    pub description: Option<String>,
    /// USB class code of the device or its first interface, for an icon and for
    /// the "this is a webcam" judgement behind [`UsbTransport::Degraded`].
    pub class: u8,
    /// How the host expects it to fare over the link.
    pub transport: UsbTransport,
    /// Who has it attached right now, if anyone. Carried in the listing itself
    /// so a machine can show "held by desktop" without a second round trip —
    /// and so a claim is refused locally, before it costs one.
    pub holder: Option<PeerId>,
}

impl UsbDevice {
    /// A name for logs and for the device list, in the order a user would
    /// recognise it: what it calls itself, else what it is numbered.
    pub fn label(&self) -> String {
        match (&self.manufacturer, &self.description) {
            (Some(m), Some(d)) => format!("{m} {d}"),
            (None, Some(d)) => d.clone(),
            (Some(m), None) => m.clone(),
            (None, None) => format!("{:04x}:{:04x}", self.vendor, self.product),
        }
    }
}

/// Why a claim could not be granted, or why an attach ended.
///
/// Errors are values, never silence — the rule the files module already
/// follows. A claim that goes unanswered leaves the user's `adb` hanging with
/// no way to tell "someone else has it" from "the driver isn't installed", and
/// the two have completely different fixes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbError {
    /// Another peer holds it. Naming who is the difference between a dead end
    /// and asking that person to let go.
    InUse(PeerId),
    /// No such device — unplugged, or a stale id from before a reconnect.
    Gone,
    /// The host is not offering this device. Sharing is opt-in per machine, and
    /// a device may be excluded deliberately.
    Denied,
    /// The kernel side is missing: modules not loaded on Linux, or `usbipd-win`
    /// not installed on Windows. Actionable, so it says which.
    DriverMissing(String),
    /// The host could not get the privilege to bind. Exporting rebinds a device
    /// away from its driver, which is a root/administrator operation.
    NeedsPrivilege,
    /// The device was physically removed while attached.
    Unplugged,
    /// The host's USB/IP layer failed, in its own words.
    Backend(String),
}

impl std::fmt::Display for UsbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InUse(peer) => write!(f, "already attached by {peer}"),
            Self::Gone => f.write_str("no such device"),
            Self::Denied => f.write_str("not offered by its host"),
            Self::DriverMissing(what) => write!(f, "usb/ip support missing: {what}"),
            Self::NeedsPrivilege => f.write_str("binding the device needs elevated privilege"),
            Self::Unplugged => f.write_str("device was unplugged"),
            Self::Backend(why) => f.write_str(why),
        }
    }
}

/// The USB module's wire vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbMsg {
    /// Host -> peers: every device this machine offers.
    ///
    /// The whole list, always, so a reconnecting peer resyncs by receiving the
    /// current one rather than replaying arrivals and departures it may have
    /// missed while away (rule 5 in `docs/platform-design.md`). Sent on start
    /// and whenever the set or a device's holder changes.
    Devices(Vec<UsbDevice>),

    /// Peer -> host: send me your device list.
    ///
    /// Distinct from an empty [`UsbMsg::Devices`], which is a host announcing it
    /// has nothing. The files module hit exactly this bug by overloading one
    /// message for both, and the request wiped the list it meant to fetch.
    ListDevices,

    /// Peer -> host: attach this device to me.
    ///
    /// `reason` is what the user was doing, in their words, so the machine
    /// physically holding the device can say *why* its phone just went away —
    /// otherwise that is an unexplained disconnect on someone else's desk.
    Claim { device: UsbDeviceId, reason: String },

    /// Host -> claimer: it is yours, and here is the stream carrying it.
    Claimed {
        device: UsbDeviceId,
        stream: UsbStreamId,
    },

    /// Host -> claimer: no, and why. The claimer stops waiting.
    Refused {
        device: UsbDeviceId,
        error: UsbError,
    },

    /// Holder -> host: I am done with it.
    ///
    /// Only the holder may send this. A release from anyone else is ignored
    /// rather than honoured: detaching a bus out from under whoever is actually
    /// using it is worse than refusing to help.
    Release { device: UsbDeviceId },

    /// Host -> holder: the attach has ended, and this is the last word on it.
    ///
    /// Sent for an orderly [`UsbMsg::Release`] and for an unplug alike, because
    /// the holder's kernel needs the detach either way and the difference is
    /// only what `error` says.
    Detached {
        device: UsbDeviceId,
        stream: UsbStreamId,
        /// `None` when the holder asked; `Some` when the host ended it.
        error: Option<UsbError>,
    },

    /// The USB/IP byte stream, in both directions.
    ///
    /// Opaque by design: these bytes mean something to two kernels and nothing
    /// to this process. Framing within the stream belongs to USB/IP, so a
    /// payload here is whatever arrived — never reassembled or interpreted.
    Data { stream: UsbStreamId, bytes: Vec<u8> },
}

impl UsbMsg {
    /// Scheduling class for claims, releases and the device list.
    ///
    /// `Control`, deliberately above the data stream it governs. A release
    /// queued behind the bulk traffic of the attach it is ending would detach
    /// seconds after the user asked — and worse, a revocation for an unplugged
    /// device would arrive after the holder's kernel had already been left
    /// waiting on transfers that can never complete.
    pub const CONTROL_PRIORITY: crate::Priority = crate::Priority::Control;

    /// Scheduling class for the URB stream.
    ///
    /// `Bulk`, the same honest class the files module takes: reading a USB disk
    /// is throughput work with no deadline, while the cursor sharing this
    /// channel has one. A device whose transfers genuinely have deadlines is
    /// the isochronous case, which does not work over a network anyway and is
    /// reported as [`UsbTransport::Degraded`] rather than given a priority it
    /// would have to take from input.
    pub const DATA_PRIORITY: crate::Priority = crate::Priority::Bulk;

    /// Largest URB payload carried in one [`UsbMsg::Data`].
    ///
    /// Well under [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN) so a chunk plus its
    /// envelope always fits, and small enough that a bulk transfer yields to
    /// higher-priority traffic often rather than holding the link between
    /// scheduling points.
    pub const MAX_CHUNK: usize = 256 * 1024;

    /// Which class this message rides in. Stated per message because the
    /// envelope carries the priority explicitly — the scheduler ranks a frame
    /// without decoding it, so it has nothing else to go on.
    pub fn priority(&self) -> crate::Priority {
        match self {
            Self::Data { .. } => Self::DATA_PRIORITY,
            _ => Self::CONTROL_PRIORITY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: u64) -> UsbDevice {
        UsbDevice {
            id: UsbDeviceId(id),
            vendor: 0x18d1,
            product: 0x4ee7,
            manufacturer: Some("Google".into()),
            description: Some("Pixel".into()),
            class: 0,
            transport: UsbTransport::Fine,
            holder: None,
        }
    }

    /// A full URB chunk plus its envelope has to fit in one frame, or a
    /// transfer of exactly `MAX_CHUNK` bytes could never be delivered.
    #[test]
    fn a_full_chunk_fits_in_a_frame() {
        // A `const` block, so the two constants are checked when the crate is
        // built rather than when someone runs the tests: a chunk larger than a
        // frame is not a failing test, it is a build that should not have
        // happened.
        const {
            assert!(
                UsbMsg::MAX_CHUNK < crate::MAX_FRAME_LEN,
                "a urb chunk must leave room for its envelope"
            )
        };
        let msg = UsbMsg::Data {
            stream: UsbStreamId(1),
            bytes: vec![0u8; UsbMsg::MAX_CHUNK],
        };
        let encoded = bincode::serialize(&msg).expect("serialises");
        assert!(
            encoded.len() < crate::MAX_FRAME_LEN,
            "a full chunk plus overhead ({}) must fit in a frame ({})",
            encoded.len(),
            crate::MAX_FRAME_LEN
        );
    }

    /// The URB stream must never outrank the cursor.
    #[test]
    fn device_traffic_yields_to_input() {
        assert!(
            UsbMsg::DATA_PRIORITY < crate::Priority::Input,
            "bulk urb traffic must not preempt input"
        );
        assert!(
            UsbMsg::DATA_PRIORITY < crate::Priority::Audio,
            "a usb disk read must not interrupt audio"
        );
    }

    /// A release must be able to overtake the stream it is ending, or a detach
    /// waits out the very transfer the user is trying to stop.
    #[test]
    fn a_release_outranks_the_stream_it_ends() {
        let release = UsbMsg::Release {
            device: UsbDeviceId(1),
        };
        let data = UsbMsg::Data {
            stream: UsbStreamId(1),
            bytes: vec![0u8; 16],
        };
        assert!(
            release.priority() > data.priority(),
            "control traffic must overtake the urb stream"
        );
    }

    /// The device list survives a round trip whole — it is the resync path, so
    /// a peer that reconnects must see exactly what the host has.
    #[test]
    fn a_device_list_round_trips() {
        let msg = UsbMsg::Devices(vec![device(1), device(2)]);
        let bytes = bincode::serialize(&msg).expect("serialises");
        let back: UsbMsg = bincode::deserialize(&bytes).expect("deserialises");
        assert_eq!(back, msg);
    }

    /// A device with nothing to say about itself still needs a name a user can
    /// tell apart from another such device.
    #[test]
    fn a_nameless_device_falls_back_to_its_ids() {
        let mut d = device(1);
        d.manufacturer = None;
        d.description = None;
        assert_eq!(d.label(), "18d1:4ee7");
    }

    /// Every refusal names its cause: a claim that fails without one leaves the
    /// user unable to tell a busy device from a missing driver.
    #[test]
    fn refusals_carry_a_readable_cause() {
        let peer = PeerId::from_bytes([7u8; 16]);
        for error in [
            UsbError::InUse(peer),
            UsbError::Gone,
            UsbError::Denied,
            UsbError::DriverMissing("vhci-hcd not loaded".into()),
            UsbError::NeedsPrivilege,
            UsbError::Unplugged,
            UsbError::Backend("bind failed".into()),
        ] {
            assert!(
                !error.to_string().is_empty(),
                "every refusal must say something"
            );
        }
    }
}
