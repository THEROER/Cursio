//! Windows: enumerating and sharing through `usbipd-win`.
//!
//! The one platform where Cursio does not talk to the kernel itself. Windows
//! has no USB/IP in the box; the importing half is a signed third-party VHCI
//! driver, and the exporting half is the service that ships with it. Both are
//! `usbipd-win`, which is an install rather than a fork to maintain — the same
//! shape as WinFsp for files.
//!
//! ## The CLI, not sysfs
//!
//! On Linux this crate writes sysfs directly and skips the `usbip` binary,
//! because its output is formatted for people and has changed between releases.
//! Windows inverts that: there is no sysfs, the driver's control surface is a
//! private device interface, and `usbipd` publishes a **`state` subcommand that
//! emits JSON** for exactly this purpose. Parsing a documented JSON contract is
//! not the same bet as parsing a table meant for a terminal.
//!
//! ## Reading is free, changing needs administrator
//!
//! Verified on the peer (usbipd-win 5.3.0): `list`, `state` and `--version` all
//! succeed as an ordinary user, while `bind` answers
//!
//! ```text
//! usbipd: error: Access denied; this operation requires administrator privileges.
//! ```
//!
//! That is the same split Linux has — enumeration is unprivileged, export is
//! not — and it is reported rather than escalated, for the same reason: a
//! daemon holding administrator rights to avoid asking is a much larger thing
//! to have made than a prompt.
//!
//! ## Absence is a first-class state
//!
//! No `usbipd.exe`, no module. [`WindowsUsb::is_available`] is what the
//! factory asks before advertising, so a Windows peer without the driver is
//! never offered devices that would fail on claim (invariant 6 in
//! `docs/usb-design.md`).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{LocalDevice, UsbBackendError};

/// Where the installer puts it. Checked before `PATH` because the installer
/// does not add itself to `PATH`, so a machine with a working install would
/// otherwise look like one without.
const DEFAULT_PATH: &str = r"C:\Program Files\usbipd-win\usbipd.exe";

/// Enumerates and shares devices through `usbipd-win`.
#[derive(Debug, Clone)]
pub struct WindowsUsb {
    exe: PathBuf,
}

impl Default for WindowsUsb {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsUsb {
    pub fn new() -> Self {
        Self {
            exe: PathBuf::from(DEFAULT_PATH),
        }
    }

    /// Point at a different binary — a non-default install, or a test double.
    pub fn at(exe: impl Into<PathBuf>) -> Self {
        Self { exe: exe.into() }
    }

    /// Whether `usbipd-win` is installed at all.
    ///
    /// The whole module hangs off this: without it Windows can neither export
    /// nor import, and saying so up front is the difference between a device
    /// list that is honestly empty and one that lies.
    pub fn is_available(&self) -> bool {
        self.exe.is_file()
    }

    /// Devices Windows can see, as `usbipd state` reports them.
    ///
    /// Unprivileged, so this is safe to call on every listing.
    pub fn devices(&self) -> Result<Vec<LocalDevice>, UsbBackendError> {
        let json = self.run(&["state"])?;
        parse_state(&json)
    }

    /// Share a device so a peer can attach it.
    ///
    /// Needs administrator. The error carries what `usbipd` said, because
    /// "access denied" without the command that fixes it is a dead end for the
    /// user — and the fix here is not a permission to grant but an elevated
    /// prompt to accept.
    pub fn bind(&self, busid: &str) -> Result<(), UsbBackendError> {
        self.run(&["bind", "--busid", busid]).map(|_| ())
    }

    /// Stop sharing.
    pub fn unbind(&self, busid: &str) {
        if let Err(e) = self.run(&["unbind", "--busid", busid]) {
            tracing::warn!(busid, error = %e, "could not unbind on windows");
        }
    }

    /// Run `usbipd` and return its stdout.
    fn run(&self, args: &[&str]) -> Result<String, UsbBackendError> {
        if !self.is_available() {
            return Err(UsbBackendError::DriverMissing(format!(
                "usbipd-win is not installed ({} is missing); \
                 install it with: winget install usbipd",
                self.exe.display()
            )));
        }
        let output = Command::new(&self.exe).args(args).output().map_err(|e| {
            UsbBackendError::Enumerate(format!("cannot run {}: {e}", self.exe.display()))
        })?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }

        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // The privilege case is told apart because it is the one the user can
        // act on, and because the wire vocabulary has a variant for it.
        if message.to_lowercase().contains("administrator") {
            return Err(UsbBackendError::NeedsPrivilege(message));
        }
        Err(UsbBackendError::Enumerate(message))
    }
}

