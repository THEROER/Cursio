//! Reading a USB/IP header, and nothing more.
//!
//! The arbiter has to tell a descriptor read from a bulk transfer from a
//! request that reconfigures the device, and route each reply back to whoever
//! asked. All of that is in a fixed 48-byte header; none of it needs a USB
//! stack, and this module deliberately cannot parse a payload.
//!
//! ## Layout
//!
//! From the kernel's own [protocol documentation]. Every field is 4 bytes, big
//! endian, and the basic header is the first 20:
//!
//! ```text
//! 0x00 command     0x0001 CMD_SUBMIT   0x0003 RET_SUBMIT
//!                  0x0002 CMD_UNLINK   0x0004 RET_UNLINK
//! 0x04 seqnum      matches a reply to its request
//! 0x08 devid
//! 0x0C direction   0 = OUT (to device), 1 = IN (to host)
//! 0x10 ep          endpoint number; 0 is the control endpoint
//! ---- basic header ends
//! 0x14 transfer_flags
//! 0x18 transfer_buffer_length
//! 0x1C start_frame
//! 0x20 number_of_packets   not -1 only for isochronous transfers
//! 0x24 interval
//! 0x28 setup       8 bytes, meaningful when ep is 0
//! ```
//!
//! Two directions are described here and they are not the same field:
//! `direction` at 0x0C is the URB's own, while `transfer_flags` carries
//! `USBIP_URB_DIR_IN` as a bit. The one at 0x0C is what the kernel routes on,
//! so it is what this reads.
//!
//! [protocol documentation]: https://docs.kernel.org/usb/usbip_protocol.html

use crate::UsbBackendError;

/// Length of the fixed header on every USB/IP message.
///
/// A submission carries its payload after this; a header shorter than this is
/// a framing error rather than a short read, because the sender always writes
/// all of it in one go.
pub const HEADER_LEN: usize = 0x30;

/// Offset of the 8 setup bytes within the header.
const SETUP_OFFSET: usize = 0x28;

/// What a USB/IP message is.
///
/// The four the protocol defines; anything else is a framing error, since a
/// stream that has lost sync produces garbage here long before it produces a
/// plausible transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// A host submitting a transfer.
    Submit,
    /// A host cancelling one it submitted.
    Unlink,
    /// The device's answer to a submission.
    SubmitReply,
    /// The answer to a cancellation.
    UnlinkReply,
}

impl Command {
    fn from_wire(value: u32) -> Option<Self> {
        match value {
            0x0000_0001 => Some(Self::Submit),
            0x0000_0002 => Some(Self::Unlink),
            0x0000_0003 => Some(Self::SubmitReply),
            0x0000_0004 => Some(Self::UnlinkReply),
            _ => None,
        }
    }
}

/// Which way a transfer moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// To the device.
    Out,
    /// To the host.
    In,
}

/// The eight setup bytes of a control transfer.
///
/// Only meaningful on endpoint 0. Named fields rather than a byte array because
/// every decision the arbiter makes reads one of them, and `setup[1]` at each
/// call site is how the wrong byte gets compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    /// Direction, type and recipient, packed as USB defines them.
    pub request_type: u8,
    /// The request itself — `USB_REQ_*` in `linux/usb/ch9.h`.
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    /// Whether this is a standard request rather than a class or vendor one.
    ///
    /// Only standard requests are understood here. A vendor request may do
    /// anything at all, including reconfiguring the device, so it is never
    /// answered from cache — it is passed through and counted as state-changing.
    pub fn is_standard(self) -> bool {
        // USB_TYPE_MASK is (0x03 << 5); USB_TYPE_STANDARD is 0.
        self.request_type & (0x03 << 5) == 0
    }

    /// Whether the device is being read rather than written.
    ///
    /// `USB_DIR_IN` is the top bit of `bmRequestType`.
    pub fn reads_device(self) -> bool {
        self.request_type & 0x80 != 0
    }
}

