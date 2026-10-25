//! Waking a paired machine that is asleep: Wake-on-LAN.
//!
//! A sleeping machine's network card listens for one frame, the "magic
//! packet" — six `0xff` bytes and its own hardware address sixteen times — and
//! nothing else, so the one thing this needs is that address. It is never
//! asked of the user, and a machine cannot tell it once it is asleep, so it is
//! learned while the machine is awake: a link over the network has just
//! exchanged packets with it, and this machine's own neighbour table (ARP)
//! holds the address its card answered from. That goes into the trust store
//! beside the peer's last address (`TrustedPeer::mac`), and `cursio wake` or
//! the app reads it back.
//!
//! The packet goes to the limited broadcast address on the discard and echo
//! ports (9 and 7), which every Wake-on-LAN listener honours. A sleeping
//! machine has no IP address anyone can reach, and a router never forwards a
//! limited broadcast, so this wakes machines on this LAN only — which is all
//! the mesh is. Whether the card listens at all is the sleeping machine's own
//! setting (firmware and driver); nothing on the wire can confirm a machine
//! woke, so success here means the packet left, and the link coming up is the
//! answer.
//!
//! The neighbour table is read where the platform keeps one an unprivileged
//! process can read: `/proc/net/arp` on Linux, `SendARP` on Windows (which
//! also resolves an address not yet cached), `arp -n` on macOS. An Android app
//! may not read it since Android 10, so a phone learns nothing and wakes only
//! the machines it is told about.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use cursio_protocol::PeerId;
use cursio_transport::pairing::TrustStore;

#[derive(Debug, thiserror::Error)]
pub enum WakeError {
    #[error("cannot read the trust store: {0}")]
    Store(String),
    #[error("no paired machine is called {0:?}; `cursio peers` and `cursio status` list them")]
    Unknown(String),
    #[error(
        "{0} has not been seen on the network yet; Cursio learns a machine's hardware address \
         the first time it connects over the network, so connect it once while it is awake"
    )]
    NoAddress(String),
    #[error("{name}'s stored hardware address {mac:?} is not one")]
    BadAddress { name: String, mac: String },
    #[error("cannot send the wake packet: {0}")]
    Send(#[from] std::io::Error),
}

/// A hardware address as `aa:bb:cc:dd:ee:ff`, lowercase.
fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Parse `aa:bb:cc:dd:ee:ff` or `aa-bb-…`, either case.
fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split([':', '-']);
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// The frame a sleeping card wakes for.
fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xffu8; 102];
    for chunk in packet[6..].as_chunks_mut::<6>().0 {
        *chunk = mac;
    }
    packet
}

/// The hardware address `ip` answers from, from `/proc/net/arp`'s text.
///
/// Flags `0x0` is an entry still being resolved, or one that failed, whose
/// address column reads all zeros; neither is an answer.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn from_proc_arp(table: &str, ip: Ipv4Addr) -> Option<[u8; 6]> {
    table.lines().skip(1).find_map(|line| {
        let mut columns = line.split_whitespace();
        let address: Ipv4Addr = columns.next()?.parse().ok()?;
        let _hw_type = columns.next()?;
        let flags = columns.next()?;
        let mac = parse_mac(columns.next()?)?;
        (address == ip && flags != "0x0" && mac != [0; 6]).then_some(mac)
    })
}

/// The hardware address `ip` answers from, by this machine's neighbour table.
#[cfg(target_os = "linux")]
fn mac_of(ip: Ipv4Addr) -> Option<[u8; 6]> {
    from_proc_arp(&std::fs::read_to_string("/proc/net/arp").ok()?, ip)
}

#[cfg(target_os = "windows")]
fn mac_of(ip: Ipv4Addr) -> Option<[u8; 6]> {
    use windows::Win32::NetworkManagement::IpHelper::SendARP;
    let mut mac = [0u8; 8];
    let mut len = mac.len() as u32;
    // SendARP takes the address in network byte order, as it sits in memory.
    let dest = u32::from_ne_bytes(ip.octets());
    // SAFETY: `mac` holds `len` bytes, which is what the call may write.
    let status = unsafe { SendARP(dest, 0, mac.as_mut_ptr().cast(), &mut len) };
    (status == 0 && len == 6).then(|| [mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]])
}

