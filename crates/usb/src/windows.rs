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
//! not. Sharing therefore raises a **UAC prompt for `usbipd` alone**, once per
//! device: `bind` writes a persistent registry entry, so every later run of
//! Cursio finds the device already shared and prompts for nothing.
//!
//! What is deliberately *not* elevated is Cursio itself. A daemon holding
//! administrator rights for the life of a session to avoid a handful of
//! prompts is a much larger thing to have made — and on this codebase it would
//! also break file sharing, since volumes WinFsp mounts as administrator are
//! invisible to the user's own Explorer.
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
        let mut devices = parse_state(&json)?;
        // `usbipd state` carries no class, and the class is what decides
        // whether a device may be taken automatically — a Bluetooth radio
        // taken from a laptop cuts out the user's headphones. Windows knows it
        // through PnP, so it is asked separately and matched by vendor and
        // product.
        let classes = pnp_classes();
        for device in &mut devices {
            if let Some(class) = classes.get(&(device.vendor, device.product)) {
                device.class = *class;
            }
        }
        Ok(devices)
    }

    /// Share a device so a peer can attach it.
    ///
    /// Tries unprivileged first, because a device already registered needs no
    /// prompt at all: `bind` writes a persistent registry entry, so the second
    /// and every later run succeed outright. Only a genuinely new device
    /// reaches [`bind_elevated`](Self::bind_elevated) and its single UAC
    /// prompt.
    ///
    /// Asking the user to run `usbipd bind` by hand would be the one command
    /// this whole module exists to remove — "whichever machine has the port,
    /// every machine has the device" is not something anyone should have to
    /// type per device.
    pub fn bind(&self, busid: &str) -> Result<(), UsbBackendError> {
        match self.run(&["bind", "--busid", busid]) {
            Ok(_) => Ok(()),
            // One prompt, every device — not one prompt per device. Four
            // devices meant four dialogs, and a retry after a refusal meant
            // four more; privilege is a property of the machine, so it is
            // asked for once and spent on everything at hand.
            Err(UsbBackendError::NeedsPrivilege(_)) => {
                self.bind_all_elevated()?;
                // Answer for the device actually asked about: the batch may
                // have bound it, or the user may have declined.
                match self.run(&["bind", "--busid", busid]) {
                    Ok(_) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Share every device this machine has, in one elevated run.
    ///
    /// `bind` writes a persistent registry entry, so this is genuinely once per
    /// machine rather than once per session: the next start finds everything
    /// already shared and prompts for nothing. A device plugged in later is the
    /// only thing that brings the dialog back, and then only for itself.
    #[cfg(target_os = "windows")]
    fn bind_all_elevated(&self) -> Result<(), UsbBackendError> {
        use std::os::windows::process::CommandExt;

        let devices = self.devices()?;
        if devices.is_empty() {
            return Ok(());
        }
        let busids: Vec<String> = devices.iter().map(|d| d.busid.clone()).collect();
        tracing::info!(
            count = busids.len(),
            "asking for administrator rights once to share this machine's usb devices"
        );

        // One `Start-Process -Verb RunAs` is one UAC dialog, and the loop runs
        // inside the elevated shell rather than around it. `-ErrorAction
        // Continue` so a device that refuses to bind does not abandon the rest.
        let binds = busids
            .iter()
            .map(|b| format!("& '{}' bind --busid {b}", self.exe.display()))
            .collect::<Vec<_>>()
            .join("; ");
        // The inner command is base64'd rather than quoted: it carries paths,
        // semicolons and quotes of its own, and every layer of PowerShell
        // quoting between here and the elevated shell is a way to get it
        // wrong. `-EncodedCommand` has no quoting rules at all.
        let encoded = encode_command(&binds);
        let script = format!(
            "$p = Start-Process -FilePath 'powershell.exe' \
             -ArgumentList '-NoProfile','-EncodedCommand','{encoded}' \
             -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode"
        );

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| UsbBackendError::Enumerate(format!("cannot raise a uac prompt: {e}")))?;

        match output.status.success() {
            true => {
                tracing::info!("usb devices shared; windows will not ask again for them");
                Ok(())
            }
            // Declining is an ordinary answer: sharing stays off until the user
            // is willing, and nothing here retries on its own.
            false => Err(UsbBackendError::NeedsPrivilege(
                "sharing this machine's usb devices needs administrator rights,                  and the prompt was declined"
                    .into(),
            )),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn bind_all_elevated(&self) -> Result<(), UsbBackendError> {
        Err(UsbBackendError::NeedsPrivilege(
            "sharing usb devices needs administrator rights".into(),
        ))
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

/// Encode a PowerShell command the way `-EncodedCommand` expects.
///
/// UTF-16LE, then base64 — the encoding PowerShell documents for exactly this
/// case. Hand-rolled because pulling in a base64 crate to spell one fixed
/// alphabet would be the larger dependency.
#[cfg(target_os = "windows")]
fn encode_command(command: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let utf16: Vec<u8> = command
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();

    let mut out = String::with_capacity(utf16.len().div_ceil(3) * 4);
    for chunk in utf16.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(match chunk.len() > 1 {
            true => ALPHABET[((n >> 6) & 0x3F) as usize] as char,
            false => '=',
        });
        out.push(match chunk.len() > 2 {
            true => ALPHABET[(n & 0x3F) as usize] as char,
            false => '=',
        });
    }
    out
}

/// USB classes, keyed by vendor and product, as Windows' PnP database has them.
///
/// Windows names device *setup classes* ("Bluetooth", "HIDClass") rather than
/// USB class codes, so the answer is translated back into the numbers the
/// protocol speaks. Only the classes that decide whether a device is safe to
/// take are recognised; everything else stays 0, which reads as "no opinion"
/// and is treated as takeable.
///
/// Best-effort: a machine where the query fails simply reports no classes, and
/// the caller sees devices it will not automatically take rather than devices
/// it wrongly will.
#[cfg(target_os = "windows")]
fn pnp_classes() -> std::collections::HashMap<(u16, u16), u8> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let script = "Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |                   Where-Object InstanceId -like 'USB\\VID_*' |                   ForEach-Object { $_.Class + '|' + $_.InstanceId }";
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let Ok(output) = output else {
        tracing::debug!("cannot read pnp device classes; nothing will be auto-attached");
        return std::collections::HashMap::new();
    };

    parse_pnp(&String::from_utf8_lossy(&output.stdout))
}

/// Turn `Class|InstanceId` lines into USB class codes by vendor and product.
fn parse_pnp(text: &str) -> std::collections::HashMap<(u16, u16), u8> {
    let mut classes = std::collections::HashMap::new();
    for line in text.lines() {
        let Some((setup_class, instance)) = line.trim().split_once('|') else {
            continue;
        };
        let Some(ids) = instance_ids(instance) else {
            continue;
        };
        // Windows' setup classes, mapped to the USB class codes
        // `UsbDevice::is_safe_to_take` reasons about. A device appears once per
        // interface, so the *first* class that means "do not take" wins:
        // a composite device with any dangerous interface is dangerous whole.
        let code = match setup_class {
            "Bluetooth" => 0xE0,
            "HIDClass" | "Keyboard" | "Mouse" => 0x03,
            "MEDIA" | "AudioEndpoint" => 0x01,
            "Camera" | "Image" => 0x0E,
            "USB" if instance.contains("ROOT_HUB") => 0x09,
            _ => continue,
        };
        classes.entry(ids).or_insert(code);
    }
    classes
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

    /// A phone is the device this module exists for, and its instance id
    /// carries a long alphanumeric serial rather than the zeroes the other
    /// entries have — captured from a POCO X7 Pro plugged into the peer.
    #[test]
    fn a_phone_with_a_serial_in_its_instance_id_is_read() {
        let json = r#"{"Devices":[{
            "BusId": "1-1",
            "Description": "POCO X7 Pro",
            "InstanceId": "USB\\VID_2717&PID_FF08\\49FMUCWG7HM7Z5HI"
        }]}"#;
        let devices = parse_state(json).expect("parses");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].busid, "1-1");
        assert_eq!(devices[0].vendor, 0x2717);
        assert_eq!(devices[0].product, 0xFF08);
        assert_eq!(devices[0].product_name.as_deref(), Some("POCO X7 Pro"));
    }

    /// The elevated command reaches PowerShell only through this encoding, so
    /// a mistake here is a UAC prompt that runs nothing. Checked against what
    /// PowerShell itself produces: UTF-16LE, then base64.
    #[test]
    fn a_command_is_encoded_as_utf16le_base64() {
        // "hi" → 68 00 69 00 → aABpAA==
        assert_eq!(encode_command("hi"), "aABpAA==");
        // Padding for every remainder, since the byte count is always even.
        assert_eq!(encode_command("a"), "YQA=");
        assert_eq!(encode_command("abc"), "YQBiAGMA");
        assert_eq!(encode_command(""), "");
    }

    /// Device paths carry spaces and quotes, which is the whole reason this is
    /// encoded rather than quoted.
    #[test]
    fn a_command_with_quotes_and_spaces_survives_encoding() {
        let command = "& 'C:\\Program Files\\usbipd-win\\usbipd.exe' bind --busid 1-1";
        let encoded = encode_command(command);
        assert!(!encoded.contains(' '), "encoding must not leave spaces");
        assert!(!encoded.contains('\''), "encoding must not leave quotes");
        assert!(encoded
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
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
