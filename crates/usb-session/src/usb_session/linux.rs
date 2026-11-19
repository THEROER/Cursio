//! The Linux backend: sysfs enumeration, `usbip-host` to export and
//! `vhci-hcd` to attach.

use crate::usb_session::refused::backend_error_to_wire;
use crate::usb_session::refused::held_problem_id;
use crate::usb_session::refused::refusal;
use crate::usb_session::UsbBackend;
use cursio_protocol::usb::UsbDevice;
use cursio_protocol::usb::UsbDeviceId;
use cursio_protocol::usb::UsbError;
use cursio_protocol::usb::UsbStreamId;
use std::collections::HashMap;

/// The Linux backend: sysfs enumeration plus `usbip-host`.
///
/// Lives here rather than in `cursio-usb` because [`UsbBackend`] is policy's
/// vocabulary and the dependency runs this way — `core` knows about `usb`, not
/// the reverse. The crate below stays free of any opinion about who may attach
/// what, which is what lets its tests run without a session.
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
struct BorrowedStream {
    pipe: cursio_usb::KernelPipe,
    _attachment: cursio_usb::import::Attachment,
}

/// One device being served over a socket the kernel holds the far end of.
struct ServedStream {
    busid: String,
    /// Our end of the pair. Dropping it closes the kernel's end, which is what
    /// ends the export — so this must live exactly as long as the attachment.
    pipe: cursio_usb::KernelPipe,
}

impl Default for LinuxUsbBackend {
    fn default() -> Self {
        Self::new()
    }
}

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
            refusal(&e, &busid, self.offers.label(device))
        })?;
        crate::diagnostics::clear(&held_problem_id(&busid));

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
