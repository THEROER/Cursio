//! Dual AirPlay/RAOP mDNS advertisement.

use std::net::IpAddr;

use cursio_transport::discovery::DiscoveryConfig;
#[cfg(not(target_os = "macos"))]
use mdns_sd::ServiceDaemon;
use mdns_sd::ServiceInfo;

use crate::{AirPlayError, ReceiverConfig};

pub const AIRPLAY_SERVICE_TYPE: &str = "_airplay._tcp.local.";
pub const RAOP_SERVICE_TYPE: &str = "_raop._tcp.local.";

/// Feature bits used by the Apple TV compatibility profile.
///
/// Keep this isolated: every bit is a protocol promise. Pairing and media
/// handlers must eventually narrow this value to what they actually implement.
pub const AIRPLAY_FEATURES: u64 = 0x527f_fee6;
const AIRPLAY_FEATURES_TXT: &str = "0x527FFEE6,0x0";
const LEGACY_RECEIVER_PUBLIC_KEY_HEX: &str =
    "b07727d6f6cd6e08b58ede525ec3cdeaa252ad9f683feb212ef8a205246554e7";
pub(crate) const LEGACY_RECEIVER_PUBLIC_KEY: [u8; 32] = [
    0xb0, 0x77, 0x27, 0xd6, 0xf6, 0xcd, 0x6e, 0x08, 0xb5, 0x8e, 0xde, 0x52, 0x5e, 0xc3, 0xcd, 0xea,
    0xa2, 0x52, 0xad, 0x9f, 0x68, 0x3f, 0xeb, 0x21, 0x2e, 0xf8, 0xa2, 0x05, 0x24, 0x65, 0x54, 0xe7,
];

/// Active registration for both service records.
#[cfg(not(target_os = "macos"))]
pub struct AirPlayAnnouncement {
    daemon: ServiceDaemon,
    fullnames: [String; 2],
}

#[cfg(target_os = "macos")]
pub struct AirPlayAnnouncement {
    _airplay: astro_dnssd::RegisteredDnsService,
    _raop: astro_dnssd::RegisteredDnsService,
}

#[cfg(not(target_os = "macos"))]
impl AirPlayAnnouncement {
    pub fn register(
        receiver: &ReceiverConfig,
        port: u16,
        discovery: &DiscoveryConfig,
    ) -> Result<Self, AirPlayError> {
        let daemon = discovery.new_daemon()?;
        let services = service_infos(receiver, port, discovery.pin_interface_addr)?;
        let fullnames = std::array::from_fn(|index| services[index].get_fullname().to_string());

        daemon
            .register(services[0].clone())
            .map_err(|error| AirPlayError::Mdns(error.to_string()))?;
        if let Err(error) = daemon.register(services[1].clone()) {
            let _ = daemon.unregister(&fullnames[0]);
            return Err(AirPlayError::Mdns(error.to_string()));
        }

        Ok(Self { daemon, fullnames })
    }
}

#[cfg(target_os = "macos")]
impl AirPlayAnnouncement {
    pub fn register(
        receiver: &ReceiverConfig,
        port: u16,
        discovery: &DiscoveryConfig,
    ) -> Result<Self, AirPlayError> {
        let services = service_infos(receiver, port, discovery.pin_interface_addr)?;
        let airplay = native_register("_airplay._tcp", receiver.name.trim(), port, &services[0])?;
        let raop = native_register(
            "_raop._tcp",
            &format!(
                "{}@{}",
                receiver.device_id.raop_prefix(),
                receiver.name.trim()
            ),
            port,
            &services[1],
        )?;
        Ok(Self {
            _airplay: airplay,
            _raop: raop,
        })
    }
}

#[cfg(target_os = "macos")]
fn native_register(
    service_type: &str,
    instance: &str,
    port: u16,
    info: &ServiceInfo,
) -> Result<astro_dnssd::RegisteredDnsService, AirPlayError> {
    let properties = info
        .get_properties()
        .iter()
        .map(|property| (property.key().to_string(), property.val_str().to_string()))
        .collect();
    astro_dnssd::DNSServiceBuilder::new(service_type, port)
        .with_name(instance)
        .with_txt_record(properties)
        .register()
        .map_err(|error| AirPlayError::Mdns(format!("{error:?}")))
}

#[cfg(not(target_os = "macos"))]
impl Drop for AirPlayAnnouncement {
    fn drop(&mut self) {
        for fullname in &self.fullnames {
            let _ = self.daemon.unregister(fullname);
        }
    }
}

