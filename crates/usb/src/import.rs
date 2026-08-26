//! Attaching a peer's device here, through `vhci-hcd`.
//!
//! The mirror of [`crate::export`]. That half gives a local device to
//! `usbip-host` so somebody else can drive it; this half tells the kernel's
//! *virtual* host controller that a device exists on the other end of a socket,
//! after which the device appears in `lsusb` and its driver binds exactly as if
//! it were plugged in here.
//!
//! ## The socket is ours, not the kernel's
//!
//! `usbip attach` opens a TCP connection to port 3240 on another machine and
//! hands the kernel that file descriptor. Cursio hands it a **socketpair**
//! instead: one end goes to `vhci-hcd`, the other stays in this process and its
//! bytes ride the mesh channel. The kernel cannot tell the difference — it sees
//! a socket carrying USB/IP — and the difference is the whole point, because it
//! means no listening port, and every attach authenticated as a peer that
//! completed SPAKE2 pairing. Invariant 5 in `docs/usb-design.md`.
//!
//! ## `attach` is a single write, and its format is positional
//!
//! ```text
//! <port> <sockfd> <busnum-devnum> <speed>
//! ```
//!
//! The kernel parses one write as one command, so this must be a single
//! `write` call rather than anything buffered. `devid` is the *remote*
//! machine's bus and device number packed into one 32-bit value: the kernel
//! only echoes it back in messages, but a wrong value makes those messages name
//! a device nobody can find.
//!
//! ## A port is claimed by writing to it, not by asking
//!
//! There is no "give me a free port" call. `status` lists every port with its
//! state, and a free one is claimed by being the process that writes to
//! `attach` first. Two processes racing both write, and the loser's write
//! fails — which is why a failed attach retries the next free port rather than
//! giving up.

use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

use crate::UsbBackendError;

/// Where `vhci-hcd` publishes its control files.
///
/// The platform device the module registers on load. The trailing `.0` is its
/// instance number: a machine has one virtual host controller unless somebody
/// asked the module for more, and `usbip` itself assumes the same.
const VHCI_ROOT: &str = "/sys/devices/platform/vhci_hcd.0";

/// USB speed codes, as `vhci-hcd` writes and reads them.
///
/// The kernel's own numbering (`USB_SPEED_*` in `linux/usb/ch9.h`), not an
/// invention: it is written into `attach` verbatim and a wrong value makes the
/// device enumerate at the wrong rate or not at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Low,
    Full,
    High,
    Super,
}

impl Speed {
    /// The number the kernel expects.
    pub fn code(self) -> u32 {
        match self {
            Self::Low => 1,
            Self::Full => 2,
            Self::High => 3,
            Self::Super => 5,
        }
    }

    /// Parse what sysfs reports for a device (`1.5`, `12`, `480`, `5000`).
    ///
    /// Unrecognised values become `High` rather than an error: a device that
    /// enumerates at the wrong speed is a device with a problem, while one
    /// refused for having an unfamiliar speed string is a device that never
    /// appears at all — and new speeds keep being added.
    pub fn from_sysfs(value: &str) -> Self {
        match value.trim() {
            "1.5" => Self::Low,
            "12" => Self::Full,
            "480" => Self::High,
            v if v.parse::<f32>().is_ok_and(|n| n >= 5000.0) => Self::Super,
            _ => Self::High,
        }
    }
}

/// One attached device, and the port it holds.
///
/// Detaches on drop: a port left attached to a socket nobody is reading is a
/// device that hangs every program touching it, and the kernel will not reclaim
/// it on its own.
#[derive(Debug)]
pub struct Attachment {
    port: u32,
    vhci: PathBuf,
}

impl Attachment {
    /// Which vhci port this holds.
    pub fn port(&self) -> u32 {
        self.port
    }

    /// Detach explicitly, reporting whether the kernel accepted it.
    ///
    /// [`Drop`] does the same thing and ignores the answer; this exists for the
    /// caller that wants to log a failure.
    pub fn detach(self) -> Result<(), UsbBackendError> {
        let result = detach_port(&self.vhci, self.port);
        // Already detached by the explicit call; skip the destructor's attempt.
        std::mem::forget(self);
        result
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if let Err(e) = detach_port(&self.vhci, self.port) {
            tracing::warn!(port = self.port, error = %e, "could not detach vhci port");
        }
    }
}

/// Attaches remote devices to this machine's virtual host controller.
#[derive(Debug, Clone)]
pub struct Importer {
    vhci: PathBuf,
}

impl Default for Importer {
    fn default() -> Self {
        Self::new()
    }
}

impl Importer {
    pub fn new() -> Self {
        Self {
            vhci: PathBuf::from(VHCI_ROOT),
        }
    }

    /// Point at a different tree, for tests.
    #[cfg(test)]
    fn rooted_at(vhci: impl Into<PathBuf>) -> Self {
        Self { vhci: vhci.into() }
    }

