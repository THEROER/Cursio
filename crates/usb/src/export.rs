//! Handing a device to `usbip-host`, so a peer can attach it.
//!
//! Enumeration ([`crate::linux`]) is ordinary file reads. This is the half that
//! changes the machine: the device is taken away from whatever driver has it —
//! `usbhid`, `btusb`, `usb-storage` — and given to `usbip-host`, which is what
//! makes its transfers available to somebody else.
//!
//! ## sysfs, not the `usbip` command
//!
//! Same reason as enumeration: `usbip bind` is a wrapper over these exact
//! writes, and its output is formatted for people. Doing the writes directly
//! removes a text format nobody promised to keep, and removes the need for the
//! binary to be installed at all.
//!
//! Three files do the whole job:
//!
//! - `/sys/bus/usb/drivers/<current>/unbind` ← the bus id, to take the device
//!   away from the driver using it.
//! - `/sys/bus/usb/drivers/usbip-host/match_busid` ← `add <busid>`, so the
//!   driver will accept this device (it binds nothing by default, which is what
//!   keeps loading the module harmless).
//! - `/sys/bus/usb/drivers/usbip-host/bind` ← the bus id.
//!
//! The order is load-bearing. Binding before the match is registered fails, and
//! registering the match before unbinding leaves the device with its old driver
//! — in both cases the device stays where it was and nothing says why.
//!
//! ## Unbinding is destructive, and that is the point
//!
//! Between the unbind and the bind the device belongs to nothing: a keyboard
//! stops typing, a stick stops being mounted. That is inherent — a device
//! cannot be driven in two places — but it is why [`unexport`] restores the
//! device rather than leaving it bound, and why a failed export undoes what it
//! managed to do rather than leaving a dead port.
//!
//! ## Root
//!
//! Every write here needs it: the files are mode 0200, owned by root. This
//! module does not escalate — it reports [`UsbBackendError::NeedsPrivilege`]
//! naming the file, and the caller decides whether that means polkit, a prompt
//! or a refusal. A daemon running as root to avoid the question would be a much
//! larger thing to have made.

use std::path::{Path, PathBuf};

use crate::UsbBackendError;

/// Where the kernel publishes USB drivers.
const USB_DRIVERS: &str = "/sys/bus/usb/drivers";

/// Where the kernel publishes USB devices.
const USB_DEVICES: &str = "/sys/bus/usb/devices";

/// The driver a device must end up on to be shareable.
const USBIP_HOST: &str = "usbip-host";

/// Exports devices by moving them onto `usbip-host`.
///
/// Holds the sysfs roots as fields so tests can point them at a fixture tree:
/// every ordering rule worth getting right is decided by which file is written
/// when, and none of that needs a kernel.
#[derive(Debug, Clone)]
pub struct Exporter {
    drivers: PathBuf,
    devices: PathBuf,
}

impl Default for Exporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Exporter {
    pub fn new() -> Self {
        Self {
            drivers: PathBuf::from(USB_DRIVERS),
            devices: PathBuf::from(USB_DEVICES),
        }
    }

    /// Point at a different tree, for tests.
    #[cfg(test)]
    fn rooted_at(drivers: impl Into<PathBuf>, devices: impl Into<PathBuf>) -> Self {
        Self {
            drivers: drivers.into(),
            devices: devices.into(),
        }
    }

    /// Whether the kernel's exporting half is available at all.
    ///
    /// The driver directory exists only once `usbip-host` is loaded, and the
    /// module is not loaded by default on any distribution. Checked before
    /// advertising the module rather than discovered on the first claim, so a
    /// peer is never offered a device that cannot actually be handed over.
    pub fn is_available(&self) -> bool {
        self.drivers.join(USBIP_HOST).is_dir()
    }

    /// Which driver currently has `busid`, if any.
    ///
    /// `None` covers both "no driver" (an unclaimed device) and "cannot tell",
    /// which behave identically here: there is nothing to unbind from.
    pub fn current_driver(&self, busid: &str) -> Option<String> {
        let link = self.devices.join(busid).join("driver");
        let target = std::fs::read_link(&link).ok()?;
        Some(target.file_name()?.to_string_lossy().into_owned())
    }

