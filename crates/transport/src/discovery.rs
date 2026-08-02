//! LAN peer discovery over mDNS.
//!
//! Each Cursio node announces a `_cursio._tcp.local.` service carrying its
//! peer id, display name, and listening port. Browsing the same service type
//! yields the other nodes on the LAN. `mdns-sd` is pure Rust and needs no
//! system daemon, so this works identically on Ubuntu and Windows.

use std::net::{IpAddr, SocketAddr};

use cursio_protocol::PeerId;
use mdns_sd::{IfKind, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::TransportError;

/// The mDNS service type all Cursio nodes register and browse.
pub const SERVICE_TYPE: &str = "_cursio._tcp.local.";

const TXT_PEER_ID: &str = "peer_id";
const TXT_DISPLAY_NAME: &str = "name";

/// Controls which network interfaces discovery uses.
///
/// By default `mdns-sd` sends multicast on *every* interface, including VPN and
/// virtual ones (WireGuard, Tailscale, Hamachi, docker/lxc bridges). Those
/// often reject multicast — e.g. WireGuard returns `Required key not available`
/// — spamming errors and making discovery unreliable. This config lets us keep
/// mDNS on the real LAN.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryConfig {
    /// If set, restrict discovery to exactly this interface (by its IPv4/IPv6
    /// address). When known, this is the most reliable option: everything else
    /// is disabled. Typically the address of the Wi-Fi/Ethernet LAN interface.
    pub pin_interface_addr: Option<IpAddr>,
}

impl DiscoveryConfig {
    /// Pin discovery to the interface owning `addr` (the LAN interface).
    pub fn pinned_to(addr: IpAddr) -> Self {
        Self {
            pin_interface_addr: Some(addr),
        }
    }

    /// Build a daemon configured per this config.
    ///
    /// - Always disables IPv6: Cursio reaches peers over IPv4 on the LAN, and
    ///   IPv6 link-local multicast on virtual interfaces is a major noise
    ///   source.
    /// - If an interface is pinned, disables *all* interfaces then re-enables
    ///   just the pinned one, so VPN/virtual interfaces can't be used.
    /// - Otherwise, best-effort excludes interfaces whose address falls in
    ///   known-virtual ranges (see [`is_virtual_addr`]).
    pub fn new_daemon(&self) -> Result<ServiceDaemon, TransportError> {
        let daemon = ServiceDaemon::new().map_err(|e| TransportError::Other(e.to_string()))?;

        // IPv4-only on the LAN; drop all the IPv6 link-local multicast noise.
        daemon
            .disable_interface(IfKind::IPv6)
            .map_err(|e| TransportError::Other(e.to_string()))?;

        match self.pin_interface_addr {
            Some(addr) => {
                // Exclusive pin: turn everything off, then enable just this one.
                daemon
                    .disable_interface(IfKind::All)
                    .map_err(|e| TransportError::Other(e.to_string()))?;
                daemon
                    .enable_interface(IfKind::Addr(addr))
                    .map_err(|e| TransportError::Other(e.to_string()))?;
            }
            None => {
                // No pin: best-effort exclude known-virtual interfaces so we
                // don't attempt multicast on VPNs/bridges that will fail.
                for addr in virtual_interface_addrs() {
                    daemon
                        .disable_interface(IfKind::Addr(addr))
                        .map_err(|e| TransportError::Other(e.to_string()))?;
                }
            }
        }

        Ok(daemon)
    }
}

/// True if `addr` looks like it belongs to a VPN or virtual interface rather
/// than a real LAN. These ranges routinely reject multicast.
///
/// - `100.64.0.0/10` — CGNAT range, used by Tailscale.
/// - `25.0.0.0/8` — legacy Hamachi range.
/// - `172.16.0.0/12` — docker/lxc default bridge networks.
///
/// Deliberately does *not* include `192.168.0.0/16` or `10.0.0.0/8` broadly,
/// since a real home LAN commonly lives there; over-excluding would break the
/// normal case. WireGuard/other point-to-point interfaces are best handled by
/// pinning the LAN interface explicitly ([`DiscoveryConfig::pinned_to`]).
pub fn is_virtual_addr(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // 100.64.0.0/10 (CGNAT / Tailscale)
            (o[0] == 100 && (o[1] & 0xC0) == 0x40)
                // 25.0.0.0/8 (Hamachi)
                || o[0] == 25
                // 172.16.0.0/12 (docker/lxc bridges)
                || (o[0] == 172 && (0x10..=0x1F).contains(&o[1]))
        }
        IpAddr::V6(_) => true, // IPv6 is disabled wholesale anyway.
    }
}

