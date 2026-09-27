//! Who may attach a device, and who holds it now.
//!
//! The policy half of [`cursio_protocol::usb`]. The mechanism — binding a
//! device away from its driver, carrying URBs — is USB/IP in the kernel; what
//! lives here is the part USB/IP has no answer for: which peer is allowed to
//! take a device, and what happens when two ask at once.
//!
//! ## Two roles, deliberately not one
//!
//! The **host** owns the physical port and is the authority on what exists. The
//! **holder** currently has the device attached. They are usually different
//! machines, and conflating them is how a detach gets honoured from the wrong
//! side: a release is only meaningful from the machine actually using the
//! device, while a revocation is only meaningful from the machine it is plugged
//! into.
//!
//! So every state-changing message is checked against the sender's role before
//! anything happens, and a message from the wrong role is refused rather than
//! obeyed. This is the same rule the shared cursor arrived at the hard way —
//! only the machine holding it may send it away — and the failure here is
//! worse: a stray release detaches a bus a program is mid-transfer on.
//!
//! ## The host decides, always
//!
//! Both machines could track ownership and stay in agreement, and that is
//! exactly the race worth avoiding: two peers claiming the same device at the
//! same moment each see it free. The host is the single writer. A claim is a
//! *request*, never an assertion, and `holder` in the device list is the host's
//! answer — which is why the list carries it rather than each peer inferring it.
//!
//! Rule 4 in `docs/platform-design.md` ("one owner per piece of state"), where
//! the owner is the machine with the port.

use std::collections::HashMap;

use async_trait::async_trait;
use cursio_protocol::usb::{UsbDevice, UsbDeviceId, UsbError, UsbMsg, UsbStreamId};
use cursio_protocol::{Message, ModuleId, PeerId};
use cursio_transport::{TransportError, TransportTx};

use crate::module::SessionModule;

/// Version of this module's wire vocabulary ([`UsbMsg`]). Bump when those types
/// change shape; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

/// How often a peer is asked again for devices it would not lend.
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How often this machine's own ports are re-read.
///
/// Short enough that plugging a phone in feels immediate, long enough to be
/// free: on Linux this is two directory reads, and on Windows a `usbipd state`
/// that takes about a tenth of a second. The pump runs far more often than
/// this, so the interval is what keeps the cost off the session loop.
const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// What the local machine can actually do with its ports.
///
/// A trait so the ownership rules can be tested without a kernel: every race
/// worth getting right is decided here, and none of them need a real device.
/// The implementations are `crates/usb`.
pub trait UsbBackend: Send {
    /// Devices this machine is offering right now.
    fn devices(&mut self) -> Vec<UsbDevice>;

    /// Export a device so a peer can attach it, yielding the stream that will
    /// carry its URBs. Takes the device away from this machine's own drivers.
    fn export(&mut self, device: UsbDeviceId) -> Result<UsbStreamId, UsbError>;

    /// Stop exporting, returning the device to this machine.
    fn unexport(&mut self, device: UsbDeviceId);

    /// Hand URB bytes from the peer to the local USB/IP layer.
    fn feed(&mut self, stream: UsbStreamId, bytes: &[u8]) -> Result<(), UsbError>;

    /// Take whatever the local USB/IP layer has produced since the last call.
    ///
    /// Must not block: this runs inside the session pump, which the shared
    /// cursor also rides. An idle device — the common case — answers with
    /// nothing.
    ///
    /// A stream that has ended is reported by its absence from the result *and*
    /// by [`ended`](Self::ended), because the two mean different things to the
    /// engine: no bytes is an idle device, while an ended stream is an
    /// attachment to tear down.
    fn collect(&mut self, out: &mut Vec<(UsbStreamId, Vec<u8>)>);

    /// Streams whose kernel side has closed since the last call.
    ///
    /// Separate from [`collect`](Self::collect) so the engine can send the last
    /// bytes of a transfer before the detach that follows them — a close and
    /// the data preceding it arrive together, and reversing them would drop the
    /// end of every transfer.
    fn ended(&mut self) -> Vec<UsbStreamId> {
        Vec::new()
    }

    /// Whether this machine can take a peer's device at all.
    ///
    /// The importing half is a separate kernel module from the exporting one
    /// (`vhci-hcd`, not `usbip-host`) and is loaded independently, so a machine
    /// can perfectly well share its own ports while being unable to accept
    /// anyone else's. Asked before attaching rather than discovered as a
    /// failure per device.
    fn can_attach(&self) -> bool {
        false
    }

    /// Make a peer's device appear on this machine.
    ///
    /// Returns the socket the peer's URBs should be written into, as far as
    /// this side is concerned: the engine feeds it with [`feed_attached`] and
    /// drains it with [`collect_attached`].
    ///
    /// [`feed_attached`]: Self::feed_attached
    /// [`collect_attached`]: Self::collect_attached
    fn attach(&mut self, _device: UsbDeviceId, _stream: UsbStreamId) -> Result<(), UsbError> {
        Err(UsbError::Backend("attaching is not supported here".into()))
    }

    /// Stop presenting a peer's device locally.
    fn detach(&mut self, _stream: UsbStreamId) {}

    /// Hand bytes from the peer's device to the local virtual controller.
    fn feed_attached(&mut self, _stream: UsbStreamId, _bytes: &[u8]) -> Result<(), UsbError> {
        Err(UsbError::Gone)
    }

    /// Take what the local virtual controller wants to send the peer's device.
    fn collect_attached(&mut self, _out: &mut Vec<(UsbStreamId, Vec<u8>)>) {}
}

/// The ownership rules, independent of transport and of any kernel.
pub struct UsbEngine {
    peer_name: String,
    /// Which peer this session talks to. Every claim arriving here came from
    /// this machine, so a claim never has to say who it is from — and cannot
    /// lie about it.
    peer_id: PeerId,
    /// This machine's ports, shared with every other link.
    ///
    /// Not owned here: a device belongs to the machine, and a session belongs
    /// to one peer. Held per link, the exports and the arbiter's cache would
    /// each be a private copy of state two peers must agree on — see
    /// [`usb_node`](crate::usb_node).
    node: std::sync::Arc<std::sync::Mutex<crate::usb_node::UsbNode>>,
    /// Queued for the next pump, because `handle_message` is synchronous while
    /// sending is not.
    outbound: Vec<UsbMsg>,
    /// Whether the device list has gone out on this link yet.
    announced: bool,
    /// Devices of *this peer* that are now present on this machine, keyed by
    /// the peer's device id. The mirror image of the node's exports: those are
    /// what we lent out, this is what we borrowed.
    borrowed: HashMap<UsbDeviceId, UsbStreamId>,
    /// Peer devices asked for but not yet answered, so a re-announced listing
    /// does not ask twice for the same device.
    asking: std::collections::HashSet<UsbDeviceId>,
    /// The peer's last listing, kept so refused devices can be asked for again
    /// without waiting for something on that machine to change.
    ///
    /// A refusal is rarely permanent — a prompt gets accepted, a holder lets
    /// go, a driver is installed — but the *listing* only travels when the
    /// peer's own device set changes, which a granted permission is not. Left
    /// alone, a device refused once would never be asked for again this
    /// session.
    offered: Vec<UsbDevice>,
    /// When the refused devices were last retried.
    last_retry: Option<std::time::Instant>,
    /// The listing last sent, and when. A device plugged in after the session
    /// started is invisible until this notices it.
    sent: Vec<UsbDevice>,
    last_scan: Option<std::time::Instant>,
    /// Peer devices this machine could not present, and when it last tried.
    ///
    /// Without this, a failed attach loops: the release frees the device on the
    /// peer, the peer re-announces because its listing changed, and the fresh
    /// listing asks again — several times a second, forever. The failures worth
    /// retrying at all (a missing module gets loaded, a permission granted) are
    /// slow, so they wait for [`RETRY_INTERVAL`] like any other refusal.
    failed: HashMap<UsbDeviceId, std::time::Instant>,
}

impl UsbEngine {
    pub fn new(
        peer_name: String,
        peer_id: PeerId,
        node: std::sync::Arc<std::sync::Mutex<crate::usb_node::UsbNode>>,
    ) -> Self {
        Self {
            peer_name,
            peer_id,
            node,
            outbound: Vec::new(),
            announced: false,
            borrowed: HashMap::new(),
            asking: std::collections::HashSet::new(),
            offered: Vec::new(),
            last_retry: None,
            failed: HashMap::new(),
            sent: Vec::new(),
            last_scan: None,
        }
    }

