//! The hands-free unit on BlueZ.
//!
//! BlueZ owns the adapter and SDP; an external profile registered with
//! `ProfileManager1.RegisterProfile` is handed each connected phone's RFCOMM
//! socket in `Profile1.NewConnection`. From there everything is ours: one
//! thread per phone runs [`HandsFree`] over that socket, and while a call is
//! up an SCO thread carries its voice.
//!
//! Plain threads, not the node's runtime: zbus's blocking calls deadlock on a
//! runtime thread (see `cursio_core::session_bus`), and both sockets are
//! blocking file descriptors read under `poll`.

mod voice;

mod channel;
mod devices;
mod media;
mod rfcomm;
mod sbc;
mod sco;
mod sdp;

pub use channel::MeshChannel;
pub use devices::{paired_computers, paired_phones};
pub use media::MediaSinkService;

/// Call `address`'s mesh channel. Blocking for as long as the radio pages
/// the device; see [`rfcomm::dial`].
pub fn dial(address: &str) -> Result<OwnedFd, BluetoothError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let channels = match sdp::channels(address) {
        Ok(channels) if channels.is_empty() => {
            return Err(BluetoothError::NoService(address.into()))
        }
        Ok(channels) => channels,
        Err(error) => {
            tracing::debug!(%address, %error, "fresh SDP lookup unavailable; trying the fixed mesh channel");
            vec![crate::MESH_CHANNEL]
        }
    };
    let mut last = BluetoothError::NoService(address.into());
    for channel in channels {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "bluetooth discovery and connect timed out",
            )
            .into());
        }
        tracing::debug!(%address, channel, "dialling the advertised mesh channel");
        match rfcomm::dial(address, channel, remaining) {
            Ok(socket) => return Ok(socket),
            Err(error) => last = error,
        }
    }
    Err(last)
}

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use zbus::blocking::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::linux::voice::run_link;
use crate::{BluetoothError, CallAudioFactory, HfCommand, LinkEvent};

const PROFILE_PATH: &str = "/dev/cursio/handsfree";
/// Hands-Free unit, the role a headset plays.
const HFP_HF_UUID: &str = "0000111e-0000-1000-8000-00805f9b34fb";
/// HFP 1.7, the version whose `+CLCC` and indicator handling this speaks.
const HFP_VERSION: u16 = 0x0107;
/// The SDP record's features: CLI presentation (bit 2) and remote volume
/// (bit 4).
const SDP_FEATURES: u16 = (1 << 2) | (1 << 4);
/// Wideband speech, in the record only where libsbc can carry it; see
/// [`crate::hands_free`].
const SDP_WIDEBAND: u16 = 1 << 5;
struct Link {
    commands: Sender<HfCommand>,
}

#[derive(Clone)]
struct Shared {
    links: Arc<Mutex<HashMap<String, Link>>>,
    events: Sender<LinkEvent>,
    audio: Arc<dyn CallAudioFactory>,
    bus: Connection,
}

impl Shared {
    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<String, Link>> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// This machine as the hands-free unit of every paired phone that connects.
pub struct HandsFreeService {
    shared: Shared,
}

struct Profile {
    shared: Arc<Mutex<Option<Shared>>>,
}

#[zbus::interface(name = "org.bluez.Profile1")]
impl Profile {
    fn release(&self) {
        tracing::info!("bluez released the hands-free profile");
    }

    fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: zbus::zvariant::OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) {
        let shared = self
            .shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(shared) = shared else { return };
        let Some(phone) = address_from_path(device.as_str()) else {
            tracing::warn!(device = %device.as_str(), "hands-free connection from a path that names no device");
            return;
        };
        let fd: OwnedFd = fd.into();
        let adapter = adapter_from_path(device.as_str());
        let spawned = std::thread::Builder::new()
            .name(format!("hfp-{phone}"))
            .spawn(move || run_link(shared, phone, adapter, device, fd));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot start the hands-free link thread");
        }
    }

    fn request_disconnection(&self, device: OwnedObjectPath) {
        let shared = self
            .shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let (Some(shared), Some(phone)) = (shared, address_from_path(device.as_str())) {
            shared.links().remove(&phone);
        }
    }
}