    /// Whether this machine can attach a peer's device at all.
    ///
    /// `vhci-hcd` is not loaded by default anywhere, so this is the ordinary
    /// answer on a fresh machine.
    pub fn is_available(&self) -> bool {
        self.vhci.join("status").is_file()
    }

    /// Ports the kernel reports as free, lowest first.
    ///
    /// The `status` file is a header line then one line per port, whose first
    /// two fields are the port number and its state — state 4 (`VDEV_ST_NULL`)
    /// means free. Parsed positionally because that is what the format is;
    /// a port whose line cannot be read is treated as busy, since claiming one
    /// that is actually in use detaches somebody else's device.
    pub fn free_ports(&self) -> Result<Vec<u32>, UsbBackendError> {
        let status = std::fs::read_to_string(self.vhci.join("status")).map_err(|e| {
            UsbBackendError::DriverMissing(format!(
                "cannot read vhci status: {e}; run: sudo modprobe vhci-hcd"
            ))
        })?;
        Ok(parse_free_ports(&status))
    }

    /// Hand `socket` to the kernel as the far end of a device.
    ///
    /// The socket must stay open for the life of the attachment: the kernel
    /// holds the same file description, and closing this end tears the device
    /// down. That is why the caller keeps it rather than passing ownership.
    ///
    /// A busy port is retried rather than reported: there is no reservation
    /// call, so the only way to learn a port is free is to have claimed it.
    pub fn attach(
        &self,
        socket: &impl AsRawFd,
        devid: u32,
        speed: Speed,
    ) -> Result<Attachment, UsbBackendError> {
        if !self.is_available() {
            return Err(UsbBackendError::DriverMissing(
                "vhci-hcd is not loaded; run: sudo modprobe vhci-hcd".into(),
            ));
        }

        let ports = self.free_ports()?;
        if ports.is_empty() {
            return Err(UsbBackendError::Enumerate(
                "every vhci port is in use; detach a device first".into(),
            ));
        }

        let mut last = None;
        for port in ports {
            match self.attach_to(port, socket.as_raw_fd(), devid, speed) {
                Ok(()) => {
                    tracing::info!(port, devid, "attached a peer device");
                    return Ok(Attachment {
                        port,
                        vhci: self.vhci.clone(),
                    });
                }
                // Another process claimed it between the listing and the write.
                Err(e) => {
                    tracing::debug!(port, error = %e, "vhci port taken; trying the next");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            UsbBackendError::Enumerate("no vhci port accepted the attach".into())
        }))
    }

    /// The single positional write the kernel parses as one command.
    fn attach_to(
        &self,
        port: u32,
        fd: RawFd,
        devid: u32,
        speed: Speed,
    ) -> Result<(), UsbBackendError> {
        let command = format!("{port} {fd} {devid} {}", speed.code());
        write_sysfs(&self.vhci.join("attach"), &command)
    }
}

/// Free a port, whoever is holding it.
fn detach_port(vhci: &Path, port: u32) -> Result<(), UsbBackendError> {
    write_sysfs(&vhci.join("detach"), &port.to_string())
}

/// Pack a remote bus and device number the way the kernel expects.
///
/// The high 16 bits are the bus, the low 16 the device. Only echoed back in
/// kernel messages, but a wrong value makes those messages name a device
/// nobody can find, which is a bad thing to discover while debugging.
pub fn devid(busnum: u16, devnum: u16) -> u32 {
    (u32::from(busnum) << 16) | u32::from(devnum)
}

/// Which ports `status` reports as free.
fn parse_free_ports(status: &str) -> Vec<u32> {
    /// `VDEV_ST_NULL` in `drivers/usb/usbip/vhci.h`: nothing attached.
    const ST_NULL: &str = "004";

    let mut ports: Vec<u32> = status
        .lines()
        // The first line is a header naming the columns.
        .skip(1)
        .filter_map(|line| {
            // `hs 0000 004 …` — the first field is the hub speed class, not a
            // port. Reading it as one made every line unparseable and so every
            // port busy, which fails as "no free port" rather than as a wrong
            // answer: an attach that never happens instead of one that steals
            // somebody's device.
            let mut fields = line.split_whitespace();
            let _hub = fields.next()?;
            let port = fields.next()?.parse::<u32>().ok()?;
            let state = fields.next()?;
            // Zero-padded in the kernel's own format string, but compared
            // numerically so a release that stops padding still parses.
            let free = state == ST_NULL || state.parse::<u32>().ok() == Some(4);
            free.then_some(port)
        })
        .collect();
    ports.sort_unstable();
    ports
}

