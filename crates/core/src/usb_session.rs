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
}

/// One attach in progress, from the host's point of view.
struct Attached {
    holder: PeerId,
    stream: UsbStreamId,
}

/// The ownership rules, independent of transport and of any kernel.
pub struct UsbEngine {
    peer_name: String,
    /// Which peer this session talks to. Every claim arriving here came from
    /// this machine, so a claim never has to say who it is from — and cannot
    /// lie about it.
    peer_id: PeerId,
    backend: Box<dyn UsbBackend>,
    /// What this machine has exported, keyed by device. The host's own record,
    /// and the only authority on who holds what.
    attached: HashMap<UsbDeviceId, Attached>,
    /// Queued for the next pump, because `handle_message` is synchronous while
    /// sending is not.
    outbound: Vec<UsbMsg>,
    /// Whether the device list has gone out on this link yet.
    announced: bool,
}

impl UsbEngine {
    pub fn new(peer_name: String, peer_id: PeerId, backend: Box<dyn UsbBackend>) -> Self {
        Self {
            peer_name,
            peer_id,
            backend,
            attached: HashMap::new(),
            outbound: Vec::new(),
            announced: false,
        }
    }

    /// The device list as this peer should see it: every device, each carrying
    /// who holds it.
    ///
    /// Built from the backend's view each time rather than cached, so a device
    /// unplugged between announcements simply is not in the next one.
    fn listing(&mut self) -> Vec<UsbDevice> {
        let attached: HashMap<UsbDeviceId, PeerId> = self
            .attached
            .iter()
            .map(|(id, a)| (*id, a.holder))
            .collect();
        let mut devices = self.backend.devices();
        for device in &mut devices {
            device.holder = attached.get(&device.id).copied();
        }
        devices
    }

    /// Answer a peer asking for a device.
    ///
    /// Refusing costs nothing and tells the user why; granting silently to a
    /// device someone else holds would detach it from under them.
    fn on_claim(&mut self, device: UsbDeviceId, reason: &str) {
        if let Some(existing) = self.attached.get(&device) {
            // Already ours to give away? Then the peer asking is the one who
            // has it, and re-claiming is a no-op rather than an error: a
            // reconnecting peer re-asks for what it believes it still holds.
            let error = match existing.holder == self.peer_id {
                true => {
                    let stream = existing.stream;
                    self.outbound.push(UsbMsg::Claimed { device, stream });
                    return;
                }
                false => UsbError::InUse(existing.holder),
            };
            self.outbound.push(UsbMsg::Refused { device, error });
            return;
        }

        if !self.backend.devices().iter().any(|d| d.id == device) {
            self.outbound.push(UsbMsg::Refused {
                device,
                error: UsbError::Gone,
            });
            return;
        }

        match self.backend.export(device) {
            Ok(stream) => {
                tracing::info!(
                    peer = %self.peer_name, %device, %reason,
                    "handing a device to a peer"
                );
                self.attached.insert(
                    device,
                    Attached {
                        holder: self.peer_id,
                        stream,
                    },
                );
                self.outbound.push(UsbMsg::Claimed { device, stream });
                // The listing changed: whoever else is watching needs to see it
                // is taken, and this peer needs its own view corrected too.
                self.announced = false;
            }
            Err(error) => {
                tracing::warn!(peer = %self.peer_name, %device, %error, "refused a device");
                self.outbound.push(UsbMsg::Refused { device, error });
            }
        }
    }

