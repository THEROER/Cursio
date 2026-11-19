//! The Windows backend: `usbipd-win` for enumeration and sharing.

use crate::usb_session::refused::backend_error_to_wire;
use crate::usb_session::refused::held_problem_id;
use crate::usb_session::refused::refusal;
use crate::usb_session::UsbBackend;
use cursio_protocol::usb::UsbDevice;
use cursio_protocol::usb::UsbDeviceId;
use cursio_protocol::usb::UsbError;
use cursio_protocol::usb::UsbStreamId;
use std::collections::HashMap;

/// The Windows backend: `usbipd-win` for both enumeration and sharing.
///
/// Shares [`DeviceOffers`](cursio_usb::DeviceOffers) and the whole policy layer
/// with Linux; what differs is only where the device list comes from and what
/// "export" means. The URB path does not exist here yet — see
/// [`feed`](UsbBackend::feed) — so this machine can *offer* devices and a peer
/// can see them, which is the half that has to work before carrying bytes is
/// worth writing.
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
struct WindowsStream {
    busid: String,
    tunnel: cursio_usb::WinTunnel,
}

impl Default for WindowsUsbBackend {
    fn default() -> Self {
        Self::new()
    }
}

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
        // transfers. Loopback only — see `cursio_usb::wintunnel`. A refusal
        // leaves it shared: `bind` is the machine's standing consent, and
        // unbinding after every failure would spend another UAC prompt on the
        // next try — or, unelevated, fail and log that too.
        let tunnel = cursio_usb::WinTunnel::connect(&busid).map_err(|e| {
            tracing::warn!(%device, busid, error = %e, "cannot reach the local usbipd server");
            refusal(&e, &busid, self.offers.label(device))
        })?;
        crate::diagnostics::clear(&held_problem_id(&busid));

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
        // learns the attachment is over and hands the device back to
        // Windows' own driver. It stays bound, so the next lend needs no
        // prompt.
        self.streams.retain(|_, held| held.busid != busid);
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

/// Say so when usbipd still answers the network.
///
/// The rule closing it is added by the elevated setup that binds devices, so a
/// machine whose devices were all bound before that existed stays exposed
/// until the next prompt; this is what makes that visible rather than silent.
pub(super) fn report_usbipd_exposure() {
    const ID: &str = "usb.usbipd-exposed";
    if cursio_usb::WindowsUsb::new().is_guarded() {
        crate::diagnostics::clear(ID);
        return;
    }
    // A node that elevated at startup can close the port itself; telling the
    // user to paste a command into an administrator shell would be asking
    // them to do what this process has the rights to do.
    if crate::elevate::is_elevated() {
        match cursio_usb::windows::close_to_network() {
            Ok(()) => {
                tracing::info!(
                    rule = cursio_usb::windows::LOOPBACK_GUARD,
                    "closed usbipd's port to the network"
                );
                crate::diagnostics::clear(ID);
                return;
            }
            Err(e) => tracing::warn!(error = %e, "could not close usbipd's port to the network"),
        }
    }
    crate::diagnostics::raise(crate::diagnostics::Problem {
        id: ID.into(),
        severity: crate::diagnostics::Severity::Warning,
        summary: "usbipd accepts connections from the network on port 3240, and anyone who \
                  reaches it can take a shared device"
            .into(),
        hint: Some(format!(
            "the next usb setup prompt closes it; to do it now, in an administrator shell: {}",
            cursio_usb::windows::close_to_network_command()
        )),
        actions: Vec::new(),
        since: std::time::SystemTime::now(),
    });
}