/// Enumerate local interface addresses that look virtual, so they can be
/// excluded from mDNS. Best-effort: returns empty if enumeration is
/// unavailable, in which case we simply don't exclude anything.
fn virtual_interface_addrs() -> Vec<IpAddr> {
    local_interface_addrs()
        .into_iter()
        .filter(is_virtual_addr)
        .collect()
}

/// Best guess at this host's real LAN IPv4 address.
///
/// Considers only non-loopback, non-virtual (see [`is_virtual_addr`]) IPv4
/// interfaces, then prefers the range most likely to be a home/office LAN so
/// the choice is deterministic on hosts with several candidates:
/// `192.168.0.0/16` first, then `10.0.0.0/8`, then anything else. Returns
/// `None` if nothing qualifies.
///
/// Note: WireGuard commonly lives in `10.0.0.0/8`, which [`is_virtual_addr`]
/// does *not* exclude (a real LAN can be there too). The `192.168` preference
/// keeps us on the Wi-Fi/Ethernet interface on such hosts; when it's ambiguous,
/// pin explicitly via [`DiscoveryConfig::pinned_to`].
pub fn guess_lan_ipv4() -> Option<IpAddr> {
    let candidates: Vec<IpAddr> = local_interface_addrs()
        .into_iter()
        .filter(|a| a.is_ipv4() && !is_virtual_addr(a))
        .collect();

    // Rank: 192.168/16 (0) < 10/8 (1) < other (2). Lowest wins.
    fn rank(addr: &IpAddr) -> u8 {
        match addr {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                if o[0] == 192 && o[1] == 168 {
                    0
                } else if o[0] == 10 {
                    1
                } else {
                    2
                }
            }
            IpAddr::V6(_) => 3,
        }
    }

    candidates.into_iter().min_by_key(rank)
}

/// All non-loopback local interface addresses. Best-effort: an enumeration
/// failure yields an empty list rather than an error, since discovery can still
/// proceed (just without virtual-interface exclusion).
fn local_interface_addrs() -> Vec<IpAddr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter(|i| !i.is_loopback())
            .map(|i| i.ip())
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not enumerate interfaces; skipping virtual-iface exclusion");
            Vec::new()
        }
    }
}

/// Turn a machine's display name into a label mDNS will carry.
///
/// One function, because every service this process announces has to answer the
/// same question and there is no reason for two of them to answer it
/// differently. Cursio's own record and AirPlay's both go through here.
///
/// It is a *host* label, which is why the rules are strict: [`announce_with`]
/// uses the same string for the service instance and for `<instance>.local.`,
/// and the second of those is DNS, where a Cyrillic or accented name is not a
/// label at all. Nothing readable is lost — the name a user sees travels in the
/// TXT record, not here.
///
/// The rules are DNS', not a preference. A label is ASCII letters, digits and
/// hyphens. Punctuation and spaces become hyphens rather than vanishing — a
/// Mac calls itself `macbook.local`, and the dot is a label *separator*, so it
/// cannot survive inside one while `macbook-local` still reads as the same
/// machine. Non-ASCII is dropped instead, because the alternative is guessing
/// at a romanisation the user never chose. Leading and trailing hyphens go too,
/// since a label may not begin or end with one, and the 48-character cap leaves
/// room for the service suffix inside DNS' 63-byte limit.
///
/// `fallback` is what a name made entirely of unusable characters becomes, so
/// the record is still published rather than silently malformed.
pub fn mdns_label(name: &str, fallback: &str) -> String {
    let label: String = name
        .chars()
        .filter_map(|character| match character {
            c if c.is_ascii_alphanumeric() || c == '-' => Some(c),
            c if c.is_ascii() || c.is_whitespace() => Some('-'),
            _ => None,
        })
        .take(MAX_LABEL_CHARS)
        .collect();
    match label.trim_matches('-') {
        "" => fallback.to_string(),
        trimmed => trimmed.to_string(),
    }
}

/// How much of a name a label keeps.
///
/// DNS allows 63 bytes; the rest is headroom for the service type appended
/// after it, and a name long enough to hit this is not one anybody reads.
const MAX_LABEL_CHARS: usize = 48;