#[cfg(target_os = "macos")]
fn mac_of(ip: Ipv4Addr) -> Option<[u8; 6]> {
    // `? (192.168.1.5) at aa:bb:cc:dd:ee:ff on en0 ifscope [ethernet]`, with
    // single hex digits where a byte is below 0x10.
    let out = std::process::Command::new("/usr/sbin/arp")
        .args(["-n", &ip.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let after = text.split(" at ").nth(1)?;
    parse_mac(after.split_whitespace().next()?)
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn mac_of(_ip: Ipv4Addr) -> Option<[u8; 6]> {
    None
}

/// Remember the hardware address `peer` answers from at `ip`, after a link
/// over the network came up. Blocking (a file, an ARP round trip on Windows),
/// so run off the session's thread. Best-effort: a machine that cannot be
/// woken later is no reason to disturb the link now.
pub fn learn(peer: PeerId, ip: IpAddr) {
    let IpAddr::V4(ip) = ip else {
        // Wake-on-LAN is a broadcast frame; a link over IPv6 has no ARP entry
        // to read, and an IPv4 one will come along the next time.
        return;
    };
    let Some(mac) = mac_of(ip) else {
        tracing::debug!(%ip, "no neighbour-table entry for a peer; its hardware address stays unknown");
        return;
    };
    let mac = format_mac(mac);
    match TrustStore::load() {
        Ok(mut store) => {
            if let Err(error) = store.remember_mac(&peer, &mac) {
                tracing::debug!(%error, "could not remember a peer's hardware address");
            }
        }
        Err(error) => {
            tracing::debug!(%error, "could not read the trust store to remember a hardware address")
        }
    }
}

/// Send the wake packet to the paired machine named `machine` (its name or
/// id). Returns the name it was sent to.
pub fn wake(machine: &str) -> Result<String, WakeError> {
    let store = TrustStore::load().map_err(|e| WakeError::Store(e.to_string()))?;
    let peer = store
        .peers()
        .find(|p| p.display_name.eq_ignore_ascii_case(machine) || p.peer_id.to_string() == machine)
        .ok_or_else(|| WakeError::Unknown(machine.to_owned()))?;
    let name = peer.display_name.clone();
    let text = peer
        .mac
        .clone()
        .ok_or_else(|| WakeError::NoAddress(name.clone()))?;
    let mac = parse_mac(&text).ok_or_else(|| WakeError::BadAddress {
        name: name.clone(),
        mac: text.clone(),
    })?;
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    for port in [9, 7] {
        socket.send_to(&packet, SocketAddr::from((Ipv4Addr::BROADCAST, port)))?;
    }
    tracing::info!(machine = %name, mac = %text, "sent a wake-on-lan packet");
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARP: &str = "\
IP address       HW type     Flags       HW address            Mask     Device
192.168.1.5      0x1         0x2         a8:5e:45:01:02:0c     *        wlp3s0
192.168.1.9      0x1         0x0         00:00:00:00:00:00     *        wlp3s0
";

    #[test]
    fn a_peers_hardware_address_is_read_from_the_neighbour_table() {
        assert_eq!(
            from_proc_arp(ARP, Ipv4Addr::new(192, 168, 1, 5)),
            Some([0xa8, 0x5e, 0x45, 0x01, 0x02, 0x0c])
        );
    }

    #[test]
    fn an_unresolved_or_absent_entry_is_no_address() {
        assert_eq!(from_proc_arp(ARP, Ipv4Addr::new(192, 168, 1, 9)), None);
        assert_eq!(from_proc_arp(ARP, Ipv4Addr::new(10, 0, 0, 1)), None);
    }

    #[test]
    fn the_magic_packet_is_six_ff_then_the_address_sixteen_times() {
        let mac = [1, 2, 3, 4, 5, 6];
        let packet = magic_packet(mac);
        assert_eq!(packet[..6], [0xff; 6]);
        assert!(packet[6..].chunks(6).all(|c| c == mac));
        assert_eq!(packet[6..].chunks(6).count(), 16);
    }

    #[test]
    fn addresses_read_in_any_spelling_and_are_stored_in_one() {
        assert_eq!(
            parse_mac("A8-5E-45-1-2-C"),
            Some([0xa8, 0x5e, 0x45, 1, 2, 0xc])
        );
        assert_eq!(
            format_mac([0xa8, 0x5e, 0x45, 1, 2, 0xc]),
            "a8:5e:45:01:02:0c"
        );
        assert_eq!(parse_mac("a8:5e:45:01:02"), None);
        assert_eq!(parse_mac("a8:5e:45:01:02:0c:ff"), None);
    }
}