    /// Let go of a device, if this peer is the one holding it.
    ///
    /// A release from anyone else is ignored, not honoured. There is nothing
    /// useful to say back — the sender does not hold it, so nothing changed for
    /// them — and obeying would detach a bus from whoever actually is using it.
    fn on_release(&mut self, device: UsbDeviceId) {
        let Some(existing) = self.attached.get(&device) else {
            return;
        };
        if existing.holder != self.peer_id {
            tracing::warn!(
                peer = %self.peer_name, %device,
                "ignoring a release from a peer that does not hold the device"
            );
            return;
        }
        let stream = existing.stream;
        self.attached.remove(&device);
        self.backend.unexport(device);
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
        let owned = self
            .attached
            .iter()
            .find(|(_, a)| a.stream == stream && a.holder == self.peer_id)
            .map(|(id, _)| *id);
        let Some(device) = owned else {
            return;
        };
        if let Err(error) = self.backend.feed(stream, bytes) {
            tracing::warn!(peer = %self.peer_name, %device, %error, "urb stream failed");
            self.attached.remove(&device);
            self.backend.unexport(device);
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
        let ours: Vec<UsbDeviceId> = self
            .attached
            .iter()
            .filter(|(_, a)| a.holder == self.peer_id)
            .map(|(id, _)| *id)
            .collect();
        for device in ours {
            self.attached.remove(&device);
            self.backend.unexport(device);
            tracing::info!(
                peer = %self.peer_name, %device,
                "reclaiming a device whose holder went away"
            );
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
        let Some((&device, _)) = self
            .attached
            .iter()
            .find(|(_, attached)| attached.stream == stream)
        else {
            return;
        };
        self.attached.remove(&device);
        self.backend.unexport(device);
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

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.pump(tx).await
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if !self.announced {
            self.announced = true;
            let devices = self.listing();
            self.outbound.push(UsbMsg::Devices(devices));
        }

        // Bytes the local kernel produced, then the streams that ended — in
        // that order, because a close and the data preceding it arrive
        // together and reversing them drops the end of every transfer.
        let mut produced = Vec::new();
        self.backend.collect(&mut produced);
        for (stream, bytes) in produced {
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
        for stream in self.backend.ended() {
            self.end_stream(stream);
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
            // Replies belong to the attaching half, which is not built yet.
            // Claimed/Refused/Detached/Devices arriving here means this machine
            // asked for something, and nothing in this engine asks.
            UsbMsg::Devices(_)
            | UsbMsg::Claimed { .. }
            | UsbMsg::Refused { .. }
            | UsbMsg::Detached { .. } => {}
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
    /// Bus ids handed out, so a release names the same device the claim did.
    streams: HashMap<UsbStreamId, String>,
    next_stream: u64,
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

        let stream = UsbStreamId(self.next_stream);
        self.next_stream += 1;
        self.streams.insert(stream, busid);
        Ok(stream)
    }

    fn unexport(&mut self, device: UsbDeviceId) {
        let Some(busid) = self.offers.busid(device).map(str::to_string) else {
            return;
        };
        self.streams.retain(|_, held| held != &busid);
        self.usbipd.unbind(&busid);
    }

    fn feed(&mut self, stream: UsbStreamId, _bytes: &[u8]) -> Result<(), UsbError> {
        // `usbipd-win` owns its own socket to the driver and offers no way to
        // hand it one of ours — the substitution `pipe.rs` performs on Linux
        // has no equivalent here. Refusing names the gap; dropping the bytes
        // would look like a device that attaches and then does nothing.
        match self.streams.contains_key(&stream) {
            true => Err(UsbError::Backend(
                "windows cannot carry urbs over the mesh channel yet".into(),
            )),
            false => Err(UsbError::Gone),
        }
    }

    fn collect(&mut self, _out: &mut Vec<(UsbStreamId, Vec<u8>)>) {}
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

/// Builds the USB module for a link.
#[cfg(feature = "usb")]
pub struct UsbFactory {
    pub enabled: bool,
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
            if !LinuxUsbBackend::new().is_available() {
                tracing::warn!(
                    "usb sharing is off: the usbip-host module is not loaded \
                     (sudo modprobe usbip-host)"
                );
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
            Box::new(LinuxUsbBackend::new()),
        )))
    }

    #[cfg(target_os = "windows")]
    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(UsbEngine::new(
            ctx.peer_name().to_string(),
            ctx.peer.peer_id,
            Box::new(WindowsUsbBackend::new()),
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

    /// A backend with no kernel behind it: every ownership race is decided in
    /// the engine, so none of them need a real device.
    #[derive(Default)]
    struct FakeBackend {
        present: Vec<UsbDeviceId>,
        exported: Vec<UsbDeviceId>,
        /// What `export` should fail with, when the test wants a refusal.
        refuse: Option<UsbError>,
        /// What `feed` should fail with, to exercise a mid-stream failure.
        feed_fails: Option<UsbError>,
        fed: Vec<(UsbStreamId, Vec<u8>)>,
        next_stream: u64,
        /// What the local kernel has produced, for the next `collect`.
        produced: Vec<(UsbStreamId, Vec<u8>)>,
        /// Streams the kernel has closed, for the next `ended`.
        closed: Vec<UsbStreamId>,
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
            self.next_stream += 1;
            self.exported.push(device);
            Ok(UsbStreamId(self.next_stream))
        }

        fn unexport(&mut self, device: UsbDeviceId) {
            self.exported.retain(|d| *d != device);
        }

        fn collect(&mut self, out: &mut Vec<(UsbStreamId, Vec<u8>)>) {
            out.append(&mut self.produced);
        }

        fn ended(&mut self) -> Vec<UsbStreamId> {
            std::mem::take(&mut self.closed)
        }

        fn feed(&mut self, stream: UsbStreamId, bytes: &[u8]) -> Result<(), UsbError> {
            if let Some(error) = self.feed_fails.clone() {
                return Err(error);
            }
            self.fed.push((stream, bytes.to_vec()));
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
            Box::new(FakeBackend::with(devices)),
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

    /// The race the host exists to settle: two peers ask for one device, and
    /// exactly one gets it. The second is told who has it, not merely "no".
    #[test]
    fn a_second_peer_is_refused_and_told_who_holds_it() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        taken(&mut e);

        // The same engine, but the claim now arrives from a different session.
        e.peer_id = peer(2);
        e.on_claim(UsbDeviceId(1), "also adb");

        match taken(&mut e).as_slice() {
            [UsbMsg::Refused { error, .. }] => {
                assert_eq!(*error, UsbError::InUse(peer(1)), "must name the holder");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
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
            e.attached.contains_key(&UsbDeviceId(1)),
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
        assert!(e.attached.is_empty(), "the device is free again");
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
            Box::new(FakeBackend {
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
            e.attached.contains_key(&UsbDeviceId(1)),
            "the attach is untouched"
        );
        assert!(taken(&mut e).is_empty(), "and nothing is said about it");
    }

    /// A stream that fails mid-transfer ends the attach and says why, rather
    /// than leaving the holder's kernel waiting on transfers that never come.
    #[test]
    fn a_failing_stream_detaches_with_a_reason() {
        let mut e = UsbEngine::new("desktop".into(), peer(1), Box::new(FakeBackend::with(&[1])));
        e.on_claim(UsbDeviceId(1), "adb");
        let stream = match taken(&mut e).as_slice() {
            [UsbMsg::Claimed { stream, .. }] => *stream,
            other => panic!("expected a grant, got {other:?}"),
        };

        // Break the backend now that the attach exists.
        e.backend = Box::new(FakeBackend {
            feed_fails: Some(UsbError::Unplugged),
            ..FakeBackend::with(&[1])
        });
        e.on_data(stream, b"urb");

        assert!(matches!(
            taken(&mut e).as_slice(),
            [UsbMsg::Detached {
                error: Some(UsbError::Unplugged),
                ..
            }]
        ));
        assert!(e.attached.is_empty(), "a broken attach does not linger");
    }

    /// A holder that goes away can never send the release that would free its
    /// device, so the link ending has to do it.
    #[test]
    fn a_device_is_reclaimed_when_its_holder_goes_away() {
        let mut e = engine(&[1]);
        e.on_claim(UsbDeviceId(1), "adb");
        taken(&mut e);

        e.release_all();

        assert!(e.attached.is_empty(), "the device came back");
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
            e.attached.is_empty(),
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
        assert_eq!(e.attached.len(), 1, "the live attach must survive");
    }
}