/// A peer found on the LAN.
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    /// Stable identity advertised by the peer.
    pub peer_id: PeerId,
    /// Human-friendly name for UI/logs.
    pub display_name: String,
    /// Where to reach the peer's TCP transport.
    pub addr: SocketAddr,
}

/// Handle to an active mDNS announcement. Dropping it unregisters the service.
pub struct Announcement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for Announcement {
    fn drop(&mut self) {
        // Best-effort: unregister so peers stop seeing us promptly.
        let _ = self.daemon.unregister(&self.fullname);
    }
}

/// Announce this node on the LAN so peers can discover it.
///
/// `port` is the TCP port our transport listens on; `instance` should be a
/// stable per-host label (e.g. the hostname) so the service name is readable.
/// Uses a default [`DiscoveryConfig`]; see [`announce_with`] to pin an
/// interface.
pub fn announce(
    peer_id: PeerId,
    display_name: &str,
    instance: &str,
    port: u16,
) -> Result<Announcement, TransportError> {
    announce_with(
        peer_id,
        display_name,
        instance,
        port,
        &DiscoveryConfig::default(),
    )
}

/// Like [`announce`], but with an explicit [`DiscoveryConfig`] to control which
/// interfaces mDNS uses.
pub fn announce_with(
    peer_id: PeerId,
    display_name: &str,
    instance: &str,
    port: u16,
    config: &DiscoveryConfig,
) -> Result<Announcement, TransportError> {
    let daemon = config.new_daemon()?;

    let properties = [
        (TXT_PEER_ID, peer_id.to_string()),
        (TXT_DISPLAY_NAME, display_name.to_string()),
    ];

    // Host name must end in `.local.`.
    let host_name = format!("{instance}.local.");
    let service = match config.pin_interface_addr {
        // Pinned: advertise exactly the LAN address. Avoids `enable_addr_auto`,
        // whose initial announce fans out over every host address (including
        // VPN/virtual ones) and triggers a one-off multicast error at startup.
        Some(addr) => ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            &host_name,
            addr,
            port,
            &properties[..],
        )
        .map_err(|e| TransportError::Other(e.to_string()))?,
        // Unpinned: let the daemon resolve this host's addresses itself.
        None => ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            &host_name,
            (),
            port,
            &properties[..],
        )
        .map_err(|e| TransportError::Other(e.to_string()))?
        .enable_addr_auto(),
    };

    let fullname = service.get_fullname().to_string();
    daemon
        .register(service)
        .map_err(|e| TransportError::Other(e.to_string()))?;

    Ok(Announcement { daemon, fullname })
}

/// Browse the LAN for Cursio peers, invoking `on_peer` for each one found.
///
/// This blocks the calling task, draining discovery events until the mDNS
/// channel closes. Peers matching `self_id` are skipped so a node never
/// discovers itself. Run it on a dedicated task.
pub fn browse<F>(self_id: PeerId, on_peer: F) -> Result<(), TransportError>
where
    F: FnMut(DiscoveredPeer),
{
    browse_with(self_id, &DiscoveryConfig::default(), on_peer)
}

/// Like [`browse`], but with an explicit [`DiscoveryConfig`] to control which
/// interfaces mDNS uses.
pub fn browse_with<F>(
    self_id: PeerId,
    config: &DiscoveryConfig,
    mut on_peer: F,
) -> Result<(), TransportError>
where
    F: FnMut(DiscoveredPeer),
{
    let daemon = config.new_daemon()?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| TransportError::Other(e.to_string()))?;

    while let Ok(event) = receiver.recv() {
        if let ServiceEvent::ServiceResolved(info) = event {
            if let Some(peer) = peer_from_info(&info, self_id) {
                on_peer(peer);
            }
        }
    }
    Ok(())
}

