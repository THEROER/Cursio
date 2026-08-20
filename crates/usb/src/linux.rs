//! Enumerating USB devices from sysfs.
//!
//! ## sysfs, not the `usbip` command
//!
//! `usbip list` exists and would be shorter to call, but its output is meant
//! for people: columns, padding, and a layout that has changed between
//! releases. Parsing it makes a distro's version bump a runtime failure in a
//! place with no test coverage. The kernel's actual interface is sysfs, which
//! `usbip` itself reads, so this reads the same files and skips the round trip
//! through a text format nobody promised to keep.
//!
//! ## What a device looks like there
//!
//! `/sys/bus/usb/devices` holds two kinds of entry, distinguished by name:
//!
//! - `1-5` — a device. Has `idVendor`, `idProduct`, `bDeviceClass`.
//! - `1-5:1.0` — one *interface* of that device. Has `bInterfaceClass` and the
//!   `ep_NN` endpoint directories.
//!
//! The colon is the whole distinction, and it matters: a device is what gets
//! exported, while the endpoints that decide whether it will survive the
//! network live one level down, on its interfaces.
//!
//! Root hubs (`usb1`, `usb2`, …) are devices too, and are excluded — exporting
//! one hands over every port on the controller.

use std::path::{Path, PathBuf};

use crate::{LocalDevice, UsbBackendError};

/// Where the kernel publishes the USB tree. A parameter in the reading code
/// rather than a constant everywhere, so tests can point it at a fixture.
const SYSFS_USB: &str = "/sys/bus/usb/devices";

/// Enumerates devices from sysfs.
pub struct LinuxUsb {
    root: PathBuf,
}

impl Default for LinuxUsb {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxUsb {
    pub fn new() -> Self {
        Self {
            root: PathBuf::from(SYSFS_USB),
        }
    }

    /// Point at a different tree. Only tests use this; the kernel has exactly
    /// one place it publishes USB devices.
    #[cfg(test)]
    fn rooted_at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Every device currently plugged in, root hubs excluded.
    ///
    /// A device that cannot be read is skipped with a warning rather than
    /// failing the whole listing: devices disappear mid-enumeration when
    /// somebody pulls a cable, and one unplugged stick must not hide every
    /// other device from the mesh.
    pub fn devices(&self) -> Result<Vec<LocalDevice>, UsbBackendError> {
        let entries = std::fs::read_dir(&self.root).map_err(|e| {
            UsbBackendError::Enumerate(format!("cannot read {}: {e}", self.root.display()))
        })?;

        let mut devices = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Interfaces carry a colon; only whole devices can be exported.
            if name.contains(':') {
                continue;
            }
            match read_device(&entry.path(), &name) {
                Ok(Some(device)) => devices.push(device),
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(busid = %name, error = %e, "skipping unreadable usb device");
                }
            }
        }
        devices.sort_by(|a, b| a.busid.cmp(&b.busid));
        Ok(devices)
    }
}

/// Read one device directory, or `None` when it is not a device at all.
fn read_device(path: &Path, busid: &str) -> Result<Option<LocalDevice>, UsbBackendError> {
    let Some(vendor) = read_hex(path, "idVendor") else {
        // No vendor id means this is not a USB device node — a stale symlink,
        // or something the kernel added that we do not understand.
        return Ok(None);
    };
    let Some(product) = read_hex(path, "idProduct") else {
        return Ok(None);
    };

    let class = read_hex_u8(path, "bDeviceClass").unwrap_or(0);
    let interfaces = interfaces_of(path, busid);

    // Class 0 means "look at the interfaces" — most composite devices report
    // it, so taking it at face value would file a keyboard and a webcam under
    // the same meaningless class.
    let class = match class {
        0 => interfaces
            .iter()
            .find_map(|i| read_hex_u8(i, "bInterfaceClass"))
            .unwrap_or(0),
        c => c,
    };

    Ok(Some(LocalDevice {
        busid: busid.to_string(),
        vendor,
        product,
        manufacturer: read_string(path, "manufacturer"),
        product_name: read_string(path, "product"),
        class,
        has_isochronous: interfaces.iter().any(|i| has_isochronous_endpoint(i)),
        // Root hubs are named `usbN` rather than `bus-port`, and are the only
        // devices whose bus id carries no dash.
        is_root_hub: !busid.contains('-'),
    }))
}

/// The interface directories belonging to one device.
///
/// They are siblings in sysfs, not children: `1-5`'s interfaces are `1-5:1.0`,
/// `1-5:1.1` … in the same directory. They are *also* symlinked inside the
/// device directory, which is what this reads — one directory listing instead
/// of re-scanning every sibling per device.
fn interfaces_of(path: &Path, busid: &str) -> Vec<PathBuf> {
    let prefix = format!("{busid}:");
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
        .collect()
}