    /// Whether a stream belongs to this link — either lent to this peer or
    /// borrowed from it.
    ///
    /// Streams are numbered per machine and carried per link, so this is what
    /// keeps one session from sending another's traffic.
    fn owns_stream(&self, stream: UsbStreamId) -> bool {
        if self.borrowed.values().any(|held| *held == stream) {
            return true;
        }
        // One lock for both questions: `Mutex` is not reentrant, so taking it
        // twice in one expression would deadlock this session against itself.
        let node = self.node();
        node.device_of_stream(stream)
            .is_some_and(|device| node.holds(device, self.peer_id, stream))
    }

    /// How many devices this machine is serving, for tests that assert a
    /// device was given back.
    #[cfg(test)]
    fn exported_count(&self) -> usize {
        self.node().exported_count()
    }

    /// This machine's ports, locked for the length of one call.
    ///
    /// Every caller here is inside the session pump, and the node's own work is
    /// a few map lookups plus whatever the backend does — so the lock is never
    /// held across an await and cannot make one link wait on another's I/O.
    ///
    /// A poisoned lock is taken anyway: the state behind it is a record of what
    /// the kernel has, and refusing to read it because some other link panicked
    /// would strand every exported device on this machine.
    fn node(&self) -> std::sync::MutexGuard<'_, crate::usb_node::UsbNode> {
        self.node.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The device list as this peer should see it: every device, each carrying
    /// who holds it.
    ///
    /// Built from the backend's view each time rather than cached, so a device
    /// unplugged between announcements simply is not in the next one.
    fn listing(&mut self) -> Vec<UsbDevice> {
        self.node().listing()
    }

    /// Answer a peer asking for a device.
    ///
    /// **Granted to any number of peers at once.** A second machine asking is
    /// not a competitor: enumeration is answered from the arbiter's descriptor
    /// cache, so it costs the device nothing and disturbs nobody already using
    /// it. What machines contend for is the device's *configuration*, and that
    /// is arbitrated per request rather than by refusing the attach —
    /// `docs/usb-design.md`, "Ownership, restated".
    ///
    /// So `Refused` is now rare: a device that is gone, or a kernel that
    /// cannot export. Neither is somebody else's turn.
    fn on_claim(&mut self, device: UsbDeviceId, reason: &str) {
        let attached = self
            .node()
            .attach_peer(device, self.peer_id, &self.peer_name);
        match attached {
            Ok(stream) => {
                tracing::info!(
                    peer = %self.peer_name, %device, %reason,
                    "handing a device to a peer"
                );
                self.outbound.push(UsbMsg::Claimed { device, stream });
                // The listing carries who is using the device, so it is stale
                // the moment somebody starts.
                self.announced = false;
            }
            Err(error) => {
                tracing::warn!(peer = %self.peer_name, %device, %error, "refused a device");
                self.outbound.push(UsbMsg::Refused { device, error });
            }
        }
    }

    /// Let go of this peer's use of a device.
    ///
    /// Only ever ends *this* peer's attachment: the device stays exported while
    /// anyone else is still using it, including this machine itself. A release
    /// from a peer that was not using it removes nothing and is silently
    /// nothing — there is no state to correct and nothing useful to say back.
    fn on_release(&mut self, device: UsbDeviceId) {
        let Some(stream) = self.node().detach_peer(device, self.peer_id) else {
            return;
        };
        tracing::info!(peer = %self.peer_name, %device, "a peer released a device");
        self.outbound.push(UsbMsg::Detached {
            device,
            stream,
            error: None,
        });
        self.announced = false;
    }

    /// Carry URB bytes into the local USB/IP layer.
    ///
    /// Checked against the holder for the same reason a release is: bytes from
    /// a peer that does not hold the device are not addressed to any attach
    /// this machine granted.
    fn on_data(&mut self, stream: UsbStreamId, bytes: &[u8]) {
        // Two directions share one message. Bytes for a device *we borrowed*
        // go to the local virtual controller; bytes for one *we lent out* go to
        // the real device. Told apart by which map the stream is in, because a
        // machine can be doing both at once with the same peer.
        if self.borrowed.values().any(|held| *held == stream) {
            if let Err(error) = self.node().backend().feed_attached(stream, bytes) {
                tracing::warn!(
                    peer = %self.peer_name, stream = stream.0, %error,
                    "cannot hand a peer device's urbs to the local controller"
                );
            }
            return;
        }

        let mut node = self.node();
        let Some(device) = node.device_of_stream(stream) else {
            return;
        };
        // Through the arbiter, not straight at the kernel: a descriptor read is
        // answered from cache, a request for a state already in force is
        // acknowledged, and one that would reconfigure the device under another
        // machine is stalled.
        if let Err(error) = node.feed_from(device, self.peer_id, stream, bytes) {
            drop(node);
            tracing::warn!(peer = %self.peer_name, %device, %error, "urb stream failed");
            self.node().detach_peer(device, self.peer_id);
            self.outbound.push(UsbMsg::Detached {
                device,
                stream,
                error: Some(error),
            });
            self.announced = false;
        }
    }

    /// End every attach this peer holds, because the link is going away.
    ///
    /// A device left exported to a peer that is gone is a device nobody can use
    /// and nobody will release: its holder is unreachable, so the release that
    /// would free it can never arrive.
    fn release_all(&mut self) {
        self.node().peer_gone(self.peer_id);
    }

    /// A peer told us what it has. Take everything we do not already have.
    ///
    /// **Automatic, and that is the whole feature.** "Whichever machine has the
    /// port, every machine has the device" is not something a user should have
    /// to ask for per device — a phone plugged into the laptop must appear in
    /// `adb devices` here without anybody running a command. Attaching is also
    /// cheap by design: enumeration is answered from the host's cache, so a
    /// second machine attaching costs the device nothing and disturbs nobody
    /// already using it (`docs/usb-design.md`, "Ownership, restated").
    ///
    /// What is *not* automatic is the configuration lease, which is taken by
    /// issuing a state-changing request and arbitrated by the host. Attaching
    /// is not claiming.
    fn on_devices(&mut self, devices: &[UsbDevice]) {
        if !self.node().backend().can_attach() {
            // Said once per listing rather than per device: a machine without
            // `vhci-hcd` would otherwise repeat itself for every port the peer
            // has.
            if !devices.is_empty() && self.borrowed.is_empty() && self.asking.is_empty() {
                tracing::info!(
                    peer = %self.peer_name, count = devices.len(),
                    "peer offers usb devices but this machine cannot attach them"
                );
            }
            return;
        }

        // A device that vanished from the listing is gone from the peer's port,
        // so it must go from here too — the peer may not get to say so.
        let present: std::collections::HashSet<UsbDeviceId> =
            devices.iter().map(|d| d.id).collect();
        let stale: Vec<UsbDeviceId> = self
            .borrowed
            .keys()
            .copied()
            .filter(|id| !present.contains(id))
            .collect();
        for device in stale {
            if let Some(stream) = self.borrowed.remove(&device) {
                self.node().backend().detach(stream);
                tracing::info!(
                    peer = %self.peer_name, %device,
                    "a peer device is no longer offered; detaching"
                );
            }
        }

        self.offered = devices.to_vec();
        let now = std::time::Instant::now();
        for device in devices {
            if self.borrowed.contains_key(&device.id) {
                continue;
            }
            // A device this machine already failed to present waits out the
            // retry interval, whatever the listing says.
            if let Some(failed_at) = self.failed.get(&device.id) {
                if now.duration_since(*failed_at) < RETRY_INTERVAL {
                    continue;
                }
                self.failed.remove(&device.id);
            }
            // Some devices are what the peer is using to be a usable machine:
            // its Bluetooth radio, its keyboard, its sound card. Taking one is
            // not sharing, it is breaking that machine — and the symptom shows
            // up nowhere near the cause, as headphones cutting out.
            //
            // A guard against the export *taking the device away from its
            // host*, not against sharing it: once the host attaches its own
            // device back to itself, sharing a radio costs its owner nothing
            // and this narrows to the classes that genuinely cannot be shared.
            if !device.is_safe_to_take() {
                continue;
            }
            if !self.asking.insert(device.id) {
                continue;
            }
            tracing::info!(
                peer = %self.peer_name, device = %device.id, name = %device.label(),
                "asking for a peer device"
            );
            self.outbound.push(UsbMsg::Claim {
                device: device.id,
                reason: "mesh attach".into(),
            });
        }
    }