/// Build a [`DiscoveredPeer`] from a resolved service, or `None` if it's us,
/// malformed, or has no reachable address.
fn peer_from_info(info: &ResolvedService, self_id: PeerId) -> Option<DiscoveredPeer> {
    let peer_id: PeerId = info
        .get_property_val_str(TXT_PEER_ID)
        .and_then(|s| s.parse().ok())?;

    if peer_id == self_id {
        return None;
    }

    let display_name = info
        .get_property_val_str(TXT_DISPLAY_NAME)
        .unwrap_or("unknown")
        .to_string();

    // Prefer an IPv4 address for LAN reachability; fall back to any address.
    let ip: IpAddr = info
        .get_addresses()
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| info.get_addresses().iter().next())
        .map(|address| address.to_ip_addr())?;

    Some(DiscoveredPeer {
        peer_id,
        display_name,
        addr: SocketAddr::new(ip, info.get_port()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn real_lan_addresses_are_not_virtual() {
        // Common home-LAN ranges must be treated as real, or we'd break the
        // normal case by excluding the interface we actually need.
        assert!(!is_virtual_addr(&v4(192, 168, 1, 21)));
        assert!(!is_virtual_addr(&v4(192, 168, 0, 5)));
        assert!(!is_virtual_addr(&v4(10, 0, 0, 4)));
    }

    #[test]
    fn vpn_and_bridge_addresses_are_virtual() {
        assert!(is_virtual_addr(&v4(100, 112, 136, 115)), "Tailscale CGNAT");
        assert!(is_virtual_addr(&v4(100, 64, 0, 1)), "CGNAT low edge");
        assert!(is_virtual_addr(&v4(100, 127, 255, 254)), "CGNAT high edge");
        assert!(is_virtual_addr(&v4(25, 20, 64, 124)), "Hamachi");
        assert!(is_virtual_addr(&v4(172, 17, 0, 1)), "docker bridge");
        assert!(
            is_virtual_addr(&v4(172, 31, 255, 1)),
            "docker/lxc high edge"
        );
    }

    #[test]
    fn cgnat_boundary_is_respected() {
        // 100.64.0.0/10 spans 100.64.0.0 – 100.127.255.255. Addresses just
        // outside must not be misclassified.
        assert!(!is_virtual_addr(&v4(100, 63, 255, 255)), "just below range");
        assert!(!is_virtual_addr(&v4(100, 128, 0, 0)), "just above range");
    }

    #[test]
    fn addr_172_outside_docker_range_is_real() {
        // 172.16.0.0/12 is .16–.31; 172.15 and 172.32 are outside it.
        assert!(!is_virtual_addr(&v4(172, 15, 0, 1)));
        assert!(!is_virtual_addr(&v4(172, 32, 0, 1)));
    }

    #[test]
    fn pinned_config_records_the_address() {
        let cfg = DiscoveryConfig::pinned_to(v4(192, 168, 1, 21));
        assert_eq!(cfg.pin_interface_addr, Some(v4(192, 168, 1, 21)));
    }

    #[test]
    fn a_label_keeps_what_dns_allows_and_bridges_the_rest() {
        assert_eq!(mdns_label("Living Room_1", "fallback"), "Living-Room-1");
        // The dot is a label separator, so it becomes a hyphen rather than
        // being dropped: `macbooklocal` is not the machine anybody named.
        assert_eq!(mdns_label("macbook.local", "fallback"), "macbook-local");
        assert_eq!(mdns_label("DESKTOP-N3R13HQ", "fallback"), "DESKTOP-N3R13HQ");
    }

    /// A label may not begin or end with a hyphen, and a name whose edges are
    /// punctuation is the ordinary way to get one.
    #[test]
    fn a_label_never_starts_or_ends_with_a_hyphen() {
        assert_eq!(mdns_label("_lab_", "fallback"), "lab");
        assert_eq!(mdns_label(" spaced ", "fallback"), "spaced");
    }

    /// A Mac's default computer name can be entirely emoji, and a record still
    /// has to be published rather than silently malformed.
    #[test]
    fn a_name_with_nothing_usable_falls_back() {
        assert_eq!(mdns_label("💻", "cursio-node"), "cursio-node");
        assert_eq!(mdns_label("", "cursio-node"), "cursio-node");
        // Non-ASCII letters are dropped, not transliterated: guessing at a
        // romanisation would produce a name the user never chose.
        assert_eq!(mdns_label("Мак", "cursio-node"), "cursio-node");
    }

    /// DNS caps a label at 63 bytes and the service type is appended after it.
    #[test]
    fn a_long_name_is_cut_to_something_dns_will_carry() {
        let label = mdns_label(&"a".repeat(200), "fallback");
        assert_eq!(label.len(), MAX_LABEL_CHARS);
        assert!(label.len() < 63);
    }
}