/// A parsed USB/IP header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub command: Command,
    /// Matches a reply to its request, per connection. The arbiter rewrites
    /// this: several hosts number their transfers independently, so two of them
    /// will use the same seqnum for different work.
    pub seqnum: u32,
    pub devid: u32,
    pub direction: Direction,
    /// Endpoint number. Zero is the control endpoint, where enumeration and
    /// configuration happen.
    pub ep: u32,
    pub transfer_flags: u32,
    /// Bytes of payload this message carries or expects.
    pub transfer_buffer_length: u32,
    /// Isochronous packet count. `0` for every other transfer type, which is
    /// how an isochronous transfer is recognised without a descriptor.
    pub number_of_packets: u32,
    /// The control request, when `ep` is 0.
    pub setup: Setup,
}

impl Header {
    /// Read a header from the first [`HEADER_LEN`] bytes of a message.
    pub fn parse(bytes: &[u8]) -> Result<Self, UsbBackendError> {
        if bytes.len() < HEADER_LEN {
            return Err(UsbBackendError::Framing(format!(
                "a usb/ip header is {HEADER_LEN} bytes, got {}",
                bytes.len()
            )));
        }
        let word = |offset: usize| -> u32 {
            u32::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ])
        };

        let raw = word(0x00);
        let Some(command) = Command::from_wire(raw) else {
            return Err(UsbBackendError::Framing(format!(
                "unknown usb/ip command {raw:#010x}"
            )));
        };

        let s = &bytes[SETUP_OFFSET..SETUP_OFFSET + 8];
        Ok(Self {
            command,
            seqnum: word(0x04),
            devid: word(0x08),
            direction: match word(0x0c) {
                0 => Direction::Out,
                _ => Direction::In,
            },
            ep: word(0x10),
            transfer_flags: word(0x14),
            transfer_buffer_length: word(0x18),
            number_of_packets: word(0x20),
            setup: Setup {
                request_type: s[0],
                request: s[1],
                // The setup packet is little endian, unlike the header around
                // it: these bytes are the device's own, passed through
                // untouched by USB/IP, and USB is a little-endian bus.
                value: u16::from_le_bytes([s[2], s[3]]),
                index: u16::from_le_bytes([s[4], s[5]]),
                length: u16::from_le_bytes([s[6], s[7]]),
            },
        })
    }

    /// Whether this is a control transfer — endpoint 0, where enumeration and
    /// configuration both happen.
    pub fn is_control(&self) -> bool {
        self.ep == 0
    }

    /// Whether this transfer has a per-frame deadline the network cannot meet.
    ///
    /// Isochronous transfers are the ones with no retransmission, recognised by
    /// carrying packet descriptors. Reported rather than refused: the user is
    /// told before attaching a webcam, not after it stutters.
    pub fn is_isochronous(&self) -> bool {
        self.number_of_packets > 0
    }

    /// Replace the sequence number, for forwarding a transfer under the
    /// arbiter's own numbering.
    ///
    /// Two hosts number independently and will collide; the arbiter issues its
    /// own seqnum to the device and maps the reply back. `bytes` must be a
    /// header this was parsed from.
    pub fn write_seqnum(bytes: &mut [u8], seqnum: u32) {
        bytes[0x04..0x08].copy_from_slice(&seqnum.to_be_bytes());
    }

    /// Build the reply this transfer would have got from the device.
    ///
    /// What turns an arbiter verdict into something a kernel accepts. A host
    /// that submitted a transfer is waiting on `RET_SUBMIT` for that seqnum and
    /// will wait forever without one — so answering from cache, reporting a
    /// state already in effect, and refusing all have to produce a real reply
    /// rather than silence.
    ///
    /// `status` is a kernel errno, negated, exactly as `usbip` carries it: `0`
    /// succeeded, and `-EPIPE` (`-32`) is how USB says a request was refused —
    /// the same stall a device answers with, so a host needs no special case
    /// for a transfer Cursio declined.
    ///
    /// Isochronous descriptors are never appended: this only answers control
    /// transfers, which have none.
    pub fn reply(seqnum: u32, devid: u32, status: i32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0x00..0x04].copy_from_slice(&RET_SUBMIT.to_be_bytes());
        bytes[0x04..0x08].copy_from_slice(&seqnum.to_be_bytes());
        bytes[0x08..0x0c].copy_from_slice(&devid.to_be_bytes());
        // Direction and endpoint are the submitter's business, and the kernel
        // ignores them on a reply: it matches on seqnum alone.
        bytes[0x14..0x18].copy_from_slice(&status.to_be_bytes());
        // `actual_length` shares the offset `transfer_buffer_length` has on a
        // submission — a reply says how much came back, not how much was asked
        // for, and a host that reads more than this gets somebody else's bytes.
        bytes[0x18..0x1c].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        // `number_of_packets` is -1 on a reply to a non-isochronous transfer.
        bytes[0x20..0x24].copy_from_slice(&(-1i32).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }
}