    /// Notice devices plugged in or unplugged since the last listing.
    ///
    /// Nothing tells a process that a USB device appeared: udev does on Linux
    /// and WM_DEVICECHANGE does on Windows, but both need a event loop of
    /// their own, and neither exists on the other platform. Re-reading the
    /// device list is two file reads on Linux and one short-lived process on
    /// Windows — cheap enough that a poll costs less than the machinery to
    /// avoid it, and the poll is what makes plugging a phone in mid-session
    /// work at all.
    ///
    /// Only a *changed* listing is sent. A machine whose ports do not change —
    /// the common case — puts nothing on the wire however often this runs.
    fn rescan(&mut self) {
        let now = std::time::Instant::now();
        match self.last_scan {
            Some(last) if now.duration_since(last) < SCAN_INTERVAL => return,
            _ => self.last_scan = Some(now),
        }

        let devices = self.listing();
        if devices == self.sent {
            return;
        }
        tracing::info!(
            peer = %self.peer_name,
            before = self.sent.len(),
            after = devices.len(),
            "this machine's usb devices changed; telling the peer"
        );
        self.sent = devices.clone();
        self.outbound.push(UsbMsg::Devices(devices));
    }

    /// Ask again for the peer's devices this machine does not have.
    ///
    /// Slow on purpose. The common reason a device is refused is that another
    /// machine holds it, and asking a peer every few seconds for something it
    /// has already said no to is noise on a link the cursor shares. A minute
    /// is short enough that accepting a prompt feels like it worked and long
    /// enough to be free.
    fn retry_refused(&mut self) {
        if !self.node().backend().can_attach() || self.offered.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        match self.last_retry {
            Some(last) if now.duration_since(last) < RETRY_INTERVAL => return,
            _ => self.last_retry = Some(now),
        }

        let offered = std::mem::take(&mut self.offered);
        self.on_devices(&offered);
    }

    /// The peer agreed. Present its device on this machine.
    fn on_claimed(&mut self, device: UsbDeviceId, stream: UsbStreamId) {
        self.asking.remove(&device);
        if self.borrowed.contains_key(&device) {
            return;
        }
        let presented = self.node().backend().attach(device, stream);
        match presented {
            Ok(()) => {
                self.borrowed.insert(device, stream);
                tracing::info!(
                    peer = %self.peer_name, %device,
                    "a peer device is now on this machine"
                );
            }
            Err(e) => {
                // Give it straight back rather than leaving the peer believing
                // this machine holds a device it never managed to present —
                // and remember the failure, or the release re-announces the
                // listing and the next one asks again immediately.
                tracing::warn!(peer = %self.peer_name, %device, %e, "cannot attach peer device");
                self.failed.insert(device, std::time::Instant::now());
                self.outbound.push(UsbMsg::Release { device });
            }
        }
    }

    /// The local kernel closed a stream, so the attachment behind it is over.
    ///
    /// Told to the peer as [`UsbMsg::Detached`] with a reason rather than
    /// letting its transfers time out: the holder's kernel is waiting on URBs
    /// that will never complete, and every program touching the device hangs
    /// until something says so. The device is unexported here too — the peer's
    /// release cannot arrive for a stream that no longer exists.
    fn end_stream(&mut self, stream: UsbStreamId) {
        let Some(device) = self.node().device_of_stream(stream) else {
            return;
        };
        self.node().detach_peer(device, self.peer_id);
        // The listing carries the holder, so it is stale the moment one lets go.
        self.announced = false;
        self.outbound.push(UsbMsg::Detached {
            device,
            stream,
            error: Some(UsbError::Unplugged),
        });
        tracing::info!(
            peer = %self.peer_name, %device, stream = stream.0,
            "the local usb/ip stream ended; telling the holder"
        );
    }
}

#[async_trait]
impl SessionModule for UsbEngine {
    fn name(&self) -> &'static str {
        "usb"
    }

    fn id(&self) -> ModuleId {
        ModuleId::USB
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.pump(tx).await
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if !self.announced {
            self.announced = true;
            let devices = self.listing();
            self.sent = devices.clone();
            self.outbound.push(UsbMsg::Devices(devices));
        }
        self.retry_refused();
        self.rescan();

        // One drain for the whole machine, then the streams that ended — in
        // that order, because a close and the data preceding it arrive together
        // and reversing them drops the end of every transfer.
        //
        // The node drains because the backend is one per machine: a session
        // collecting for itself would take the bytes belonging to every other
        // link. What comes back is already routed, and the host's own loop has
        // been closed inside it.
        let drained = self.node().drain();
        for (stream, bytes) in drained.to_peers {
            // Only this link's traffic. A stream belongs to one peer, and
            // sending another's down this link would hand it URBs for an
            // attachment it never made.
            if !self.owns_stream(stream) {
                continue;
            }
            // Split at the protocol's cap rather than trusting the kernel to
            // read small: one read can be a whole bulk transfer, and a frame
            // over the limit is refused by the transport rather than sent.
            for chunk in bytes.chunks(UsbMsg::MAX_CHUNK) {
                self.outbound.push(UsbMsg::Data {
                    stream,
                    bytes: chunk.to_vec(),
                });
            }
        }
        for stream in drained.ended {
            if self.owns_stream(stream) {
                self.end_stream(stream);
            }
        }

        // Replies the arbiter produced without the device — a cached
        // descriptor, an acknowledgement, a stall. The submitter is waiting on
        // each one exactly as it would wait for the device's own answer.
        let answers = self.node().take_answers(self.peer_id);
        for (stream, bytes) in answers {
            for chunk in bytes.chunks(UsbMsg::MAX_CHUNK) {
                self.outbound.push(UsbMsg::Data {
                    stream,
                    bytes: chunk.to_vec(),
                });
            }
        }

        for msg in std::mem::take(&mut self.outbound) {
            // Each message states its own class: the device list and the
            // claim/release handshake are control traffic, while the URB stream
            // is bulk. A release must be able to overtake the very transfer it
            // is ending.
            let priority = msg.priority();
            tx.send(&Message::module(ModuleId::USB, priority, &msg)?)
                .await?;
        }
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        let decoded: Option<UsbMsg> = match msg.decode_module(ModuleId::USB) {
            Ok(decoded) => decoded,
            Err(e) => {
                tracing::warn!(peer = %self.peer_name, error = %e, "undecodable usb message");
                return true;
            }
        };
        let Some(decoded) = decoded else {
            return false;
        };

        match decoded {
            UsbMsg::ListDevices => self.announced = false,
            UsbMsg::Claim { device, reason } => self.on_claim(device, &reason),
            UsbMsg::Release { device } => self.on_release(device),
            UsbMsg::Data { stream, bytes } => self.on_data(stream, &bytes),
            UsbMsg::Devices(devices) => self.on_devices(&devices),
            UsbMsg::Claimed { device, stream } => self.on_claimed(device, stream),
            UsbMsg::Refused { device, error } => {
                // Cleared either way, so the device is asked for again on the
                // next listing. A refusal is rarely permanent: the peer that
                // held it lets go, the user accepts the prompt, the driver
                // gets installed. Treating it as final would mean a device
                // that never appears until Cursio is restarted.
                self.asking.remove(&device);
                tracing::info!(
                    peer = %self.peer_name, %device, %error,
                    "peer would not lend a device"
                );
            }
            UsbMsg::Detached {
                device,
                stream,
                error,
            } => {
                self.asking.remove(&device);
                if self.borrowed.remove(&device).is_some() {
                    self.node().backend().detach(stream);
                    tracing::info!(
                        peer = %self.peer_name, %device, ?error,
                        "a peer device went away"
                    );
                }
            }
        }
        true
    }

    async fn shutdown(&mut self, _tx: &mut dyn TransportTx) {
        self.release_all();
    }
}

/// The Linux backend: sysfs enumeration plus `usbip-host`.
///
/// Lives here rather than in `cursio-usb` because [`UsbBackend`] is policy's
/// vocabulary and the dependency runs this way — `core` knows about `usb`, not
/// the reverse. The crate below stays free of any opinion about who may attach
/// what, which is what lets its tests run without a session.
#[cfg(all(feature = "usb", target_os = "linux"))]
pub struct LinuxUsbBackend {
    enumerator: cursio_usb::LinuxUsb,
    exporter: cursio_usb::Exporter,
    /// Stable ids for devices, so a peer's claim survives a re-enumeration.
    offers: cursio_usb::DeviceOffers,
    /// Streams handed out, and the socket carrying each one's URBs.
    streams: HashMap<UsbStreamId, ServedStream>,
    next_stream: u64,
    /// Streams the kernel closed, reported after their last bytes.
    ended: Vec<UsbStreamId>,
    /// The importing half: `vhci-hcd`, which is loaded separately from
    /// `usbip-host` and may well be absent while exporting works.
    importer: cursio_usb::Importer,
    /// Peer devices presented on this machine, keyed by the peer's stream id.
    borrowed: HashMap<UsbStreamId, BorrowedStream>,
}

