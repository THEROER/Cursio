//! Phones and computers paired with this one, as BlueZ lists them.
//!
//! A device's kind is the major class of its class of device, or, when the
//! class is missing (a device known only from LE), the icon BlueZ gives it.
//! Read with one `GetManagedObjects` call rather than kept up to date: the
//! settings page asks every few seconds, and the mesh's Bluetooth dialler
//! only when a peer has gone missing.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::{BluetoothError, PairedDevice};

/// Bits 8..13 of the class of device.
const MAJOR_CLASS_COMPUTER: u32 = 0x01;
const MAJOR_CLASS_PHONE: u32 = 0x02;

type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

pub fn paired_phones() -> Result<Vec<PairedDevice>, BluetoothError> {
    paired(MAJOR_CLASS_PHONE, "phone")
}

/// Paired computers: the devices that may answer on the mesh channel.
pub fn paired_computers() -> Result<Vec<PairedDevice>, BluetoothError> {
    paired(MAJOR_CLASS_COMPUTER, "computer")
}

fn paired(major: u32, icon: &str) -> Result<Vec<PairedDevice>, BluetoothError> {
    let bus =
        zbus::blocking::Connection::system().map_err(|e| BluetoothError::Bus(e.to_string()))?;
    let reply = bus
        .call_method(
            Some("org.bluez"),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .map_err(|e| BluetoothError::Bus(e.to_string()))?;
    let objects: Objects = reply
        .body()
        .deserialize()
        .map_err(|e| BluetoothError::Bus(e.to_string()))?;
    let mut devices: Vec<PairedDevice> = objects
        .values()
        .filter_map(|interfaces| interfaces.get("org.bluez.Device1"))
        .filter_map(|props| device(props, major, icon))
        .collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(devices)
}

fn device(props: &HashMap<String, OwnedValue>, major: u32, icon: &str) -> Option<PairedDevice> {
    let flag = |name: &str| {
        props
            .get(name)
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false)
    };
    let text = |name: &str| {
        props
            .get(name)
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| String::try_from(v).ok())
    };
    if !flag("Paired") {
        return None;
    }
    let class = props.get("Class").and_then(|v| u32::try_from(v).ok());
    let matches = match class {
        Some(class) => major_class(class) == major,
        None => text("Icon").as_deref() == Some(icon),
    };
    if !matches {
        return None;
    }
    let address = text("Address")?;
    Some(PairedDevice {
        name: text("Alias").unwrap_or_else(|| address.clone()),
        address,
        connected: flag("Connected"),
    })
}

fn major_class(class: u32) -> u32 {
    (class >> 8) & 0x1F
}

/// Whether a Class of Device has the phone major class (bits 8–12).
pub(super) fn is_phone_class(class: u32) -> bool {
    major_class(class) == MAJOR_CLASS_PHONE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_smartphones_class_is_a_phone_and_a_laptops_is_not() {
        // Read from a paired POCO X7 Pro and a Windows laptop that offers
        // hands-free too.
        assert!(is_phone_class(0x5A420C));
        assert!(!is_phone_class(0x2E410C));
    }

    #[test]
    fn a_laptops_class_is_a_computer() {
        assert_eq!(major_class(0x2E410C), MAJOR_CLASS_COMPUTER);
        assert_ne!(major_class(0x5A420C), MAJOR_CLASS_COMPUTER);
    }
}