impl HandsFreeService {
    /// Register with BlueZ and take every phone that connects from now on.
    pub fn start(
        audio: Arc<dyn CallAudioFactory>,
    ) -> Result<(Self, Receiver<LinkEvent>), BluetoothError> {
        let slot = Arc::new(Mutex::new(None));
        let bus = zbus::blocking::connection::Builder::system()
            .and_then(|b| {
                b.serve_at(
                    PROFILE_PATH,
                    Profile {
                        shared: slot.clone(),
                    },
                )
            })
            .and_then(|b| b.build())
            .map_err(|e| BluetoothError::Bus(e.to_string()))?;
        let (events, events_rx) = mpsc::channel();
        let shared = Shared {
            links: Arc::default(),
            events,
            audio,
            bus: bus.clone(),
        };
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(shared.clone());

        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("Name", Value::from("Cursio hands-free"));
        options.insert("Version", Value::U16(HFP_VERSION));
        let features = if sbc::msbc_available() {
            SDP_FEATURES | SDP_WIDEBAND
        } else {
            SDP_FEATURES
        };
        options.insert("Features", Value::U16(features));
        options.insert("RequireAuthentication", Value::Bool(true));
        options.insert("AutoConnect", Value::Bool(true));
        let path = ObjectPath::try_from(PROFILE_PATH).expect("constant path is valid");
        bus.call_method(
            Some("org.bluez"),
            "/org/bluez",
            Some("org.bluez.ProfileManager1"),
            "RegisterProfile",
            &(path, HFP_HF_UUID, options),
        )
        .map_err(|e| BluetoothError::Register(e.to_string()))?;
        tracing::info!("registered as the hands-free unit for paired phones");
        Ok((HandsFreeService { shared }, events_rx))
    }

    pub fn command(&self, phone: &str, command: HfCommand) -> Result<(), BluetoothError> {
        let links = self.shared.links();
        let link = links
            .get(phone)
            .ok_or_else(|| BluetoothError::NotConnected(phone.to_string()))?;
        link.commands
            .send(command)
            .map_err(|_| BluetoothError::NotConnected(phone.to_string()))
    }

    /// Ask BlueZ to bring up the hands-free link to a paired phone that is in
    /// range but not connected.
    pub fn connect(&self, phone: &str) -> Result<(), BluetoothError> {
        let path = format!("/org/bluez/hci0/dev_{}", phone.replace(':', "_"));
        self.shared
            .bus
            .call_method(
                Some("org.bluez"),
                path.as_str(),
                Some("org.bluez.Device1"),
                "ConnectProfile",
                &(HFP_HF_UUID,),
            )
            .map(|_| ())
            .map_err(|e| BluetoothError::Bus(e.to_string()))
    }
}

/// `/org/bluez/hci0/dev_9C_9E_D5_78_FE_C4` → `9C:9E:D5:78:FE:C4`.
fn address_from_path(path: &str) -> Option<String> {
    let dev = path.rsplit('/').next()?.strip_prefix("dev_")?;
    let address = dev.replace('_', ":");
    (address.len() == 17).then_some(address)
}

fn adapter_from_path(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(adapter, _)| adapter.to_string())
        .unwrap_or_else(|| "/org/bluez/hci0".into())
}

fn property(bus: &Connection, path: &str, interface: &str, name: &str) -> Option<String> {
    let reply = bus
        .call_method(
            Some("org.bluez"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(interface, name),
        )
        .ok()?;
    let value: OwnedValue = reply.body().deserialize().ok()?;
    String::try_from(value).ok()
}

/// Whether a device of Bluetooth Class of Device `class` may be taken as a
/// phone. A laptop can play the gateway side of hands-free too — Windows does,
/// for its own Phone Link — and taken as a phone it showed up among the
/// calls with nothing to call. A device that gives no class is let through:
/// refusing it would refuse every phone whose stack leaves the field out.
fn takes_calls(class: Option<u32>) -> bool {
    class.is_none_or(devices::is_phone_class)
}

fn device_class(bus: &Connection, path: &str) -> Option<u32> {
    let reply = bus
        .call_method(
            Some("org.bluez"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.bluez.Device1", "Class"),
        )
        .ok()?;
    let value: OwnedValue = reply.body().deserialize().ok()?;
    u32::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_path_names_its_address_and_adapter() {
        let path = "/org/bluez/hci0/dev_9C_9E_D5_78_FE_C4";
        assert_eq!(
            address_from_path(path).as_deref(),
            Some("9C:9E:D5:78:FE:C4")
        );
        assert_eq!(adapter_from_path(path), "/org/bluez/hci0");
        assert_eq!(address_from_path("/org/bluez/hci0"), None);
    }

    #[test]
    fn only_a_phone_class_device_is_taken_as_a_phone() {
        // A smartphone (major class Phone) and a laptop (major class Computer)
        // as BlueZ reports them.
        assert!(takes_calls(Some(0x5a020c)));
        assert!(!takes_calls(Some(0x2a010c)));
        assert!(takes_calls(None));
    }
}