/// One device borrowed from a peer and presented locally.
///
/// The pipe carries URBs between this machine's virtual controller and the
/// mesh; the attachment holds the vhci port and releases it on drop, so a lost
/// session cannot leave a port bound to a socket nobody reads.
#[cfg(all(feature = "usb", target_os = "linux"))]
struct BorrowedStream {
    pipe: cursio_usb::KernelPipe,
    _attachment: cursio_usb::import::Attachment,
}

/// One device being served over a socket the kernel holds the far end of.
#[cfg(all(feature = "usb", target_os = "linux"))]
struct ServedStream {
    busid: String,
    /// Our end of the pair. Dropping it closes the kernel's end, which is what
    /// ends the export — so this must live exactly as long as the attachment.
    pipe: cursio_usb::KernelPipe,
}

#[cfg(all(feature = "usb", target_os = "linux"))]
impl Default for LinuxUsbBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(feature = "usb", target_os = "linux"))]
impl LinuxUsbBackend {
    pub fn new() -> Self {
        Self {
            enumerator: cursio_usb::LinuxUsb::new(),
            exporter: cursio_usb::Exporter::new(),
            offers: cursio_usb::DeviceOffers::new(),
            streams: HashMap::new(),
            next_stream: 1,
            ended: Vec::new(),
            importer: cursio_usb::Importer::new(),
            borrowed: HashMap::new(),
        }
    }

    /// Whether this machine can hand a device over at all.
    ///
    /// The `usbip-host` module is not loaded by default on any distribution, so
    /// this is the ordinary answer on a fresh machine. Checked before
    /// advertising rather than on the first claim: invariant 6 in
    /// `docs/usb-design.md` — absence is advertised, never simulated.
    pub fn is_available(&self) -> bool {
        self.exporter.is_available()
    }
}

#[cfg(all(feature = "usb", target_os = "linux"))]
impl UsbBackend for LinuxUsbBackend {
    fn devices(&mut self) -> Vec<UsbDevice> {
        match self.enumerator.devices() {
            Ok(local) => self.offers.offer(&local),
            Err(e) => {
                // An empty list rather than a stale one: a peer shown devices
                // that can no longer be enumerated would claim one and get a
                // refusal it cannot act on.
                tracing::warn!(error = %e, "cannot enumerate usb devices");
                Vec::new()
            }
        }
    }

    fn export(&mut self, device: UsbDeviceId) -> Result<UsbStreamId, UsbError> {
        let Some(busid) = self.offers.busid(device).map(str::to_string) else {
            return Err(UsbError::Gone);
        };
        self.exporter.export(&busid).map_err(|e| {
            tracing::warn!(%device, busid, error = %e, "cannot export device");
            backend_error_to_wire(&e)
        })?;

        // The socket the kernel serves this device over. Made after the bind
        // because `usbip_sockfd` does not exist until then.
        let (pipe, kernel_end) = cursio_usb::KernelPipe::new().map_err(|e| {
            self.exporter.unexport(&busid);
            backend_error_to_wire(&e)
        })?;
        if let Err(e) = self
            .exporter
            .serve(&busid, std::os::fd::AsRawFd::as_raw_fd(&kernel_end))
        {
            // Nothing is serving, so leaving the device bound would strand it:
            // taken from its own driver and given to nobody.
            self.exporter.unexport(&busid);
            tracing::warn!(%device, busid, error = %e, "cannot hand the socket to the kernel");
            return Err(backend_error_to_wire(&e));
        }
        // The kernel `dup`s the descriptor, so ours has done its job. Holding
        // it would keep a second reference alive and make the close that ends
        // the export do nothing.
        drop(kernel_end);

        let stream = UsbStreamId(self.next_stream);
        self.next_stream += 1;
        self.streams.insert(stream, ServedStream { busid, pipe });
        Ok(stream)
    }

    fn unexport(&mut self, device: UsbDeviceId) {
        let Some(busid) = self.offers.busid(device).map(str::to_string) else {
            return;
        };
        // Dropping the pipe closes the kernel's end, which is what stops the
        // export; the explicit `-1` is for the case where the kernel is slower
        // to notice than the unbind that follows.
        self.streams.retain(|_, served| served.busid != busid);
        let _ = self.exporter.stop_serving(&busid);
        self.exporter.unexport(&busid);
    }

    fn feed(&mut self, stream: UsbStreamId, bytes: &[u8]) -> Result<(), UsbError> {
        let Some(served) = self.streams.get_mut(&stream) else {
            return Err(UsbError::Gone);
        };
        served.pipe.write(bytes).map_err(|e| {
            tracing::warn!(stream = stream.0, error = %e, "cannot hand urbs to the kernel");
            backend_error_to_wire(&e)
        })
    }

    fn collect(&mut self, out: &mut Vec<(UsbStreamId, Vec<u8>)>) {
        let mut chunks = Vec::new();
        for (stream, served) in &mut self.streams {
            chunks.clear();
            match served.pipe.drain(&mut chunks) {
                Ok(()) => {}
                // Recorded rather than acted on here: the bytes drained above
                // are the end of a transfer and must be sent before the detach.
                Err(cursio_usb::PipeEnded) => self.ended.push(*stream),
            }
            out.extend(chunks.drain(..).map(|bytes| (*stream, bytes)));
        }
    }

    fn ended(&mut self) -> Vec<UsbStreamId> {
        let ended = std::mem::take(&mut self.ended);
        for stream in &ended {
            self.streams.remove(stream);
        }
        ended
    }

    fn can_attach(&self) -> bool {
        self.importer.is_available()
    }

    fn attach(&mut self, device: UsbDeviceId, stream: UsbStreamId) -> Result<(), UsbError> {
        let (pipe, kernel_end) = cursio_usb::KernelPipe::new().map_err(|e| {
            tracing::warn!(%device, error = %e, "cannot make a socket for a peer device");
            backend_error_to_wire(&e)
        })?;

        // `devid` is the *remote* machine's bus and device number, which this
        // side does not know: the peer's ids are opaque here by design. The
        // kernel only echoes it back in messages, so a placeholder costs a
        // less useful log line there and nothing else.
        //
        // Speed is High for the same reason — the listing carries no speed, and
        // a device that enumerates at the wrong rate is a device with a problem
        // while one refused for an unknown speed never appears at all.
        let attachment = self
            .importer
            .attach(&kernel_end, 0, cursio_usb::Speed::High)
            .map_err(|e| {
                tracing::warn!(%device, error = %e, "cannot attach a peer device");
                backend_error_to_wire(&e)
            })?;
        // The kernel `dup`s the descriptor; holding ours would keep a second
        // reference alive and make the close that ends the attach do nothing.
        drop(kernel_end);

        tracing::info!(%device, port = attachment.port(), "peer device attached locally");
        self.borrowed.insert(
            stream,
            BorrowedStream {
                pipe,
                _attachment: attachment,
            },
        );
        Ok(())
    }

    fn detach(&mut self, stream: UsbStreamId) {
        // Dropping both closes the socket and frees the vhci port, in that
        // order — the port cannot be reused while a socket still references it.
        self.borrowed.remove(&stream);
    }

    fn feed_attached(&mut self, stream: UsbStreamId, bytes: &[u8]) -> Result<(), UsbError> {
        let Some(borrowed) = self.borrowed.get_mut(&stream) else {
            return Err(UsbError::Gone);
        };
        borrowed.pipe.write(bytes).map_err(|e| {
            tracing::warn!(stream = stream.0, error = %e, "cannot write to the virtual controller");
            backend_error_to_wire(&e)
        })
    }

    fn collect_attached(&mut self, out: &mut Vec<(UsbStreamId, Vec<u8>)>) {
        let mut chunks = Vec::new();
        let mut closed = Vec::new();
        for (stream, borrowed) in &mut self.borrowed {
            chunks.clear();
            if borrowed.pipe.drain(&mut chunks).is_err() {
                closed.push(*stream);
            }
            out.extend(chunks.drain(..).map(|bytes| (*stream, bytes)));
        }
        // After draining, so a transfer's last bytes reach the peer before the
        // attachment they belong to disappears.
        for stream in closed {
            self.borrowed.remove(&stream);
        }
    }
}