    /// Move `busid` onto `usbip-host`.
    ///
    /// Idempotent: a device already on `usbip-host` is left alone and reported
    /// as success, because a peer re-claiming after a reconnect is the ordinary
    /// case and re-binding would tear down a working attach to rebuild it.
    ///
    /// On failure the device is put back where it was. A half-exported device
    /// is one nobody drives — not the local machine, because it was unbound,
    /// and not the peer, because the export never completed.
    pub fn export(&self, busid: &str) -> Result<(), UsbBackendError> {
        if !self.is_available() {
            return Err(UsbBackendError::DriverMissing(format!(
                "{USBIP_HOST} is not loaded; run: sudo modprobe {USBIP_HOST}"
            )));
        }
        if !self.devices.join(busid).is_dir() {
            return Err(UsbBackendError::NoSuchDevice(busid.to_string()));
        }

        let previous = self.current_driver(busid);
        if previous.as_deref() == Some(USBIP_HOST) {
            tracing::debug!(busid, "already exported");
            return Ok(());
        }

        // The match must be registered before the bind, and the device must be
        // free before either. `usbip-host` binds nothing it has not been told
        // about, which is what makes loading the module harmless.
        if let Some(driver) = &previous {
            self.write_driver(driver, "unbind", busid)?;
        }
        if let Err(e) = self.write_driver(USBIP_HOST, "match_busid", &format!("add {busid}")) {
            self.restore(busid, previous.as_deref());
            return Err(e);
        }
        if let Err(e) = self.write_driver(USBIP_HOST, "bind", busid) {
            // Leave no stale match behind: the next export of this device would
            // otherwise find one already registered and read the failure as
            // success.
            let _ = self.write_driver(USBIP_HOST, "match_busid", &format!("del {busid}"));
            self.restore(busid, previous.as_deref());
            return Err(e);
        }

        tracing::info!(busid, previous = ?previous, "exported to usbip-host");
        Ok(())
    }

    /// Give `busid` back to this machine.
    ///
    /// Best-effort by design: this runs when a peer detaches, when a session
    /// drops, and on shutdown, and none of those can do anything useful with an
    /// error. What they can do is leave the device unusable by giving up early,
    /// so every step is attempted even when an earlier one failed.
    ///
    /// The device is not re-bound to its old driver explicitly: removing the
    /// match and unbinding makes the kernel re-probe, and the driver that
    /// claimed it before claims it again. Naming the old driver ourselves would
    /// be a guess that goes wrong exactly when a device has been reconfigured.
    pub fn unexport(&self, busid: &str) {
        if self.current_driver(busid).as_deref() != Some(USBIP_HOST) {
            return;
        }
        if let Err(e) = self.write_driver(USBIP_HOST, "unbind", busid) {
            tracing::warn!(busid, error = %e, "could not unbind from usbip-host");
        }
        if let Err(e) = self.write_driver(USBIP_HOST, "match_busid", &format!("del {busid}")) {
            tracing::warn!(busid, error = %e, "could not clear the usbip match");
        }
        // Re-probe, so whatever drove it before takes it back.
        if let Err(e) = self.write_driver("usb", "bind", busid) {
            tracing::debug!(busid, error = %e, "device not re-bound; it may need a replug");
        }
        tracing::info!(busid, "returned to this machine");
    }

    /// Put a device back after a failed export, as far as that is possible.
    fn restore(&self, busid: &str, previous: Option<&str>) {
        let Some(driver) = previous else {
            return;
        };
        if let Err(e) = self.write_driver(driver, "bind", busid) {
            tracing::warn!(
                busid,
                driver,
                error = %e,
                "could not return the device to its driver; it may need a replug"
            );
        }
    }

    /// Hand `usbip-host` the socket it should serve this device over.
    ///
    /// The attribute appears on the *device*, not the driver, and only once the
    /// bind has happened — which is why this is a second step rather than part
    /// of [`export`](Self::export). Writing a descriptor number here is what
    /// starts the kernel serving; until then the device is bound to
    /// `usbip-host` and idle.
    ///
    /// The socket must stay open for as long as the device is served. The
    /// kernel holds the same file description, so closing this end is what ends
    /// the export — deliberately, since it means a dropped session cannot leave
    /// a device attached to nobody.
    ///
    /// The attribute is **write-only** (`DEVICE_ATTR_WO` in the kernel's
    /// `stub_dev.c`), so there is no reading back what was handed over: a
    /// successful write is the only confirmation available, and code that
    /// verified by reading would find an empty value and conclude the export
    /// had failed.
    pub fn serve(&self, busid: &str, fd: std::os::fd::RawFd) -> Result<(), UsbBackendError> {
        write_sysfs(&self.sockfd_path(busid), &fd.to_string())
    }

    /// Stop serving, without unbinding.
    ///
    /// `-1` is the kernel's own "no socket" value. Used when an attachment ends
    /// but the device should stay exported for the next claim, which saves the
    /// unbind/rebind cycle a peer reconnecting would otherwise pay.
    pub fn stop_serving(&self, busid: &str) -> Result<(), UsbBackendError> {
        write_sysfs(&self.sockfd_path(busid), "-1")
    }

    fn sockfd_path(&self, busid: &str) -> PathBuf {
        self.devices.join(busid).join("usbip_sockfd")
    }

