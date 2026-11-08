//! Joining a group through NetworkManager, on the second station interface.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::{Ability, DirectError, Result};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const NM_DEVICE_TYPE_WIFI: u32 = 2;
const NM_DEVICE_STATE_ACTIVATED: u32 = 100;
const NM_ACTIVE_CONNECTION_STATE_ACTIVATED: u32 = 2;
const NM_ACTIVE_CONNECTION_STATE_DEACTIVATED: u32 = 4;

/// The interface `cursio wifi install` creates. Found by name: it is the only
/// way to tell it from the card's first station interface, which carries the
/// machine's own network and must never be touched.
pub const INTERFACE_PREFIX: &str = "wlcursio";

/// How long NetworkManager may take to find the group, associate and get an
/// address before the attempt is given up.
const JOIN_WITHIN: Duration = Duration::from_secs(40);

fn nm_err(e: impl std::fmt::Display) -> DirectError {
    DirectError::NetworkManager(e.to_string())
}

fn proxy<'a>(bus: &'a Connection, path: &str, iface: &str) -> Result<Proxy<'a>> {
    Proxy::new(bus, NM, path.to_owned(), iface.to_owned()).map_err(nm_err)
}

struct WifiDevice {
    path: OwnedObjectPath,
    name: String,
    state: u32,
}

fn wifi_devices(bus: &Connection) -> Result<Vec<WifiDevice>> {
    let nm = proxy(bus, NM_PATH, NM)?;
    let paths: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).map_err(nm_err)?;
    let mut out = Vec::new();
    for path in paths {
        let dev = proxy(bus, path.as_str(), DEVICE)?;
        let Ok(kind) = dev.get_property::<u32>("DeviceType") else {
            continue;
        };
        if kind != NM_DEVICE_TYPE_WIFI {
            continue;
        }
        let name: String = dev.get_property("Interface").map_err(nm_err)?;
        let state: u32 = dev.get_property("State").map_err(nm_err)?;
        out.push(WifiDevice { path, name, state });
    }
    Ok(out)
}

/// The frequency of the access point `device` is associated with.
fn frequency(bus: &Connection, device: &OwnedObjectPath) -> Option<u32> {
    let wifi = proxy(
        bus,
        device.as_str(),
        "org.freedesktop.NetworkManager.Device.Wireless",
    )
    .ok()?;
    let ap: OwnedObjectPath = wifi.get_property("ActiveAccessPoint").ok()?;
    if ap.as_str() == "/" {
        return None;
    }
    let ap = proxy(
        bus,
        ap.as_str(),
        "org.freedesktop.NetworkManager.AccessPoint",
    )
    .ok()?;
    ap.get_property::<u32>("Frequency").ok()
}

pub fn ability() -> Ability {
    let bus = match Connection::system() {
        Ok(bus) => bus,
        Err(e) => {
            return Ability {
                why_not: Some(format!("cannot reach NetworkManager: {e}")),
                ..Ability::default()
            }
        }
    };
    let devices = match wifi_devices(&bus) {
        Ok(d) => d,
        Err(e) => {
            return Ability {
                why_not: Some(e.to_string()),
                ..Ability::default()
            }
        }
    };
    let freq_mhz = devices
        .iter()
        .filter(|d| !d.name.starts_with(INTERFACE_PREFIX) && d.state == NM_DEVICE_STATE_ACTIVATED)
        .find_map(|d| frequency(&bus, &d.path));
    let second = devices
        .iter()
        .find(|d| d.name.starts_with(INTERFACE_PREFIX));
    if let Some(second) = second {
        keep_from_autoconnecting(&bus, &second.path);
    }
    Ability {
        host: false,
        join: second.is_some(),
        freq_mhz,
        why_not: second.is_none().then(|| {
            "no second Wi-Fi interface to join a direct link with; `cursio wifi install` creates one"
                .to_string()
        }),
    }
}

/// Left to itself NetworkManager would bring a saved network up on the second
/// interface — the router's own, beside the first one — which is a second
/// default route nobody asked for. Only Cursio's own connection may use it.
/// The flag is the device's runtime state, owned by whoever drives the device,
/// and is not written anywhere.
fn keep_from_autoconnecting(bus: &Connection, device: &OwnedObjectPath) {
    let Ok(dev) = proxy(bus, device.as_str(), DEVICE) else {
        return;
    };
    if dev.get_property::<bool>("Autoconnect").unwrap_or(false) {
        if let Err(e) = dev.set_property("Autoconnect", false) {
            tracing::warn!(error = %e, "could not keep NetworkManager from using the direct-link interface for other networks");
        }
    }
}

/// This machine's station on a group: the D-Bus connection the activation is
/// bound to, and the activation itself.
pub struct Station {
    bus: Connection,
    active: OwnedObjectPath,
}

impl Station {
    pub fn is_up(&self) -> bool {
        proxy(
            &self.bus,
            self.active.as_str(),
            "org.freedesktop.NetworkManager.Connection.Active",
        )
        .and_then(|p| p.get_property::<u32>("State").map_err(nm_err))
        .is_ok_and(|s| s == NM_ACTIVE_CONNECTION_STATE_ACTIVATED)
    }
}

impl Drop for Station {
    fn drop(&mut self) {
        if let Ok(nm) = proxy(&self.bus, NM_PATH, NM) {
            let _: zbus::Result<()> = nm.call("DeactivateConnection", &(&self.active,));
        }
    }
}

