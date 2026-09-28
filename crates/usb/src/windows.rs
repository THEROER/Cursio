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

use std::collections::HashMap;
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
    /// USB classes by vendor and product, and the device set they were read
    /// for.
    ///
    /// `Get-PnpDevice` costs about **2.8 seconds** — nine times what `usbipd
    /// state` costs — and the listing is re-read every few seconds so a device
    /// plugged in mid-session appears. Paying that on every listing blocked the
    /// session loop the shared cursor rides (`took_ms=50..112` in the logs) and
    /// made the whole enumeration miss its window, so a claim arriving mid-scan
    /// was answered `Gone` for a device that was plugged in the whole time.
    ///
    /// A class cannot change while a device stays plugged in, so the answer is
    /// only re-read when the *set* of devices changes — which is exactly when a
    /// class this does not know about could appear.
    classes: std::cell::RefCell<Option<ClassCache>>,
    /// The last listing, and what it was read under. See [`Self::devices`].
    listing: std::cell::RefCell<Option<Listing>>,
}

/// A USB vendor and product id, the key PnP classes are matched by.
type VendorProduct = (u16, u16);

/// PnP classes, and the device set they were read for.
type ClassCache = (Vec<VendorProduct>, HashMap<VendorProduct, u8>);

/// A `usbipd state` answer kept until something could have changed it.
#[derive(Debug, Clone)]
struct Listing {
    devices: Vec<LocalDevice>,
    /// [`device_changes`] when it was read.
    generation: u64,
    read_at: std::time::Instant,
}

/// How long a listing is trusted without a device notification.
///
/// Notifications cover plugging and unplugging, but not everything `state`
/// reports: another program binding a device through usbipd changes the answer
/// with no device arriving. A minute bounds how stale that can get while
/// keeping the process count to one a minute instead of one every few seconds.
const LISTING_TRUSTED_FOR: std::time::Duration = std::time::Duration::from_secs(60);

/// A counter Windows advances whenever a USB device arrives or leaves, or
/// `None` where no notification could be registered (and on other platforms,
/// where this backend only runs under test).
///
/// The listing is polled for the life of the node, and on Windows each poll is
/// a process. Asking Windows to say when a device appears turns that into one
/// process per actual change. The registration is process-wide and never
/// released: it is one per node, lives as long as the node, and the callback
/// touches nothing but an atomic.
#[cfg(target_os = "windows")]
fn device_changes() -> Option<u64> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    use windows_sys::core::GUID;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Register_Notification, CM_NOTIFY_ACTION, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
        CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CR_SUCCESS, HCMNOTIFICATION,
    };

    static GENERATION: AtomicU64 = AtomicU64::new(0);
    static REGISTERED: OnceLock<bool> = OnceLock::new();

    // GUID_DEVINTERFACE_USB_DEVICE: every USB device, hubs' children included.
    const USB_DEVICE: GUID = GUID::from_u128(0xA5DCBF10_6530_11D2_901F_00C04FB951ED);

    unsafe extern "system" fn changed(
        _: HCMNOTIFICATION,
        _: *const core::ffi::c_void,
        _: CM_NOTIFY_ACTION,
        _: *const CM_NOTIFY_EVENT_DATA,
        _: u32,
    ) -> u32 {
        GENERATION.fetch_add(1, Ordering::Relaxed);
        0
    }

    let registered = *REGISTERED.get_or_init(|| {
        let mut filter = CM_NOTIFY_FILTER {
            cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
            ..Default::default()
        };
        filter.u.DeviceInterface.ClassGuid = USB_DEVICE;
        let mut handle: HCMNOTIFICATION = std::ptr::null_mut();
        // SAFETY: `filter` is initialised for the interface-class variant it
        // names, the callback is a plain function, and `handle` outlives the
        // call. The handle is deliberately never unregistered (see above).
        let result = unsafe {
            CM_Register_Notification(&filter, std::ptr::null(), Some(changed), &mut handle)
        };
        if result != CR_SUCCESS {
            tracing::warn!(
                code = result,
                "windows would not report usb arrivals; re-reading the device list on a timer"
            );
        }
        result == CR_SUCCESS
    });
    registered.then(|| GENERATION.load(Ordering::Relaxed))
}

#[cfg(not(target_os = "windows"))]
fn device_changes() -> Option<u64> {
    None
}