/// Write to a sysfs attribute in one call, naming the file in every error.
fn write_sysfs(path: &Path, value: &str) -> Result<(), UsbBackendError> {
    match std::fs::write(path, value) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(
            UsbBackendError::NeedsPrivilege(format!("cannot write {}: {e}", path.display())),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(UsbBackendError::DriverMissing(
            format!("{} does not exist: {e}", path.display()),
        )),
        Err(e) => Err(UsbBackendError::Enumerate(format!(
            "cannot write {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's real `status` layout, header included.
    const STATUS: &str = "\
hub port sta spd dev      sockfd local_busid
hs  0000 004 000 00000000 000000 0-0
hs  0001 006 003 00020002 000003 1-5
hs  0002 004 000 00000000 000000 0-0
";

    fn fixture() -> (tempfile::TempDir, Importer) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("status"), STATUS).expect("status");
        std::fs::write(dir.path().join("attach"), "").expect("attach");
        std::fs::write(dir.path().join("detach"), "").expect("detach");
        let importer = Importer::rooted_at(dir.path());
        (dir, importer)
    }

    /// Claiming a port that is actually in use detaches somebody else's device,
    /// so only state 4 counts as free.
    #[test]
    fn only_ports_in_the_null_state_are_free() {
        assert_eq!(parse_free_ports(STATUS), vec![0, 2]);
    }

    /// The header names the columns and is not a port.
    #[test]
    fn the_status_header_is_not_read_as_a_port() {
        assert!(!parse_free_ports(STATUS).contains(&u32::MAX));
        assert_eq!(
            parse_free_ports("hub port sta spd dev sockfd local_busid\n"),
            vec![]
        );
    }

    /// A line that cannot be parsed must be treated as busy: guessing free
    /// costs somebody their device.
    #[test]
    fn an_unparseable_line_is_not_offered_as_free() {
        let status = "header\nhs 0000 004 000 00000000 000000 0-0\ngarbage\n\n";
        assert_eq!(parse_free_ports(status), vec![0]);
    }

    /// The module is not loaded by default anywhere, and that must read as a
    /// missing driver with the modprobe to run.
    #[test]
    fn an_unloaded_module_is_reported_with_the_command_to_fix_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let importer = Importer::rooted_at(dir.path());
        assert!(!importer.is_available());

        let error = importer.free_ports().expect_err("should fail");
        match error {
            UsbBackendError::DriverMissing(message) => {
                assert!(message.contains("modprobe"), "unhelpful: {message}");
            }
            other => panic!("expected a missing driver, got {other:?}"),
        }
    }

    /// The write is positional and the kernel parses it as one command, so the
    /// exact shape is part of the contract.
    #[test]
    fn attaching_writes_the_port_fd_devid_and_speed() {
        let (dir, importer) = fixture();
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("socket");
        let fd = socket.as_raw_fd();

        let attachment = importer
            .attach(&socket, devid(2, 7), Speed::High)
            .expect("attach");

        let written = std::fs::read_to_string(dir.path().join("attach")).expect("read");
        assert_eq!(written, format!("0 {fd} {} 3", devid(2, 7)));
        assert_eq!(attachment.port(), 0);
        // Dropping detaches, so the port is not left holding a dead socket.
        drop(attachment);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("detach")).expect("read"),
            "0"
        );
    }

    /// The kernel only echoes devid back, but a wrong value names a device
    /// nobody can find in every later message.
    #[test]
    fn a_devid_packs_the_bus_above_the_device() {
        assert_eq!(devid(1, 5), 0x0001_0005);
        assert_eq!(devid(0, 0), 0);
        assert_eq!(devid(0xFFFF, 0xFFFF), 0xFFFF_FFFF);
    }

    /// Speeds are the kernel's own numbering; getting one wrong makes a device
    /// enumerate at the wrong rate.
    #[test]
    fn speeds_use_the_kernels_numbering() {
        assert_eq!(Speed::from_sysfs("1.5").code(), 1);
        assert_eq!(Speed::from_sysfs("12").code(), 2);
        assert_eq!(Speed::from_sysfs("480").code(), 3);
        assert_eq!(Speed::from_sysfs("5000").code(), 5);
        assert_eq!(Speed::from_sysfs("10000").code(), 5);
    }

    /// A speed string this build has never seen must still attach: new speeds
    /// keep being added, and refusing one means the device never appears.
    #[test]
    fn an_unfamiliar_speed_still_attaches() {
        assert_eq!(Speed::from_sysfs("wat"), Speed::High);
        assert_eq!(Speed::from_sysfs(""), Speed::High);
    }

    /// With no free port the caller needs to be told to detach something,
    /// rather than seeing a write failure from the last port tried.
    #[test]
    fn a_full_controller_says_to_detach_something() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("status"),
            "hub port sta spd dev sockfd local_busid\nhs 0000 006 003 00020002 000003 1-5\n",
        )
        .expect("status");
        let importer = Importer::rooted_at(dir.path());
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("socket");

        let error = importer
            .attach(&socket, devid(1, 1), Speed::High)
            .expect_err("should refuse");
        assert!(error.to_string().contains("detach"), "unhelpful: {error}");
    }
}