/// A reply to a submission, as the wire numbers it.
const RET_SUBMIT: u32 = 0x0000_0003;

/// What USB calls a stalled endpoint, which is how a refusal is reported.
///
/// A host asked for something another machine's lease forbids. `-EPIPE` is what
/// the device itself would answer if it declined, so nothing on the host needs
/// to know Cursio was involved.
pub const STATUS_STALL: i32 = -32;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a header the way a host would put one on the wire.
    fn header(command: u32, ep: u32, direction: u32, setup: [u8; 8]) -> Vec<u8> {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0x00..0x04].copy_from_slice(&command.to_be_bytes());
        bytes[0x04..0x08].copy_from_slice(&7u32.to_be_bytes());
        bytes[0x08..0x0c].copy_from_slice(&0x0001_0002u32.to_be_bytes());
        bytes[0x0c..0x10].copy_from_slice(&direction.to_be_bytes());
        bytes[0x10..0x14].copy_from_slice(&ep.to_be_bytes());
        bytes[0x18..0x1c].copy_from_slice(&64u32.to_be_bytes());
        bytes[SETUP_OFFSET..SETUP_OFFSET + 8].copy_from_slice(&setup);
        bytes
    }

    /// A host waits on `RET_SUBMIT` for the seqnum it submitted, so a verdict
    /// answered locally has to look exactly like one the device produced.
    #[test]
    fn a_local_reply_carries_the_seqnum_and_length_a_host_waits_for() {
        let reply = Header::reply(7, 0x0001_0002, 0, b"descriptor");

        assert_eq!(&reply[0x00..0x04], &3u32.to_be_bytes(), "RET_SUBMIT");
        assert_eq!(&reply[0x04..0x08], &7u32.to_be_bytes(), "the host's seqnum");
        assert_eq!(&reply[0x14..0x18], &0i32.to_be_bytes(), "succeeded");
        assert_eq!(
            &reply[0x18..0x1c],
            &10u32.to_be_bytes(),
            "actual_length, not what was asked for"
        );
        assert_eq!(
            &reply[0x20..0x24],
            &(-1i32).to_be_bytes(),
            "not an isochronous transfer"
        );
        assert_eq!(&reply[HEADER_LEN..], b"descriptor");
    }

    /// A refusal is a stall, which is what the device itself would answer —
    /// so a host needs no special case for a transfer Cursio declined.
    #[test]
    fn a_refusal_is_reported_as_a_stall_with_no_payload() {
        let reply = Header::reply(9, 0, STATUS_STALL, &[]);

        assert_eq!(&reply[0x14..0x18], &(-32i32).to_be_bytes());
        assert_eq!(&reply[0x18..0x1c], &0u32.to_be_bytes());
        assert_eq!(reply.len(), HEADER_LEN, "a stall carries nothing back");
    }

    /// And what it builds is what it can read back: the reply is a header like
    /// any other, so a parser on the far end has to accept it.
    #[test]
    fn a_built_reply_parses_as_the_reply_it_claims_to_be() {
        let reply = Header::reply(7, 0x0001_0002, 0, b"abc");
        let parsed = Header::parse(&reply).expect("a reply must parse");

        assert_eq!(parsed.command, Command::SubmitReply);
        assert_eq!(parsed.seqnum, 7);
        assert_eq!(parsed.devid, 0x0001_0002);
    }

    /// `GET_DESCRIPTOR` for the device descriptor: the first thing every host
    /// asks, and the request the cache exists to answer.
    fn get_descriptor() -> [u8; 8] {
        // bmRequestType 0x80 (IN, standard, device), bRequest 0x06,
        // wValue 0x0100 (device descriptor, index 0), wIndex 0, wLength 18.
        [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]
    }

    #[test]
    fn reads_every_field_of_a_submission() {
        let bytes = header(0x0000_0001, 0, 1, get_descriptor());
        let h = Header::parse(&bytes).expect("parses");

        assert_eq!(h.command, Command::Submit);
        assert_eq!(h.seqnum, 7);
        assert_eq!(h.devid, 0x0001_0002);
        assert_eq!(h.direction, Direction::In);
        assert_eq!(h.ep, 0);
        assert_eq!(h.transfer_buffer_length, 64);
    }

    /// The header is big endian and the setup packet inside it is not. Reading
    /// both the same way turns `wValue` 0x0100 — the device descriptor — into
    /// 0x0001, which is no descriptor at all.
    #[test]
    fn the_setup_packet_is_little_endian_inside_a_big_endian_header() {
        let bytes = header(0x0000_0001, 0, 1, get_descriptor());
        let h = Header::parse(&bytes).expect("parses");

        assert_eq!(h.setup.request, 0x06, "GET_DESCRIPTOR");
        assert_eq!(h.setup.value, 0x0100, "device descriptor, index 0");
        assert_eq!(h.setup.length, 18);
    }

    #[test]
    fn tells_the_four_commands_apart() {
        for (raw, expected) in [
            (0x0000_0001, Command::Submit),
            (0x0000_0002, Command::Unlink),
            (0x0000_0003, Command::SubmitReply),
            (0x0000_0004, Command::UnlinkReply),
        ] {
            let bytes = header(raw, 0, 1, [0; 8]);
            assert_eq!(Header::parse(&bytes).expect("parses").command, expected);
        }
    }

    /// A stream that has lost sync produces a nonsense command long before it
    /// produces a plausible transfer, so this is where desync is caught.
    #[test]
    fn an_unknown_command_is_a_framing_error() {
        let bytes = header(0x0000_00ff, 0, 1, [0; 8]);
        assert!(matches!(
            Header::parse(&bytes),
            Err(UsbBackendError::Framing(_))
        ));
    }

    /// A header split across two reads must be reported as such rather than
    /// parsed from whatever happened to follow it in memory.
    #[test]
    fn a_short_header_is_refused() {
        let bytes = header(0x0000_0001, 0, 1, get_descriptor());
        assert!(matches!(
            Header::parse(&bytes[..HEADER_LEN - 1]),
            Err(UsbBackendError::Framing(_))
        ));
    }

    /// Endpoint 0 is where enumeration and configuration both happen, so it is
    /// the only endpoint the arbiter inspects further.
    #[test]
    fn endpoint_zero_is_the_control_endpoint() {
        assert!(Header::parse(&header(1, 0, 1, get_descriptor()))
            .expect("parses")
            .is_control());
        assert!(!Header::parse(&header(1, 2, 1, [0; 8]))
            .expect("parses")
            .is_control());
    }

    /// Isochronous transfers carry packet descriptors; nothing else does. That
    /// is how they are recognised without reading an endpoint descriptor.
    #[test]
    fn packet_descriptors_mark_an_isochronous_transfer() {
        let mut bytes = header(1, 1, 1, [0; 8]);
        assert!(!Header::parse(&bytes).expect("parses").is_isochronous());

        bytes[0x20..0x24].copy_from_slice(&8u32.to_be_bytes());
        assert!(Header::parse(&bytes).expect("parses").is_isochronous());
    }

    /// A standard request is one the arbiter understands. A vendor request may
    /// do anything, so it must not be mistaken for one.
    #[test]
    fn a_vendor_request_is_not_standard() {
        let standard = Setup {
            request_type: 0x80,
            request: 0x06,
            value: 0x0100,
            index: 0,
            length: 18,
        };
        assert!(standard.is_standard());
        assert!(standard.reads_device());

        // 0x40: host-to-device, vendor type.
        let vendor = Setup {
            request_type: 0x40,
            ..standard
        };
        assert!(!vendor.is_standard());
        assert!(!vendor.reads_device());
    }

    /// Two hosts number their transfers independently, so the arbiter renumbers
    /// before forwarding. Rewriting must touch nothing else in the header.
    #[test]
    fn renumbering_changes_only_the_sequence_number() {
        let mut bytes = header(0x0000_0001, 0, 1, get_descriptor());
        let before = Header::parse(&bytes).expect("parses");

        Header::write_seqnum(&mut bytes, 4242);
        let after = Header::parse(&bytes).expect("parses");

        assert_eq!(after.seqnum, 4242);
        assert_eq!(
            Header {
                seqnum: before.seqnum,
                ..after
            },
            before,
            "no other field may move"
        );
    }
}