/// Whether an interface has an endpoint the network cannot carry well.
///
/// Endpoints appear as `ep_NN` directories holding a `type` file, whose
/// contents are one of `Control`, `Isoc`, `Bulk`, `Interrupt`. Isochronous is
/// the one with a per-frame deadline and no retransmission — a webcam or an
/// audio interface — and it is the reason a device gets flagged.
fn has_isochronous_endpoint(interface: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(interface) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name().to_string_lossy().starts_with("ep_")
            && std::fs::read_to_string(entry.path().join("type"))
                .map(|t| t.trim() == "Isoc")
                .unwrap_or(false)
    })
}

/// Read a sysfs attribute as text, trimmed. Absent or unreadable is `None` —
/// plenty of devices simply do not carry `manufacturer`.
fn read_string(path: &Path, attr: &str) -> Option<String> {
    let value = std::fs::read_to_string(path.join(attr)).ok()?;
    let value = value.trim();
    match value.is_empty() {
        true => None,
        false => Some(value.to_string()),
    }
}

/// Read a hex attribute such as `idVendor` (`18d1`, no `0x`).
fn read_hex(path: &Path, attr: &str) -> Option<u16> {
    u16::from_str_radix(read_string(path, attr)?.trim(), 16).ok()
}

fn read_hex_u8(path: &Path, attr: &str) -> Option<u8> {
    u8::from_str_radix(read_string(path, attr)?.trim(), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a sysfs-shaped tree, so enumeration is tested without hardware.
    struct FakeSysfs {
        dir: tempfile::TempDir,
    }

    impl FakeSysfs {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("tempdir"),
            }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn device(&self, busid: &str, attrs: &[(&str, &str)]) -> &Self {
            let path = self.dir.path().join(busid);
            std::fs::create_dir_all(&path).expect("device dir");
            for (name, value) in attrs {
                std::fs::write(path.join(name), value).expect("attr");
            }
            self
        }

        /// An interface, with the endpoint types it exposes.
        fn interface(&self, busid: &str, iface: &str, class: &str, endpoints: &[&str]) -> &Self {
            // Interfaces live inside the device directory, mirroring the
            // symlinks the kernel puts there.
            let path = self.dir.path().join(busid).join(iface);
            std::fs::create_dir_all(&path).expect("iface dir");
            std::fs::write(path.join("bInterfaceClass"), class).expect("class");
            for (n, kind) in endpoints.iter().enumerate() {
                let ep = path.join(format!("ep_{n:02}"));
                std::fs::create_dir_all(&ep).expect("ep dir");
                std::fs::write(ep.join("type"), kind).expect("type");
            }
            self
        }
    }

    fn phone(fs: &FakeSysfs) {
        fs.device(
            "1-5",
            &[
                ("idVendor", "18d1"),
                ("idProduct", "4ee7"),
                ("bDeviceClass", "00"),
                ("manufacturer", "Google"),
                ("product", "Pixel"),
            ],
        )
        .interface("1-5", "1-5:1.0", "ff", &["Bulk", "Bulk"]);
    }

    #[test]
    fn reads_a_device_from_sysfs() {
        let fs = FakeSysfs::new();
        phone(&fs);

        let devices = LinuxUsb::rooted_at(fs.root())
            .devices()
            .expect("enumerates");

        assert_eq!(devices.len(), 1);
        let d = &devices[0];
        assert_eq!(d.busid, "1-5");
        assert_eq!(d.vendor, 0x18d1);
        assert_eq!(d.product, 0x4ee7);
        assert_eq!(d.manufacturer.as_deref(), Some("Google"));
        assert_eq!(d.product_name.as_deref(), Some("Pixel"));
    }

    /// An interface entry is not a device. Counting `1-5:1.0` as one would
    /// offer the mesh three "devices" for one phone, none of them exportable.
    #[test]
    fn interfaces_are_not_mistaken_for_devices() {
        let fs = FakeSysfs::new();
        phone(&fs);
        // A sibling interface directory, as the kernel really lays them out.
        fs.device("1-5:1.0", &[("bInterfaceClass", "ff")]);

        let devices = LinuxUsb::rooted_at(fs.root())
            .devices()
            .expect("enumerates");

        assert_eq!(devices.len(), 1, "only the device itself is a device");
        assert_eq!(devices[0].busid, "1-5");
    }

    /// Exporting a root hub hands over every port on the controller — including
    /// the one the user's keyboard is in.
    #[test]
    fn a_root_hub_is_not_shareable() {
        let fs = FakeSysfs::new();
        fs.device(
            "usb1",
            &[
                ("idVendor", "1d6b"),
                ("idProduct", "0002"),
                ("bDeviceClass", "09"),
            ],
        );

        let devices = LinuxUsb::rooted_at(fs.root())
            .devices()
            .expect("enumerates");

        assert_eq!(devices.len(), 1, "it is still enumerated");
        assert!(devices[0].is_root_hub);
        assert!(!devices[0].is_shareable(), "but never offered");
    }

    /// A bluetooth adapter carries isochronous endpoints for headset audio.
    /// Those are the transfers a network hop ruins, so the listing says so.
    #[test]
    fn an_isochronous_endpoint_marks_the_device() {
        let fs = FakeSysfs::new();
        fs.device(
            "1-5",
            &[
                ("idVendor", "13d3"),
                ("idProduct", "3563"),
                ("bDeviceClass", "e0"),
            ],
        )
        .interface("1-5", "1-5:1.0", "e0", &["Bulk", "Interrupt"])
        .interface("1-5", "1-5:1.1", "e0", &["Isoc", "Isoc"]);

        let devices = LinuxUsb::rooted_at(fs.root())
            .devices()
            .expect("enumerates");

        assert!(
            devices[0].has_isochronous,
            "an isochronous endpoint on any interface counts"
        );
    }

    #[test]
    fn a_device_without_isochronous_endpoints_is_not_flagged() {
        let fs = FakeSysfs::new();
        fs.device(
            "5-4",
            &[
                ("idVendor", "258a"),
                ("idProduct", "0150"),
                ("bDeviceClass", "00"),
            ],
        )
        .interface("5-4", "5-4:1.0", "03", &["Interrupt"])
        .interface("5-4", "5-4:1.1", "03", &["Interrupt"]);

        let devices = LinuxUsb::rooted_at(fs.root())
            .devices()
            .expect("enumerates");

        assert!(!devices[0].has_isochronous);
    }

    /// Class 0 means "ask the interfaces". Reporting it verbatim files a
    /// keyboard and a phone under the same meaningless class.
    #[test]
    fn a_deferring_device_takes_its_class_from_its_interface() {
        let fs = FakeSysfs::new();
        fs.device(
            "5-4",
            &[
                ("idVendor", "258a"),
                ("idProduct", "0150"),
                ("bDeviceClass", "00"),
            ],
        )
        .interface("5-4", "5-4:1.0", "03", &["Interrupt"]);

        let devices = LinuxUsb::rooted_at(fs.root())
            .devices()
            .expect("enumerates");

        assert_eq!(devices[0].class, 0x03, "HID, from the interface");
    }

    /// Plenty of devices carry no strings at all. That is ordinary, not an
    /// error, and must not drop them from the listing.
    #[test]
    fn a_device_with_no_strings_is_still_listed() {
        let fs = FakeSysfs::new();
        fs.device(
            "1-6",
            &[
                ("idVendor", "0b05"),
                ("idProduct", "1939"),
                ("bDeviceClass", "00"),
            ],
        );

        let devices = LinuxUsb::rooted_at(fs.root())
            .devices()
            .expect("enumerates");

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].manufacturer, None);
        assert_eq!(devices[0].product_name, None);
    }

    /// A cable pulled mid-enumeration leaves a directory with nothing in it.
    /// One such device must not hide every other one from the mesh.
    #[test]
    fn an_unreadable_device_does_not_hide_the_others() {
        let fs = FakeSysfs::new();
        phone(&fs);
        std::fs::create_dir_all(fs.root().join("2-1")).expect("empty dir");

        let devices = LinuxUsb::rooted_at(fs.root())
            .devices()
            .expect("enumerates");

        assert_eq!(devices.len(), 1, "the readable device still arrives");
        assert_eq!(devices[0].busid, "1-5");
    }

    /// Listings are compared and shown to people; a stable order keeps a
    /// re-announcement from looking like a change.
    #[test]
    fn devices_come_back_in_a_stable_order() {
        let fs = FakeSysfs::new();
        for busid in ["5-4", "1-5", "2-1"] {
            fs.device(busid, &[("idVendor", "1234"), ("idProduct", "5678")]);
        }

        let devices = LinuxUsb::rooted_at(fs.root())
            .devices()
            .expect("enumerates");
        let order: Vec<&str> = devices.iter().map(|d| d.busid.as_str()).collect();

        assert_eq!(order, ["1-5", "2-1", "5-4"]);
    }
}