fn service_infos(
    receiver: &ReceiverConfig,
    port: u16,
    pinned_addr: Option<IpAddr>,
) -> Result<[ServiceInfo; 2], AirPlayError> {
    let instance = receiver.name.trim();
    let host = format!("{}.local.", host_label(instance));
    let device_id = receiver.device_id.to_string();
    let identity = receiver.identity.to_string();
    let airplay_properties = [
        ("deviceid", device_id),
        ("features", AIRPLAY_FEATURES_TXT.to_string()),
        ("flags", "0x4".to_string()),
        ("model", receiver.model.clone()),
        ("pk", LEGACY_RECEIVER_PUBLIC_KEY_HEX.to_string()),
        ("pi", identity),
        ("srcvers", receiver.source_version.clone()),
        ("vv", "2".to_string()),
    ];
    let raop_properties = [
        ("ch", "2".to_string()),
        ("cn", "0,1,2,3".to_string()),
        ("da", "true".to_string()),
        ("et", "0,3,5".to_string()),
        ("ft", AIRPLAY_FEATURES_TXT.to_string()),
        ("am", receiver.model.clone()),
        ("md", "0,1,2".to_string()),
        ("pk", LEGACY_RECEIVER_PUBLIC_KEY_HEX.to_string()),
        ("pw", "false".to_string()),
        ("rhd", "5.6.0.0".to_string()),
        ("sr", "44100".to_string()),
        ("sf", "0x4".to_string()),
        ("ss", "16".to_string()),
        ("sv", "false".to_string()),
        ("tp", "UDP".to_string()),
        ("txtvers", "1".to_string()),
        ("vn", "65537".to_string()),
        ("vs", receiver.source_version.clone()),
        ("vv", "2".to_string()),
    ];
    let raop_instance = format!("{}@{instance}", receiver.device_id.raop_prefix());

    let airplay = new_service(
        AIRPLAY_SERVICE_TYPE,
        instance,
        &host,
        pinned_addr,
        port,
        &airplay_properties,
    )?;
    let raop = new_service(
        RAOP_SERVICE_TYPE,
        &raop_instance,
        &host,
        pinned_addr,
        port,
        &raop_properties,
    )?;
    Ok([airplay, raop])
}

fn new_service(
    service_type: &str,
    instance: &str,
    host: &str,
    pinned_addr: Option<IpAddr>,
    port: u16,
    properties: &[(&str, String)],
) -> Result<ServiceInfo, AirPlayError> {
    let service = match pinned_addr {
        Some(addr) => ServiceInfo::new(service_type, instance, host, addr, port, properties),
        None => ServiceInfo::new(service_type, instance, host, (), port, properties)
            .map(ServiceInfo::enable_addr_auto),
    };
    service.map_err(|error| AirPlayError::Mdns(error.to_string()))
}

fn host_label(name: &str) -> String {
    let label: String = name
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                Some(character)
            } else if character.is_whitespace() || character == '_' {
                Some('-')
            } else {
                None
            }
        })
        .take(48)
        .collect();
    let label = label.trim_matches('-');
    if label.is_empty() {
        "cursio-airplay".to_string()
    } else {
        label.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Resolution, VideoProfile};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use uuid::Uuid;

    fn config(name: &str) -> ReceiverConfig {
        ReceiverConfig::new(
            name,
            Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
            SocketAddr::from(([0, 0, 0, 0], 7000)),
            VideoProfile::new(Resolution::FullHd, 60).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn builds_matching_airplay_and_raop_records() {
        let receiver = config("Living Room");
        let services = service_infos(
            &receiver,
            7100,
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))),
        )
        .unwrap();

        assert_eq!(services[0].get_type(), AIRPLAY_SERVICE_TYPE);
        assert_eq!(services[1].get_type(), RAOP_SERVICE_TYPE);
        assert_eq!(services[0].get_port(), 7100);
        assert_eq!(services[1].get_port(), 7100);
        assert_eq!(services[0].get_hostname(), services[1].get_hostname());
        assert_eq!(
            services[0].get_property_val_str("deviceid"),
            Some("02:11:22:33:44:55")
        );
        assert_eq!(
            services[0].get_property_val_str("features"),
            Some(AIRPLAY_FEATURES_TXT)
        );
        assert_eq!(
            services[0].get_property_val_str("model"),
            Some(receiver.model.as_str())
        );
        assert!(services[1]
            .get_fullname()
            .starts_with("021122334455@Living Room."));
        for key in ["ch", "cn", "et", "md", "pw", "sr", "ss", "tp", "txtvers"] {
            assert!(services[1].get_property(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn automatic_addresses_are_enabled_when_not_pinned() {
        let services = service_infos(&config("Cursio"), 7000, None).unwrap();
        assert!(services[0].is_addr_auto());
        assert!(services[1].is_addr_auto());
    }

    #[test]
    fn host_label_is_safe_and_has_a_fallback() {
        assert_eq!(host_label("Living Room_1"), "Living-Room-1");
        assert_eq!(host_label("💻"), "cursio-airplay");
    }
}