/// Whether a kept listing still answers, given the notification counter now.
fn listing_is_current(listing: &Listing, generation: Option<u64>) -> bool {
    generation == Some(listing.generation) && listing.read_at.elapsed() < LISTING_TRUSTED_FOR
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
            classes: std::cell::RefCell::new(None),
            listing: std::cell::RefCell::new(None),
        }
    }

    /// Point at a different binary — a non-default install, or a test double.
    pub fn at(exe: impl Into<PathBuf>) -> Self {
        Self {
            exe: exe.into(),
            classes: std::cell::RefCell::new(None),
            listing: std::cell::RefCell::new(None),
        }
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
    /// Unprivileged, so this is safe to call on every listing. The answer is
    /// kept until Windows reports a device arriving or leaving, this backend
    /// binds one, or [`LISTING_TRUSTED_FOR`] passes.
    pub fn devices(&self) -> Result<Vec<LocalDevice>, UsbBackendError> {
        let generation = device_changes();
        if let Some(listing) = self.listing.borrow().as_ref() {
            if listing_is_current(listing, generation) {
                return Ok(listing.devices.clone());
            }
        }
        let devices = self.read_devices()?;
        *self.listing.borrow_mut() = generation.map(|generation| Listing {
            devices: devices.clone(),
            generation,
            read_at: std::time::Instant::now(),
        });
        Ok(devices)
    }

    fn read_devices(&self) -> Result<Vec<LocalDevice>, UsbBackendError> {
        let json = self.run(&["state"])?;
        let mut devices = parse_state(&json)?;
        // `usbipd state` carries no class, and the class is what decides
        // whether a device may be taken automatically — a Bluetooth radio
        // taken from a laptop cuts out the user's headphones. Windows knows it
        // through PnP, so it is asked separately and matched by vendor and
        // product, and cached: see `classes`.
        let present: Vec<(u16, u16)> = devices.iter().map(|d| (d.vendor, d.product)).collect();
        let mut slot = self.classes.borrow_mut();
        let classes = match slot.as_ref() {
            Some((read_for, classes)) if !needs_reread(read_for, &present) => classes,
            _ => {
                let fresh = pnp_classes();
                &slot.insert((present, fresh)).1
            }
        };
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
        // The elevated shell binds behind `run`'s back.
        self.listing.borrow_mut().take();
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
        let mut steps: Vec<String> = busids
            .iter()
            .map(|b| format!("& '{}' bind --busid {b}", self.exe.display()))
            .collect();
        // The same prompt closes usbipd's port to the network: this is the
        // one moment Cursio holds administrator rights, and asking again for
        // it alone would be a second dialog. See `LOOPBACK_GUARD`.
        steps.push(guard_script());
        let binds = steps.join("; ");
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

    /// Whether usbipd's port is closed to everything but this machine.
    ///
    /// Reading a firewall rule needs no elevation, so this is asked at
    /// startup and a machine still exposed is told so — the rule itself is
    /// only added by the next elevated setup.
    #[cfg(target_os = "windows")]
    pub fn is_guarded(&self) -> bool {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "show",
                "rule",
                &format!("name={LOOPBACK_GUARD}"),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .is_ok_and(|out| out.status.success())
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
        // Anything but `state` changes what `state` answers.
        if args.first() != Some(&"state") {
            self.listing.borrow_mut().take();
        }
        let mut command = Command::new(&self.exe);
        command.args(args);
        // `state` is polled for the life of the node. A node started from the
        // app or at login has no console, so without this every poll opens a
        // console window of its own and the screen flickers.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let output = command.output().map_err(|e| {
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

/// The firewall rule that keeps usbipd's port on this machine.
///
/// `usbipd-win` listens on `0.0.0.0:3240` and authenticates nobody: whoever
/// reaches the port can attach any bound device, which on a café's Wi-Fi is
/// anyone. Cursio reaches it only over loopback (`crate::wintunnel`) and
/// carries the device through the paired, encrypted mesh, so the port has no
/// business answering the network at all.
pub const LOOPBACK_GUARD: &str = "Cursio - usbipd from this machine only";

/// Every address except loopback, as `netsh` takes a remote-address list.
///
/// Spelled out rather than `any` and trusting the firewall's loopback
/// exemption, so the rule says what it means on its own: `127.0.0.0/8` and
/// `::1` are the gaps, and nothing else reaches 3240. `::` is unroutable and
/// left out with `::1`.
const NOT_LOOPBACK: &str = "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,\
                            ::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff";

/// Add [`LOOPBACK_GUARD`] from a process that already holds administrator
/// rights — an elevated node — so closing the port costs no prompt at all.
#[cfg(target_os = "windows")]
pub fn close_to_network() -> Result<(), UsbBackendError> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={LOOPBACK_GUARD}"),
            "dir=in",
            "action=block",
            "protocol=TCP",
            "localport=3240",
            &format!("remoteip={NOT_LOOPBACK}"),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| UsbBackendError::Enumerate(format!("cannot run netsh: {e}")))?;
    match output.status.success() {
        true => Ok(()),
        // netsh reports its failures on stdout.
        false => Err(UsbBackendError::Enumerate(format!(
            "netsh could not add the usbipd firewall rule: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        ))),
    }
}

/// `netsh` command a user can paste to add [`LOOPBACK_GUARD`] by hand.
pub fn close_to_network_command() -> String {
    format!(
        "netsh advfirewall firewall add rule name=\"{LOOPBACK_GUARD}\" dir=in action=block \
         protocol=TCP localport=3240 remoteip={NOT_LOOPBACK}"
    )
}

/// The PowerShell step that adds [`LOOPBACK_GUARD`] unless it is there.
///
/// Idempotent by checking first rather than deleting and re-adding, so a run
/// that fails halfway never leaves the port open where it was closed.
///
/// Each `key=value` is one single-quoted PowerShell string: unquoted, the
/// commas in the address list would make PowerShell pass an array, split into
/// separate arguments `netsh` would reject.
fn guard_script() -> String {
    format!(
        "netsh advfirewall firewall show rule 'name={LOOPBACK_GUARD}' | Out-Null; \
         if ($LASTEXITCODE -ne 0) {{ netsh advfirewall firewall add rule \
         'name={LOOPBACK_GUARD}' dir=in action=block protocol=TCP localport=3240 \
         'remoteip={NOT_LOOPBACK}' | Out-Null }}"
    )
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

/// Whether the cached classes have to be read again.
///
/// Only a device that was not there before can carry a class the cache does not
/// know: one that has gone away leaves an entry nobody asks about, and a device
/// cannot change class while it stays plugged in. So a *smaller* set is free,
/// and re-reading it would pay seconds to forget something harmless.
fn needs_reread(read_for: &[(u16, u16)], present: &[(u16, u16)]) -> bool {
    present.iter().any(|device| !read_for.contains(device))
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

    fn listing(generation: u64, age: std::time::Duration) -> Listing {
        Listing {
            devices: Vec::new(),
            generation,
            read_at: std::time::Instant::now() - age,
        }
    }

    #[test]
    fn a_listing_is_reused_until_a_device_arrives_or_leaves() {
        let kept = listing(4, std::time::Duration::from_secs(1));
        assert!(listing_is_current(&kept, Some(4)));
        assert!(!listing_is_current(&kept, Some(5)));
    }

    #[test]
    fn a_listing_is_reread_after_a_minute_even_without_notifications() {
        let kept = listing(4, LISTING_TRUSTED_FOR + std::time::Duration::from_secs(1));
        assert!(!listing_is_current(&kept, Some(4)));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_accepts_the_usb_arrival_registration() {
        assert!(device_changes().is_some());
    }

    #[test]
    fn without_notifications_every_listing_is_read_afresh() {
        let kept = listing(0, std::time::Duration::ZERO);
        assert!(!listing_is_current(&kept, None));
    }

    #[test]
    fn pasted_guard_command_blocks_the_same_addresses_as_the_setup_prompt() {
        let command = close_to_network_command();
        assert!(command.contains(&format!("remoteip={NOT_LOOPBACK}")));
        assert!(command.contains(LOOPBACK_GUARD));
        assert!(guard_script().contains(NOT_LOOPBACK));
    }

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

    /// `Get-PnpDevice` costs about 2.8 seconds — nine times `usbipd state` —
    /// and the listing is re-read every few seconds. Paying it each time
    /// blocked the session loop the shared cursor rides.
    #[test]
    fn classes_are_only_re_read_when_a_new_device_appears() {
        let phone = (0x2717, 0xFF08);
        let radio = (0x0489, 0xE0CD);

        assert!(
            !needs_reread(&[phone, radio], &[phone, radio]),
            "the same devices must not cost another pnp query"
        );
        assert!(
            !needs_reread(&[phone, radio], &[phone]),
            "a device going away teaches nothing new"
        );
        assert!(
            needs_reread(&[phone], &[phone, radio]),
            "a device that was not there before may have an unknown class"
        );
        assert!(
            !needs_reread(&[phone, radio], &[radio, phone]),
            "order is not a change"
        );
    }

    /// The rule is added inside an elevated shell nobody watches, so what it
    /// says is asserted here: a block on 3240 from everything but loopback,
    /// added only when absent.
    #[test]
    fn the_guard_blocks_usbipd_from_everything_but_loopback_and_only_once() {
        let script = guard_script();
        assert!(script.contains("action=block protocol=TCP localport=3240"));
        assert!(
            script.find("show rule") < script.find("add rule"),
            "checks before adding, so a rerun adds nothing"
        );
        assert!(NOT_LOOPBACK.contains("0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255"));
        assert!(NOT_LOOPBACK.contains("::2-"), "::1 must stay reachable");
        assert!(!NOT_LOOPBACK.contains(' '), "one argument, no spaces");
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