/// Turn a local failure into what a peer is told.
///
/// The mapping is deliberately not "everything becomes a string": the peer
/// shows the user a different thing for each of these. A missing module is the
/// host's to fix and is worth naming; a privilege failure is a prompt on the
/// host, not a retry here; a device that is gone means stop asking.
#[cfg(all(feature = "usb", target_os = "linux"))]
fn backend_error_to_wire(error: &cursio_usb::UsbBackendError) -> UsbError {
    use cursio_usb::UsbBackendError;
    match error {
        UsbBackendError::NoSuchDevice(_) => UsbError::Gone,
        UsbBackendError::DriverMissing(what) => UsbError::DriverMissing(what.clone()),
        UsbBackendError::NeedsPrivilege(_) => UsbError::NeedsPrivilege,
        other => UsbError::Backend(other.to_string()),
    }
}

/// The Windows backend: `usbipd-win` for both enumeration and sharing.
///
/// Shares [`DeviceOffers`](cursio_usb::DeviceOffers) and the whole policy layer
/// with Linux; what differs is only where the device list comes from and what
/// "export" means. The URB path does not exist here yet — see
/// [`feed`](UsbBackend::feed) — so this machine can *offer* devices and a peer
/// can see them, which is the half that has to work before carrying bytes is
/// worth writing.
#[cfg(all(feature = "usb", target_os = "windows"))]
pub struct WindowsUsbBackend {
    usbipd: cursio_usb::WindowsUsb,
    offers: cursio_usb::DeviceOffers,
    /// Bus ids handed out, and the connection carrying each one's URBs.
    streams: HashMap<UsbStreamId, WindowsStream>,
    next_stream: u64,
}

/// One device shared over a connection to the local USB/IP server.
///
/// `usbipd-win` will not take a socket from us the way the Linux kernel does,
/// but it does serve one on loopback — so this is a client connection rather
/// than a socketpair, and the mesh carries what crosses it.
#[cfg(all(feature = "usb", target_os = "windows"))]
struct WindowsStream {
    busid: String,
    tunnel: cursio_usb::WinTunnel,
}

#[cfg(all(feature = "usb", target_os = "windows"))]
impl Default for WindowsUsbBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(feature = "usb", target_os = "windows"))]
impl WindowsUsbBackend {
    pub fn new() -> Self {
        Self {
            usbipd: cursio_usb::WindowsUsb::new(),
            offers: cursio_usb::DeviceOffers::new(),
            streams: HashMap::new(),
            next_stream: 1,
        }
    }

    /// Whether `usbipd-win` is installed.
    pub fn is_available(&self) -> bool {
        self.usbipd.is_available()
    }
}

#[cfg(all(feature = "usb", target_os = "windows"))]
impl UsbBackend for WindowsUsbBackend {
    fn devices(&mut self) -> Vec<UsbDevice> {
        match self.usbipd.devices() {
            Ok(local) => self.offers.offer(&local),
            Err(e) => {
                tracing::warn!(error = %e, "cannot enumerate usb devices");
                Vec::new()
            }
        }
    }

    fn export(&mut self, device: UsbDeviceId) -> Result<UsbStreamId, UsbError> {
        let Some(busid) = self.offers.busid(device).map(str::to_string) else {
            return Err(UsbError::Gone);
        };
        // Needs administrator, and says so rather than escalating: the fix is a
        // prompt the user accepts, not a right this process should hold.
        self.usbipd.bind(&busid).map_err(|e| {
            tracing::warn!(%device, busid, error = %e, "cannot share device");
            backend_error_to_wire(&e)
        })?;

        // The device is shared; now open the connection that will carry its
        // transfers. Loopback only — see `cursio_usb::wintunnel`.
        let tunnel = cursio_usb::WinTunnel::connect(&busid).map_err(|e| {
            self.usbipd.unbind(&busid);
            tracing::warn!(%device, busid, error = %e, "cannot reach the local usbipd server");
            backend_error_to_wire(&e)
        })?;

        let stream = UsbStreamId(self.next_stream);
        self.next_stream += 1;
        self.streams.insert(stream, WindowsStream { busid, tunnel });
        Ok(stream)
    }

    fn unexport(&mut self, device: UsbDeviceId) {
        let Some(busid) = self.offers.busid(device).map(str::to_string) else {
            return;
        };
        // Dropping the stream closes the connection, which is how usbipd
        // learns the attachment is over.
        self.streams.retain(|_, held| held.busid != busid);
        self.usbipd.unbind(&busid);
    }

    fn feed(&mut self, stream: UsbStreamId, bytes: &[u8]) -> Result<(), UsbError> {
        let Some(served) = self.streams.get_mut(&stream) else {
            return Err(UsbError::Gone);
        };
        served.tunnel.write(bytes).map_err(|e| {
            tracing::warn!(stream = stream.0, error = %e, "cannot hand urbs to usbipd");
            backend_error_to_wire(&e)
        })
    }

    fn collect(&mut self, out: &mut Vec<(UsbStreamId, Vec<u8>)>) {
        let mut chunks = Vec::new();
        let mut closed = Vec::new();
        for (stream, served) in &mut self.streams {
            chunks.clear();
            if served.tunnel.drain(&mut chunks).is_err() {
                closed.push(*stream);
            }
            out.extend(chunks.drain(..).map(|bytes| (*stream, bytes)));
        }
        // After draining, so a transfer's last bytes reach the peer before the
        // stream they belong to disappears.
        for stream in closed {
            self.streams.remove(&stream);
        }
    }
}

/// Turn a local failure into what a peer is told.
///
/// Shared with the Linux backend deliberately: the peer's vocabulary is the
/// same whichever kernel refused, and a second mapping would drift.
#[cfg(all(feature = "usb", target_os = "windows"))]
fn backend_error_to_wire(error: &cursio_usb::UsbBackendError) -> UsbError {
    use cursio_usb::UsbBackendError;
    match error {
        UsbBackendError::NoSuchDevice(_) => UsbError::Gone,
        UsbBackendError::DriverMissing(what) => UsbError::DriverMissing(what.clone()),
        UsbBackendError::NeedsPrivilege(_) => UsbError::NeedsPrivilege,
        other => UsbError::Backend(other.to_string()),
    }
}

/// Builds the USB module for a link, and holds what every link shares.
///
/// The factory outlives each session — a node builds one registry at startup —
/// which is what makes it the right place for [`UsbNode`]. Built on the first
/// link rather than in the constructor, so a machine that never negotiates the
/// module never touches its kernel.
///
/// [`UsbNode`]: crate::usb_node::UsbNode
#[cfg(feature = "usb")]
pub struct UsbFactory {
    pub enabled: bool,
    /// This machine's ports. One per node, shared by every session.
    node: std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<crate::usb_node::UsbNode>>>>,
}

#[cfg(feature = "usb")]
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

