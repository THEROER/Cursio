//! Phones paired with this computer, as BlueZ lists them.
//!
//! A phone is a paired device whose class of device has the "phone" major
//! class, or which BlueZ gives the `phone` icon when the class is missing (a
//! device known only from LE). Read with one `GetManagedObjects` call rather
//! than kept up to date: the settings page is the only reader, and it asks
//! again every few seconds.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::{BluetoothError, PairedDevice};

/// Bits 8..13 of the class of device.
const MAJOR_CLASS_PHONE: u32 = 0x02;

type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

pub fn paired_phones() -> Result<Vec<PairedDevice>, BluetoothError> {
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
    let mut phones: Vec<PairedDevice> = objects
        .values()
        .filter_map(|interfaces| interfaces.get("org.bluez.Device1"))
        .filter_map(phone)
        .collect();
    phones.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(phones)
}

fn phone(props: &HashMap<String, OwnedValue>) -> Option<PairedDevice> {
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
    let is_phone = match class {
        Some(class) => is_phone_class(class),
        None => text("Icon").as_deref() == Some("phone"),
    };
    if !is_phone {
        return None;
    }
    let address = text("Address")?;
    Some(PairedDevice {
        name: text("Alias").unwrap_or_else(|| address.clone()),
        address,
        connected: flag("Connected"),
    })
}

fn is_phone_class(class: u32) -> bool {
    (class >> 8) & 0x1F == MAJOR_CLASS_PHONE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_smartphones_class_is_a_phone_and_a_laptops_is_not() {
        // A Xiaomi phone, and a Windows laptop that offers hands-free too.
        assert!(is_phone_class(0x5A020C));
        assert!(!is_phone_class(0x2A010C));
    }
}