pub fn join(ssid: &str, passphrase: &str) -> Result<(Station, Ipv4Addr, Ipv4Addr, Option<u32>)> {
    let bus = Connection::system().map_err(nm_err)?;
    let device = wifi_devices(&bus)?
        .into_iter()
        .find(|d| d.name.starts_with(INTERFACE_PREFIX))
        .ok_or_else(|| {
            DirectError::Unsupported(
                "no second Wi-Fi interface; `cursio wifi install` creates one".into(),
            )
        })?;

    let mut connection: HashMap<&str, Value> = HashMap::new();
    connection.insert("id", Value::from("Cursio direct link"));
    connection.insert("type", Value::from("802-11-wireless"));
    connection.insert("interface-name", Value::from(device.name.as_str()));
    connection.insert("autoconnect", Value::from(false));
    let mut wireless: HashMap<&str, Value> = HashMap::new();
    wireless.insert("ssid", Value::from(ssid.as_bytes().to_vec()));
    wireless.insert("mode", Value::from("infrastructure"));
    // NM_SETTING_WIRELESS_POWERSAVE_DISABLE. With power saving on, the median
    // round trip stayed 1.6 ms but the mean was 50 ms: the card dozes between
    // beacons and a frame waits for it to wake.
    wireless.insert("powersave", Value::from(2u32));
    let mut security: HashMap<&str, Value> = HashMap::new();
    security.insert("key-mgmt", Value::from("wpa-psk"));
    security.insert("psk", Value::from(passphrase));
    let mut ipv4: HashMap<&str, Value> = HashMap::new();
    ipv4.insert("method", Value::from("auto"));
    // The group must never become the way to the internet: only the mesh
    // dials over it, by its address.
    ipv4.insert("never-default", Value::from(true));
    ipv4.insert("ignore-auto-dns", Value::from(true));
    let mut ipv6: HashMap<&str, Value> = HashMap::new();
    ipv6.insert("method", Value::from("disabled"));
    let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
    settings.insert("connection", connection);
    settings.insert("802-11-wireless", wireless);
    settings.insert("802-11-wireless-security", security);
    settings.insert("ipv4", ipv4);
    settings.insert("ipv6", ipv6);
    let mut options: HashMap<&str, Value> = HashMap::new();
    // Never written to disk, and torn down by NetworkManager itself when this
    // D-Bus connection goes — a node that crashes leaves no link behind.
    options.insert("persist", Value::from("volatile"));
    options.insert("bind-activation", Value::from("dbus-client"));

    let root = OwnedObjectPath::try_from("/").map_err(nm_err)?;
    let (_settings, active, _result): (
        OwnedObjectPath,
        OwnedObjectPath,
        HashMap<String, OwnedValue>,
    ) = proxy(&bus, NM_PATH, NM)?
        .call(
            "AddAndActivateConnection2",
            &(settings, &device.path, &root, options),
        )
        .map_err(|e| DirectError::Join(e.to_string()))?;
    let station = Station { bus, active };

    let started = Instant::now();
    loop {
        let state = proxy(
            &station.bus,
            station.active.as_str(),
            "org.freedesktop.NetworkManager.Connection.Active",
        )
        .and_then(|p| p.get_property::<u32>("State").map_err(nm_err));
        match state {
            Ok(NM_ACTIVE_CONNECTION_STATE_ACTIVATED) => break,
            Ok(NM_ACTIVE_CONNECTION_STATE_DEACTIVATED) | Err(_) => {
                return Err(DirectError::Join(format!(
                    "NetworkManager could not bring up {ssid} on {}; is the other machine near enough?",
                    device.name
                )))
            }
            Ok(_) if started.elapsed() > JOIN_WITHIN => {
                return Err(DirectError::Join(format!(
                    "{ssid} did not come up within {} s",
                    JOIN_WITHIN.as_secs()
                )))
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }

    let (local, host) = addresses(&station.bus, &device.path)?;
    let freq = frequency(&station.bus, &device.path);
    Ok((station, local, host, freq))
}

/// This machine's address on the group, and the host's: the DHCP server that
/// handed the address out is the group's owner.
fn addresses(bus: &Connection, device: &OwnedObjectPath) -> Result<(Ipv4Addr, Ipv4Addr)> {
    let dev = proxy(bus, device.as_str(), DEVICE)?;
    let ip4: OwnedObjectPath = dev.get_property("Ip4Config").map_err(nm_err)?;
    let config = proxy(
        bus,
        ip4.as_str(),
        "org.freedesktop.NetworkManager.IP4Config",
    )?;
    let data: Vec<HashMap<String, OwnedValue>> =
        config.get_property("AddressData").map_err(nm_err)?;
    let local = data
        .iter()
        .find_map(|a| {
            let v = a.get("address")?;
            String::try_from(v.try_clone().ok()?).ok()?.parse().ok()
        })
        .ok_or_else(|| DirectError::Join("the group gave this machine no address".into()))?;
    let dhcp: OwnedObjectPath = dev.get_property("Dhcp4Config").map_err(nm_err)?;
    let options: HashMap<String, OwnedValue> = proxy(
        bus,
        dhcp.as_str(),
        "org.freedesktop.NetworkManager.DHCP4Config",
    )?
    .get_property("Options")
    .map_err(nm_err)?;
    let host = ["dhcp_server_identifier", "routers"]
        .iter()
        .find_map(|key| {
            let v = options.get(*key)?;
            let s = String::try_from(v.try_clone().ok()?).ok()?;
            s.split_whitespace().next()?.parse().ok()
        })
        .ok_or_else(|| DirectError::Join("the group did not say which machine owns it".into()))?;
    Ok((local, host))
}