#[cfg(feature = "usb")]
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
            // One prompt for the machine, not one per device: loading the
            // modules and making the control files group-writable is a
            // property of this host, and everything after it runs
            // unprivileged. Does nothing when the machine is already set up,
            // so this is free on every start but the first.
            match cursio_usb::setup::ensure_ready() {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        "usb sharing is off: setting this machine up needs privilege \
                         and the prompt was declined"
                    );
                    return None;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "usb sharing is off");
                    return None;
                }
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
            self.node(ctx.local.peer_id, || Box::new(WindowsUsbBackend::new())),
        )))
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    fn build(&self, _ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        anyhow::bail!("usb sharing is not implemented on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::usb::UsbTransport;

    /// A buffer the test and the fake backend both hold: the node owns the
    /// backend, so a test reaches its recordings through one of these.
    type Shared = std::sync::Arc<std::sync::Mutex<Vec<(UsbStreamId, Vec<u8>)>>>;

    /// A fake backend wrapped in the node every session shares.
    fn node_of(
        backend: impl UsbBackend + 'static,
    ) -> std::sync::Arc<std::sync::Mutex<crate::usb_node::UsbNode>> {
        std::sync::Arc::new(std::sync::Mutex::new(crate::usb_node::UsbNode::new(
            Box::new(backend),
            peer(200),
        )))
    }

    /// A backend with no kernel behind it: every ownership race is decided in
    /// the engine, so none of them need a real device.
    #[derive(Default)]
    struct FakeBackend {
        present: Vec<UsbDeviceId>,
        exported: Vec<UsbDeviceId>,
        /// What `export` should fail with, when the test wants a refusal.
        refuse: Option<UsbError>,
        /// What `feed` should fail with, to exercise a mid-stream failure.
        ///
        /// Shared so a test can break the backend *after* an attach exists:
        /// replacing the whole backend would throw away the export the node is
        /// holding, which is the state the test is about.
        feed_fails: std::sync::Arc<std::sync::Mutex<Option<UsbError>>>,
        /// What actually reached the device. Shared so a test can prove a
        /// transfer was answered without the device hearing about it.
        fed: Shared,
        next_stream: u64,
        /// What the local kernel has produced, for the next `collect`.
        ///
        /// Shared so a test can make the device answer after an attach exists.
        produced: Shared,
        /// Streams the kernel has closed, for the next `ended`.
        closed: Vec<UsbStreamId>,
        /// Whether this machine pretends to have a virtual controller.
        attaches: bool,
        /// Bytes handed to a local virtual controller, keyed by stream. What
        /// the host's own loop delivers shows up here.
        ///
        /// Shared so a test can read it while the node owns the backend.
        to_controller: Shared,
        /// What a local virtual controller wants to submit, for the next
        /// `collect_attached`.
        submitted: Vec<(UsbStreamId, Vec<u8>)>,
        /// Peer devices this backend was told to present locally.
        borrowed: Vec<(UsbDeviceId, UsbStreamId)>,
        /// Whether presenting a peer device should fail, as it does on a
        /// machine whose `vhci-hcd` refuses without root.
        attach_fails: bool,
    }

    impl FakeBackend {
        fn with(devices: &[u64]) -> Self {
            Self {
                present: devices.iter().map(|d| UsbDeviceId(*d)).collect(),
                ..Default::default()
            }
        }
    }

    impl UsbBackend for FakeBackend {
        fn devices(&mut self) -> Vec<UsbDevice> {
            self.present
                .iter()
                .map(|id| UsbDevice {
                    id: *id,
                    vendor: 0x18d1,
                    product: 0x4ee7,
                    manufacturer: Some("Google".into()),
                    description: Some("Pixel".into()),
                    class: 0,
                    transport: UsbTransport::Fine,
                    holder: None,
                })
                .collect()
        }

        fn export(&mut self, device: UsbDeviceId) -> Result<UsbStreamId, UsbError> {
            if let Some(error) = self.refuse.clone() {
                return Err(error);
            }
            // A device that is not here is `Gone`, exactly as both real
            // backends answer when the id resolves to no port: they look it up
            // in the table the last listing built, and a device unplugged since
            // then is no longer in it.
            if !self.present.contains(&device) {
                return Err(UsbError::Gone);
            }
            self.next_stream += 1;
            self.exported.push(device);
            Ok(UsbStreamId(self.next_stream))
        }

        fn unexport(&mut self, device: UsbDeviceId) {
            self.exported.retain(|d| *d != device);
        }

        fn collect(&mut self, out: &mut Vec<(UsbStreamId, Vec<u8>)>) {
            out.append(&mut self.produced.lock().unwrap_or_else(|e| e.into_inner()));
        }

        fn can_attach(&self) -> bool {
            self.attaches
        }

        fn attach(&mut self, device: UsbDeviceId, stream: UsbStreamId) -> Result<(), UsbError> {
            if self.attach_fails {
                return Err(UsbError::NeedsPrivilege);
            }
            self.borrowed.push((device, stream));
            Ok(())
        }

        fn detach(&mut self, stream: UsbStreamId) {
            self.borrowed.retain(|(_, held)| *held != stream);
        }

        fn feed_attached(&mut self, stream: UsbStreamId, bytes: &[u8]) -> Result<(), UsbError> {
            self.to_controller
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((stream, bytes.to_vec()));
            Ok(())
        }

        fn collect_attached(&mut self, out: &mut Vec<(UsbStreamId, Vec<u8>)>) {
            out.append(&mut self.submitted);
        }

        fn ended(&mut self) -> Vec<UsbStreamId> {
            std::mem::take(&mut self.closed)
        }

        fn feed(&mut self, stream: UsbStreamId, bytes: &[u8]) -> Result<(), UsbError> {
            if let Some(error) = self
                .feed_fails
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return Err(error);
            }
            self.fed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((stream, bytes.to_vec()));
            Ok(())
        }
    }

    fn peer(n: u8) -> PeerId {
        PeerId::from_bytes([n; 16])
    }

    fn engine(devices: &[u64]) -> UsbEngine {
        UsbEngine::new(
            "desktop".into(),
            peer(1),
            node_of(FakeBackend::with(devices)),
        )
    }

    /// Everything the engine queued, draining it the way a pump would.
    fn taken(engine: &mut UsbEngine) -> Vec<UsbMsg> {
        std::mem::take(&mut engine.outbound)
    }

    #[test]
    fn a_claim_on_a_free_device_is_granted() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        assert!(matches!(
            taken(&mut e).as_slice(),
            [UsbMsg::Claimed { device, .. }] if *device == UsbDeviceId(1)
        ));
    }

    /// How many *requests* actually reached the device. Replies travelling the
    /// other way go through the same call, and counting those would score the
    /// device's own answer as a second question.
    fn submissions(fed: &Shared) -> usize {
        fed.lock()
            .expect("test lock")
            .iter()
            .filter(|(_, bytes)| {
                cursio_usb::urb::Header::parse(bytes)
                    .is_ok_and(|h| h.command == cursio_usb::urb::Command::Submit)
            })
            .count()
    }

    /// A USB/IP submission, as a host puts one on the wire.
    fn submission(seqnum: u32, setup: [u8; 8]) -> Vec<u8> {
        let mut bytes = vec![0u8; cursio_usb::urb::HEADER_LEN];
        bytes[0x00..0x04].copy_from_slice(&1u32.to_be_bytes());
        bytes[0x04..0x08].copy_from_slice(&seqnum.to_be_bytes());
        bytes[0x0c..0x10].copy_from_slice(&1u32.to_be_bytes());
        bytes[0x28..0x30].copy_from_slice(&setup);
        bytes
    }

    /// `GET_DESCRIPTOR` for the device descriptor — the first thing every host
    /// asks, and the request the cache exists to answer.
    fn get_descriptor() -> [u8; 8] {
        [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]
    }

    /// What makes "every machine sees it" cheap rather than a thundering herd:
    /// the second machine to enumerate is answered from the cache and the
    /// device never hears about it.
    #[test]
    fn a_second_machine_enumerates_without_touching_the_device() {
        let device = UsbDeviceId(1);
        let fed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut e = UsbEngine::new(
            "desktop".into(),
            peer(1),
            node_of(FakeBackend {
                fed: fed.clone(),
                ..FakeBackend::with(&[1])
            }),
        );

        // The first machine asks, and the device answers.
        e.on_claim(device, "adb");
        let first = match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => *stream,
            other => panic!("expected a grant, got {other:?}"),
        };
        e.on_data(first, &submission(1, get_descriptor()));
        assert_eq!(
            submissions(&fed),
            1,
            "the first enumeration must reach the device"
        );
        // The device answers, which is what fills the cache.
        let reply = cursio_usb::urb::Header::reply(1, 0, 0, b"the descriptor");
        e.node().feed_from(device, peer(1), first, &reply).ok();

        // A second machine asks the identical question.
        e.peer_id = peer(2);
        e.on_claim(device, "also adb");
        let second = match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => *stream,
            other => panic!("expected a second grant, got {other:?}"),
        };
        e.on_data(second, &submission(1, get_descriptor()));

        assert_eq!(
            submissions(&fed),
            1,
            "the device must not be asked twice for what cannot have changed"
        );
        let answered = e.node().take_answers(peer(2));
        assert!(
            answered
                .iter()
                .any(|(s, b)| *s == second && b.ends_with(b"the descriptor")),
            "the second machine is answered from the cache instead"
        );
    }

    /// The symptom this exists to fix: a phone plugged into the laptop showed
    /// up in `adb devices` on every machine *except* the laptop. Exporting
    /// binds the device away from its own driver, so sharing it meant losing
    /// it — available to everyone but its owner.
    #[test]
    fn a_machine_keeps_its_own_device_while_sharing_it() {
        let mut e = UsbEngine::new(
            "desktop".into(),
            peer(1),
            node_of(FakeBackend {
                attaches: true,
                ..FakeBackend::with(&[1])
            }),
        );
        e.on_claim(UsbDeviceId(1), "adb");

        assert!(
            e.node().is_local(UsbDeviceId(1)),
            "the host must attach its own device back to itself"
        );
    }

    /// And the device's answers reach the host's own controller, not only the
    /// peer that asked. Two machines attached means two are owed the reply.
    #[test]
    fn the_device_answers_the_host_as_well_as_the_peer() {
        let device = UsbDeviceId(1);
        let produced = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let to_controller = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut e = UsbEngine::new(
            "desktop".into(),
            peer(1),
            node_of(FakeBackend {
                attaches: true,
                produced: produced.clone(),
                to_controller: to_controller.clone(),
                ..FakeBackend::with(&[1])
            }),
        );
        e.on_claim(device, "adb");
        let peer_stream = match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => *stream,
            other => panic!("expected a grant, got {other:?}"),
        };

        // The device produces a reply on the stream the peer is reading.
        produced
            .lock()
            .expect("test lock")
            .push((peer_stream, b"reply".to_vec()));
        let drained = e.node().drain();

        assert!(
            drained
                .to_peers
                .iter()
                .any(|(s, b)| *s == peer_stream && b == b"reply"),
            "the peer that asked still gets its answer"
        );
        assert!(
            to_controller
                .lock()
                .expect("test lock")
                .iter()
                .any(|(_, b)| b == b"reply"),
            "and so does the machine the device is plugged into"
        );
    }

    /// A machine with no virtual controller of its own still shares what it
    /// has. It simply does not get the device back — which is what happened
    /// everywhere before the loop existed.
    #[test]
    fn a_machine_that_cannot_attach_still_shares_its_ports() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");

        assert!(matches!(taken(&mut e).as_slice(), [UsbMsg::Claimed { .. }]));
        assert!(
            !e.node().is_local(UsbDeviceId(1)),
            "nothing to attach it to, and that is not an error"
        );
    }

    /// The point of arbitration: two machines use one device at once. The
    /// second to ask is granted its own stream rather than told somebody else
    /// got there first — enumeration is answered from the arbiter's cache, so
    /// joining costs the device nothing.
    ///
    /// This asserted the opposite until the session was brought in line with
    /// `docs/usb-design.md`: "one holder, everyone else refused" refuses
    /// machines that only ever wanted to read a descriptor.
    #[test]
    fn a_second_peer_gets_its_own_stream_onto_the_same_device() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        let first = match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => *stream,
            other => panic!("expected a grant, got {other:?}"),
        };

        // The same engine, but the claim now arrives from a different session.
        e.peer_id = peer(2);
        e.on_claim(UsbDeviceId(1), "also adb");

        match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => {
                assert_ne!(*stream, first, "each holder needs a stream of its own");
            }
            other => panic!("expected a second grant, got {other:?}"),
        }
    }

    /// And one peer letting go does not detach the bus from under the other.
    #[test]
    fn releasing_one_holder_leaves_the_device_exported_for_the_rest() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        e.peer_id = peer(2);
        e.on_claim(UsbDeviceId(1), "also adb");
        taken(&mut e);

        e.on_release(UsbDeviceId(1));
        assert!(
            e.node().is_exported(UsbDeviceId(1)),
            "the first peer is still using it"
        );

        e.peer_id = peer(1);
        e.on_release(UsbDeviceId(1));
        assert_eq!(e.exported_count(), 0, "now nobody is, so it goes back");
    }

    /// A peer that reconnects re-asks for what it believes it still holds. That
    /// is not a conflict with itself — answering `InUse(you)` would strand it.
    #[test]
    fn the_holder_reclaiming_gets_the_same_stream_back() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        let first = match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => *stream,
            other => panic!("expected a grant, got {other:?}"),
        };

        e.on_claim(UsbDeviceId(1), "adb again");
        match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => assert_eq!(*stream, first),
            other => panic!("expected the same grant, got {other:?}"),
        }
    }

    /// Only the holder may let go. A release from anyone else must leave the
    /// attach exactly as it was — obeying it detaches a bus mid-transfer.
    #[test]
    fn a_release_from_a_non_holder_is_ignored() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        taken(&mut e);

        e.peer_id = peer(2);
        e.on_release(UsbDeviceId(1));

        assert!(taken(&mut e).is_empty(), "nothing to say to a non-holder");
        assert!(
            e.node().is_exported(UsbDeviceId(1)),
            "the real holder must keep the device"
        );
    }

    #[test]
    fn the_holder_releasing_frees_the_device() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        taken(&mut e);

        e.on_release(UsbDeviceId(1));

        assert!(matches!(
            taken(&mut e).as_slice(),
            [UsbMsg::Detached { error: None, .. }]
        ));
        assert!(e.exported_count() == 0, "the device is free again");
    }

    /// A device that is not there is refused as gone, rather than exported and
    /// found missing later by a peer that has already started using it.
    #[test]
    fn claiming_a_device_that_is_not_here_is_refused_as_gone() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(99), "adb");
        assert!(matches!(
            taken(&mut e).as_slice(),
            [UsbMsg::Refused {
                error: UsbError::Gone,
                ..
            }]
        ));
    }

    /// A backend that cannot bind says why, and the reason reaches the peer
    /// intact: "needs privilege" and "already in use" have different fixes.
    #[test]
    fn a_backend_refusal_reaches_the_peer_with_its_reason() {
        let mut e = UsbEngine::new(
            "desktop".into(),
            peer(1),
            node_of(FakeBackend {
                refuse: Some(UsbError::NeedsPrivilege),
                ..FakeBackend::with(&[1])
            }),
        );
        e.on_claim(UsbDeviceId(1), "adb");
        assert!(matches!(
            taken(&mut e).as_slice(),
            [UsbMsg::Refused {
                error: UsbError::NeedsPrivilege,
                ..
            }]
        ));
    }

    /// URB bytes from a peer that does not hold the device are not addressed to
    /// any attach this machine granted, and must not reach the kernel.
    #[test]
    fn urb_bytes_from_a_non_holder_never_reach_the_backend() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        taken(&mut e);

        e.peer_id = peer(2);
        e.on_data(UsbStreamId(1), b"urb");

        // The backend is behind a box; ask the engine's own record instead.
        assert!(
            e.node().is_exported(UsbDeviceId(1)),
            "the attach is untouched"
        );
        assert!(taken(&mut e).is_empty(), "and nothing is said about it");
    }

    /// A stream that fails mid-transfer ends the attach and says why, rather
    /// than leaving the holder's kernel waiting on transfers that never come.
    #[test]
    fn a_failing_stream_detaches_with_a_reason() {
        let breaks = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut e = UsbEngine::new(
            "desktop".into(),
            peer(1),
            node_of(FakeBackend {
                feed_fails: breaks.clone(),
                ..FakeBackend::with(&[1])
            }),
        );
        e.on_claim(UsbDeviceId(1), "adb");
        let stream = match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => *stream,
            other => panic!("expected a grant, got {other:?}"),
        };

        // Break the device now that the attach exists.
        *breaks.lock().expect("test lock") = Some(UsbError::Unplugged);
        e.on_data(stream, b"urb");

        assert!(matches!(
            taken(&mut e).as_slice(),
            [UsbMsg::Detached {
                error: Some(UsbError::Unplugged),
                ..
            }]
        ));
        assert!(e.exported_count() == 0, "a broken attach does not linger");
    }

    /// A holder that goes away can never send the release that would free its
    /// device, so the link ending has to do it.
    #[test]
    fn a_device_is_reclaimed_when_its_holder_goes_away() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        taken(&mut e);

        e.release_all();

        assert!(e.exported_count() == 0, "the device came back");
    }

    /// The listing is the resync path, so it must say who holds what — a peer
    /// reconnecting learns the device is taken without asking for it first.
    #[test]
    fn the_listing_names_the_holder_of_each_device() {
        let mut e = engine(&[1, 2]);
        e.on_claim(UsbDeviceId(1), "adb");
        taken(&mut e);

        let listing = e.listing();
        let held = listing.iter().find(|d| d.id == UsbDeviceId(1)).unwrap();
        let free = listing.iter().find(|d| d.id == UsbDeviceId(2)).unwrap();
        assert_eq!(held.holder, Some(peer(1)));
        assert_eq!(free.holder, None, "an unclaimed device names nobody");
    }

    /// A claim changes what every peer should see, so the next pump re-announces
    /// rather than leaving other machines showing it as free.
    #[test]
    fn granting_a_device_schedules_a_fresh_listing() {
        let mut e = engine(&[1]);
        e.announced = true;
        e.on_claim(UsbDeviceId(1), "adb");
        assert!(!e.announced, "the listing is stale once a device is taken");
    }

    /// The kernel closing a stream is not something the holder can see: its own
    /// kernel is left waiting on URBs that will never complete, and every
    /// program touching the device hangs until it is told.
    #[test]
    fn a_stream_the_kernel_closed_detaches_the_holder() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        let stream = match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => *stream,
            other => panic!("expected a grant, got {other:?}"),
        };

        e.end_stream(stream);

        assert!(matches!(
            taken(&mut e).as_slice(),
            [UsbMsg::Detached { device, error: Some(UsbError::Unplugged), .. }]
                if *device == UsbDeviceId(1)
        ));
        assert!(
            e.exported_count() == 0,
            "the device must not stay recorded as held"
        );
    }

    /// The listing carries the holder, so it is stale the moment a stream ends
    /// — otherwise every other peer keeps showing the device as taken.
    #[test]
    fn a_closed_stream_schedules_a_fresh_listing() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        let stream = match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => *stream,
            other => panic!("expected a grant, got {other:?}"),
        };
        e.announced = true;

        e.end_stream(stream);
        assert!(!e.announced);
    }

    /// Build an engine whose machine can also take a peer's devices.
    fn attaching_engine() -> UsbEngine {
        let backend = FakeBackend {
            attaches: true,
            ..FakeBackend::with(&[])
        };
        UsbEngine::new("laptop".into(), peer(1), node_of(backend))
    }

    /// One offered device, as a peer would announce it.
    fn offered(id: u64) -> UsbDevice {
        UsbDevice {
            id: UsbDeviceId(id),
            vendor: 0x2717,
            product: 0xFF08,
            manufacturer: None,
            description: Some("POCO X7 Pro".into()),
            class: 0,
            transport: UsbTransport::Fine,
            holder: None,
        }
    }

    /// The whole feature: "whichever machine has the port, every machine has
    /// the device". Seeing a peer's listing must ask for its devices without
    /// anybody running a command.
    #[test]
    fn a_peers_devices_are_asked_for_the_moment_they_are_announced() {
        let mut e = attaching_engine();
        e.on_devices(&[offered(1), offered(2)]);

        let asked: Vec<UsbDeviceId> = taken(&mut e)
            .into_iter()
            .filter_map(|msg| match msg {
                UsbMsg::Claim { device, .. } => Some(device),
                _ => None,
            })
            .collect();
        assert_eq!(asked, vec![UsbDeviceId(1), UsbDeviceId(2)]);
    }

    /// The listing is re-announced whenever anything changes, and asking again
    /// for a device already being fetched would flood the link.
    #[test]
    fn a_re_announced_listing_does_not_ask_twice() {
        let mut e = attaching_engine();
        e.on_devices(&[offered(1)]);
        let _ = taken(&mut e);

        e.on_devices(&[offered(1)]);
        assert!(taken(&mut e).is_empty(), "asked for the same device twice");
    }

    /// A machine without `vhci-hcd` can still share its own ports, and must not
    /// ask for devices it has nowhere to put.
    #[test]
    fn a_machine_that_cannot_attach_asks_for_nothing() {
        let mut e = engine(&[]);
        e.on_devices(&[offered(1)]);
        assert!(taken(&mut e).is_empty());
    }

    /// The grant is what makes the device appear locally.
    #[test]
    fn a_granted_device_is_presented_on_this_machine() {
        let mut e = attaching_engine();
        e.on_devices(&[offered(1)]);
        let _ = taken(&mut e);

        e.on_claimed(UsbDeviceId(1), UsbStreamId(7));
        assert_eq!(e.borrowed.get(&UsbDeviceId(1)), Some(&UsbStreamId(7)));
    }

    /// A device unplugged from the peer's port simply stops being listed — the
    /// peer may never get to say so — and must go from here too.
    #[test]
    fn a_device_dropped_from_the_listing_is_detached() {
        let mut e = attaching_engine();
        e.on_devices(&[offered(1)]);
        let _ = taken(&mut e);
        e.on_claimed(UsbDeviceId(1), UsbStreamId(7));

        e.on_devices(&[]);
        assert!(
            e.borrowed.is_empty(),
            "a device that is gone must not linger"
        );
    }

    /// A refusal must clear the pending ask, or the device is never retried
    /// when the peer that held it lets go.
    #[test]
    fn a_refusal_lets_the_device_be_asked_for_again() {
        let mut e = attaching_engine();
        e.on_devices(&[offered(1)]);
        let _ = taken(&mut e);

        // What `handle_message` does for a refusal: clear the pending ask.
        e.asking.remove(&UsbDeviceId(1));
        e.on_devices(&[offered(1)]);

        assert!(
            taken(&mut e)
                .iter()
                .any(|m| matches!(m, UsbMsg::Claim { device, .. } if *device == UsbDeviceId(1))),
            "a refused device must be asked for again later"
        );
    }

    /// The reason retrying exists: a refusal is rarely permanent, and the
    /// peer's listing only travels when its *device set* changes — which
    /// accepting an elevation prompt is not. Without this a device refused
    /// once would never be asked for again this session.
    #[test]
    fn a_refused_device_is_asked_for_again_later() {
        let mut e = attaching_engine();
        e.on_devices(&[offered(1)]);
        let _ = taken(&mut e);
        // What `handle_message` does for a refusal.
        e.asking.remove(&UsbDeviceId(1));

        // Too soon: the peer has just said no, and asking every tick is noise
        // on a link the cursor shares.
        e.last_retry = Some(std::time::Instant::now());
        e.retry_refused();
        assert!(taken(&mut e).is_empty(), "retried immediately");

        // Once the interval has passed, ask again.
        e.last_retry = Some(std::time::Instant::now() - RETRY_INTERVAL * 2);
        e.retry_refused();
        assert!(
            taken(&mut e)
                .iter()
                .any(|m| matches!(m, UsbMsg::Claim { device, .. } if *device == UsbDeviceId(1))),
            "a refused device must eventually be asked for again"
        );
    }

    /// The loop this cost a flood of log lines to find: a failed attach
    /// releases the device, the release re-announces the peer's listing
    /// because it changed, and the fresh listing asks again — several times a
    /// second, forever. A failure must be remembered, not just reported.
    #[test]
    fn a_device_that_could_not_be_attached_is_not_asked_for_again_at_once() {
        let backend = FakeBackend {
            attaches: true,
            attach_fails: true,
            ..FakeBackend::with(&[])
        };
        let mut e = UsbEngine::new("laptop".into(), peer(1), node_of(backend));

        e.on_devices(&[offered(1)]);
        let _ = taken(&mut e);
        e.on_claimed(UsbDeviceId(1), UsbStreamId(7));
        // The engine hands it back rather than pretending to hold it.
        assert!(taken(&mut e)
            .iter()
            .any(|m| matches!(m, UsbMsg::Release { device } if *device == UsbDeviceId(1))));

        // The peer re-announces because its listing changed. This must not
        // start the cycle again.
        e.on_devices(&[offered(1)]);
        assert!(
            taken(&mut e).is_empty(),
            "asked again immediately after failing"
        );
    }

    /// But the failure is not permanent: a module gets loaded, a permission
    /// granted. After the interval the device is worth another try.
    #[test]
    fn a_failed_attach_is_retried_once_the_interval_passes() {
        let backend = FakeBackend {
            attaches: true,
            attach_fails: true,
            ..FakeBackend::with(&[])
        };
        let mut e = UsbEngine::new("laptop".into(), peer(1), node_of(backend));
        e.on_devices(&[offered(1)]);
        let _ = taken(&mut e);
        e.on_claimed(UsbDeviceId(1), UsbStreamId(7));
        let _ = taken(&mut e);

        e.failed.insert(
            UsbDeviceId(1),
            std::time::Instant::now() - RETRY_INTERVAL * 2,
        );
        e.on_devices(&[offered(1)]);
        assert!(
            taken(&mut e)
                .iter()
                .any(|m| matches!(m, UsbMsg::Claim { device, .. } if *device == UsbDeviceId(1))),
            "a failure must not be permanent"
        );
    }

    /// Retrying must not re-ask for what this machine already has: the peer
    /// answers a duplicate claim with the same stream, but the traffic is
    /// pointless and the log line misleading.
    #[test]
    fn retrying_skips_devices_already_attached() {
        let mut e = attaching_engine();
        e.on_devices(&[offered(1)]);
        let _ = taken(&mut e);
        e.on_claimed(UsbDeviceId(1), UsbStreamId(7));

        e.last_retry = Some(std::time::Instant::now() - RETRY_INTERVAL * 2);
        e.retry_refused();
        assert!(
            taken(&mut e).is_empty(),
            "asked for a device it already has"
        );
    }

    /// A stream that ended twice — a close racing a release — must not detach a
    /// device somebody has since claimed again.
    #[test]
    fn ending_an_unknown_stream_changes_nothing() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        let _ = taken(&mut e);

        e.end_stream(UsbStreamId(9999));

        assert!(
            taken(&mut e).is_empty(),
            "nothing to say about a dead stream"
        );
        assert_eq!(e.exported_count(), 1, "the live attach must survive");
    }
}