/// Read the device list out of `usbipd state`'s JSON.
///
/// Deliberately hand-rolled against the two fields that matter rather than
/// deserialised into a mirror of usbipd's whole schema: this crate needs a bus
/// id and a description, and a struct naming every field would break on a
/// release that adds one.
fn parse_state(json: &str) -> Result<Vec<LocalDevice>, UsbBackendError> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| UsbBackendError::Enumerate(format!("cannot read usbipd state: {e}")))?;

    let Some(devices) = value.get("Devices").and_then(|d| d.as_array()) else {
        return Err(UsbBackendError::Enumerate(
            "usbipd state has no Devices array".into(),
        ));
    };

    Ok(devices
        .iter()
        .filter_map(|device| {
            let busid = device.get("BusId")?.as_str()?;
            // A device with no bus id is one Windows knows about but that is
            // not currently plugged in — usbipd lists persisted shares that
            // way, and offering one to a peer would advertise nothing.
            if busid.is_empty() {
                return None;
            }
            let (vendor, product) = instance_ids(device.get("InstanceId")?.as_str()?)?;
            Some(LocalDevice {
                busid: busid.to_string(),
                vendor,
                product,
                manufacturer: None,
                product_name: device
                    .get("Description")
                    .and_then(|d| d.as_str())
                    .map(str::to_string),
                // usbipd does not report a class, and inventing one from the
                // description would be a guess the user could not correct.
                class: 0,
                // Nor does it report endpoint types. Reported as fine rather
                // than degraded: flagging every Windows device as unreliable
                // would make the warning meaningless where it matters.
                has_isochronous: false,
                is_root_hub: false,
            })
        })
        .collect())
}

/// Pull vendor and product out of a Windows instance id.
///
/// `USB\VID_0489&PID_E0CD\000000000` — the ids are hex and the rest is a
/// per-device serial that varies in shape, so only the two fields are read.
fn instance_ids(instance: &str) -> Option<(u16, u16)> {
    let vid = field_after(instance, "VID_")?;
    let pid = field_after(instance, "PID_")?;
    Some((vid, pid))
}

/// Read the four hex digits following `marker`.
fn field_after(text: &str, marker: &str) -> Option<u16> {
    let start = text.find(marker)? + marker.len();
    let digits: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .take(4)
        .collect();
    u16::from_str_radix(&digits, 16).ok()
}

/// Where `usbipd-win` would be, for a message that names it.
pub fn default_exe() -> &'static Path {
    Path::new(DEFAULT_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real output from `usbipd state` on the peer (usbipd-win 5.3.0), so the
    /// parser is tested against the format that actually ships rather than one
    /// invented here.
    const STATE: &str = r#"{
      "Devices": [
        {
          "BusId": "3-4",
          "ClientIPAddress": null,
          "Description": "MediaTek Bluetooth Adapter",
          "InstanceId": "USB\\VID_0489&PID_E0CD\\000000000",
          "IsForced": false,
          "PersistedGuid": null,
          "StubInstanceId": null
        },
        {
          "BusId": "4-3",
          "ClientIPAddress": null,
          "Description": "USB-устройство ввода",
          "InstanceId": "USB\\VID_048D&PID_C999\\6&1790878C&0&3",
          "IsForced": false,
          "PersistedGuid": null,
          "StubInstanceId": null
        }
      ]
    }"#;

    #[test]
    fn devices_are_read_from_the_state_json() {
        let devices = parse_state(STATE).expect("parses");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].busid, "3-4");
        assert_eq!(devices[0].vendor, 0x0489);
        assert_eq!(devices[0].product, 0xE0CD);
        assert_eq!(
            devices[0].product_name.as_deref(),
            Some("MediaTek Bluetooth Adapter")
        );
    }

    /// Device names are whatever Windows is localised to, and a parser that
    /// assumed ASCII would drop exactly the devices a user recognises.
    #[test]
    fn a_non_ascii_description_survives() {
        let devices = parse_state(STATE).expect("parses");
        assert_eq!(
            devices[1].product_name.as_deref(),
            Some("USB-устройство ввода")
        );
        assert_eq!(devices[1].vendor, 0x048D);
    }

    /// usbipd lists persisted shares with an empty bus id when the device is
    /// not plugged in. Offering one would advertise a device that is not there.
    #[test]
    fn a_device_that_is_not_plugged_in_is_not_listed() {
        let json = r#"{"Devices":[{"BusId":"","InstanceId":"USB\\VID_0489&PID_E0CD\\x"}]}"#;
        assert!(parse_state(json).expect("parses").is_empty());
    }

    /// A release that adds fields must not break the listing — which is the
    /// whole reason this reads two fields rather than mirroring the schema.
    #[test]
    fn unknown_fields_do_not_break_the_listing() {
        let json = r#"{"Devices":[{"BusId":"1-1","InstanceId":"USB\\VID_1234&PID_5678\\z",
                       "SomethingNew":42,"AndAnother":{"nested":true}}],"Extra":"ignored"}"#;
        let devices = parse_state(json).expect("parses");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].vendor, 0x1234);
    }

    /// Output that is not the expected shape must say so rather than reporting
    /// an empty machine, which is indistinguishable from one with no devices.
    #[test]
    fn output_without_a_devices_array_is_an_error() {
        assert!(parse_state("{}").is_err());
        assert!(parse_state("not json at all").is_err());
    }

    /// No install is the ordinary state on a fresh machine, and the message
    /// must carry the command that fixes it.
    #[test]
    fn a_missing_install_names_the_command_to_run() {
        let usb = WindowsUsb::at(r"C:\nowhere\usbipd.exe");
        assert!(!usb.is_available());

        let error = usb.devices().expect_err("should fail");
        match error {
            UsbBackendError::DriverMissing(message) => {
                assert!(message.contains("winget"), "unhelpful: {message}");
            }
            other => panic!("expected a missing driver, got {other:?}"),
        }
    }

    #[test]
    fn instance_ids_are_read_as_hex() {
        assert_eq!(
            instance_ids(r"USB\VID_0489&PID_E0CD\000000000"),
            Some((0x0489, 0xE0CD))
        );
        assert_eq!(instance_ids("nothing here"), None);
    }
}