    /// Write one value to a driver's control file.
    ///
    /// The error distinguishes "needs root" from everything else, because they
    /// are different problems for the user: one is a permission to grant, the
    /// other is a device or a kernel that will not cooperate however the
    /// process is run.
    fn write_driver(&self, driver: &str, file: &str, value: &str) -> Result<(), UsbBackendError> {
        let path = self.drivers.join(driver).join(file);
        write_sysfs(&path, value)
    }
}

/// Write to a sysfs attribute, naming the file in every error.
///
/// sysfs writes must be a single `write` call — the kernel parses one write as
/// one command, and a buffered writer splitting it would deliver two
/// meaningless fragments. `std::fs::write` is exactly one call, which is why it
/// is used rather than a `File` and `write_all`.
fn write_sysfs(path: &Path, value: &str) -> Result<(), UsbBackendError> {
    match std::fs::write(path, value) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(
            UsbBackendError::NeedsPrivilege(format!("cannot write {}: {e}", path.display())),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(UsbBackendError::NoSuchDevice(
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

    /// A sysfs-shaped tree: drivers with writable control files, devices with a
    /// `driver` symlink. Writes are recorded by their content, which is what
    /// every ordering assertion below reads.
    struct Fixture {
        _dir: tempfile::TempDir,
        drivers: PathBuf,
        devices: PathBuf,
    }

    impl Fixture {
        fn new(drivers: &[&str], device: &str, bound_to: Option<&str>) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let drivers_root = dir.path().join("drivers");
            let devices_root = dir.path().join("devices");

            for driver in drivers {
                let d = drivers_root.join(driver);
                std::fs::create_dir_all(&d).expect("driver dir");
                for file in ["bind", "unbind", "match_busid"] {
                    std::fs::write(d.join(file), "").expect("control file");
                }
            }
            std::fs::create_dir_all(devices_root.join(device)).expect("device dir");
            if let Some(driver) = bound_to {
                std::os::unix::fs::symlink(
                    drivers_root.join(driver),
                    devices_root.join(device).join("driver"),
                )
                .expect("driver link");
            }

            Self {
                _dir: dir,
                drivers: drivers_root,
                devices: devices_root,
            }
        }

        fn exporter(&self) -> Exporter {
            Exporter::rooted_at(&self.drivers, &self.devices)
        }

        fn written(&self, driver: &str, file: &str) -> String {
            std::fs::read_to_string(self.drivers.join(driver).join(file)).unwrap_or_default()
        }
    }

    /// The module is not loaded by default anywhere, so "no driver directory"
    /// is the ordinary state and must be reported as a missing driver rather
    /// than as a missing device.
    #[test]
    fn an_unloaded_module_is_reported_as_a_missing_driver() {
        let fixture = Fixture::new(&["usbhid"], "1-5", Some("usbhid"));
        let error = fixture.exporter().export("1-5").expect_err("should refuse");
        assert!(
            matches!(error, UsbBackendError::DriverMissing(_)),
            "got {error:?}"
        );
        assert!(!fixture.exporter().is_available());
    }

    /// Binding before the match is registered fails in the kernel, so the two
    /// writes must happen in this order and the device must be free first.
    #[test]
    fn exporting_unbinds_then_matches_then_binds() {
        let fixture = Fixture::new(&["usbhid", USBIP_HOST], "1-5", Some("usbhid"));
        fixture.exporter().export("1-5").expect("export");

        assert_eq!(fixture.written("usbhid", "unbind"), "1-5");
        assert_eq!(fixture.written(USBIP_HOST, "match_busid"), "add 1-5");
        assert_eq!(fixture.written(USBIP_HOST, "bind"), "1-5");
    }

    /// A device nothing drives is common — an unclaimed interface, a device
    /// whose module is absent — and needs no unbind at all.
    #[test]
    fn a_device_with_no_driver_is_bound_without_unbinding() {
        let fixture = Fixture::new(&[USBIP_HOST], "1-5", None);
        fixture.exporter().export("1-5").expect("export");

        assert_eq!(fixture.written(USBIP_HOST, "bind"), "1-5");
    }

    /// A peer re-claiming after a reconnect must not tear down a working
    /// attach to rebuild it.
    #[test]
    fn exporting_an_already_exported_device_changes_nothing() {
        let fixture = Fixture::new(&[USBIP_HOST], "1-5", Some(USBIP_HOST));
        fixture.exporter().export("1-5").expect("export");

        assert_eq!(fixture.written(USBIP_HOST, "bind"), "");
        assert_eq!(fixture.written(USBIP_HOST, "match_busid"), "");
    }

    /// A stale id from before a replug must not be read as a device to unbind.
    #[test]
    fn exporting_a_device_that_is_gone_says_so() {
        let fixture = Fixture::new(&[USBIP_HOST], "1-5", None);
        let error = fixture.exporter().export("9-9").expect_err("should refuse");
        assert!(
            matches!(error, UsbBackendError::NoSuchDevice(_)),
            "got {error:?}"
        );
    }

    /// The failure that matters: unbound from its own driver, not bound to
    /// usbip-host, and therefore driven by nothing. The device must go back.
    #[test]
    fn a_failed_bind_returns_the_device_to_its_driver() {
        use std::os::unix::fs::PermissionsExt;
        if nix_is_root() {
            // Root writes a read-only file, so there is no failure to observe.
            return;
        }
        let fixture = Fixture::new(&["usbhid", USBIP_HOST], "1-5", Some("usbhid"));
        // Make the last step fail the way the kernel refuses one — the file is
        // there and the write is rejected. Deleting it instead would not fail
        // at all: `fs::write` creates a missing file, which sysfs never does.
        let bind = fixture.drivers.join(USBIP_HOST).join("bind");
        std::fs::set_permissions(&bind, std::fs::Permissions::from_mode(0o400)).expect("chmod");

        let error = fixture.exporter().export("1-5").expect_err("should fail");
        assert!(
            matches!(error, UsbBackendError::NeedsPrivilege(_)),
            "got {error:?}"
        );
        // Put back where it came from, and no stale match left behind.
        assert_eq!(fixture.written("usbhid", "bind"), "1-5");
        assert_eq!(fixture.written(USBIP_HOST, "match_busid"), "del 1-5");
    }

    /// Unexporting a device this machine never exported must not unbind it
    /// from the driver that is legitimately using it.
    #[test]
    fn unexporting_a_device_we_do_not_hold_is_a_no_op() {
        let fixture = Fixture::new(&["usbhid", USBIP_HOST], "1-5", Some("usbhid"));
        fixture.exporter().unexport("1-5");

        assert_eq!(fixture.written(USBIP_HOST, "unbind"), "");
        assert_eq!(fixture.written("usbhid", "bind"), "");
    }

    /// Releasing must clear the match too: a device left matched would be
    /// grabbed by usbip-host again the moment it is replugged.
    #[test]
    fn unexporting_unbinds_and_clears_the_match() {
        let fixture = Fixture::new(&["usb", USBIP_HOST], "1-5", Some(USBIP_HOST));
        fixture.exporter().unexport("1-5");

        assert_eq!(fixture.written(USBIP_HOST, "unbind"), "1-5");
        assert_eq!(fixture.written(USBIP_HOST, "match_busid"), "del 1-5");
    }

    /// The permission case is the one users actually hit, and it must name the
    /// file rather than saying "denied".
    #[test]
    fn a_permission_failure_names_the_file_and_asks_for_privilege() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new(&[USBIP_HOST], "1-5", None);
        let bind = fixture.drivers.join(USBIP_HOST).join("bind");
        std::fs::set_permissions(&bind, std::fs::Permissions::from_mode(0o400)).expect("chmod");

        let error = fixture.exporter().export("1-5").expect_err("should fail");
        match error {
            UsbBackendError::NeedsPrivilege(message) => {
                assert!(message.contains("bind"), "unhelpful message: {message}");
            }
            // Running the suite as root makes the write succeed; the assertion
            // is on the message, and there is none to check.
            other => assert!(nix_is_root(), "expected a privilege error, got {other:?}"),
        }
    }

    /// The attribute lives on the device and the kernel starts serving the
    /// moment a descriptor is written to it — that write is what turns a bound
    /// device into an exported one.
    #[test]
    fn serving_writes_the_descriptor_to_the_devices_own_attribute() {
        let fixture = Fixture::new(&[USBIP_HOST], "1-5", Some(USBIP_HOST));
        let sockfd = fixture.devices.join("1-5").join("usbip_sockfd");
        std::fs::write(&sockfd, "").expect("sockfd");

        fixture.exporter().serve("1-5", 42).expect("serve");
        assert_eq!(std::fs::read_to_string(&sockfd).expect("read"), "42");
    }

    /// `-1` is the kernel's "no socket": it ends the attachment while leaving
    /// the device bound, so a peer reconnecting skips the unbind/rebind cycle.
    #[test]
    fn stopping_writes_the_kernels_no_socket_value() {
        let fixture = Fixture::new(&[USBIP_HOST], "1-5", Some(USBIP_HOST));
        let sockfd = fixture.devices.join("1-5").join("usbip_sockfd");
        std::fs::write(&sockfd, "").expect("sockfd");

        fixture.exporter().stop_serving("1-5").expect("stop");
        assert_eq!(std::fs::read_to_string(&sockfd).expect("read"), "-1");
    }

    /// Root can write a 0400 file, so the permission test above cannot assert
    /// unconditionally.
    fn nix_is_root() -> bool {
        // SAFETY: `geteuid` reads a process property and cannot fail.
        unsafe { libc_geteuid() == 0 }
    }

    extern "C" {
        #[link_name = "geteuid"]
        fn libc_geteuid() -> u32;
    }
}
