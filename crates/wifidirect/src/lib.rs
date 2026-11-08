//! A direct Wi-Fi link between two machines that both stay on their router.
//!
//! Two machines on one access point reach each other in two hops through it,
//! and a far router turns every frame into two slow ones. A second, private
//! link beside the infrastructure one on the same card — with its own small
//! subnet and no default route, so only the mesh rides it and each machine's
//! internet stays on its own router connection — measured 1.6 ms median and
//! no losses where the hotspot beside it gave 5.5 ms and lost one ping in five.
//!
//! ## Why a legacy group, not Wi-Fi Direct proper
//!
//! The obvious shape is P2P on both ends: NetworkManager has a `wifi-p2p`
//! device, Windows a Wi-Fi Direct API. They do not meet. Windows refuses
//! NetworkManager's group negotiation (`P2P-GO-NEG-FAILURE status=4`) whichever
//! side starts it and whether or not Windows already owns a group, because
//! NetworkManager never sends the provision discovery Windows waits for and
//! accepts no incoming request; Windows' own search never lists
//! NetworkManager's device at all. Reaching wpa_supplicant beneath it is
//! reserved to root. So the host side runs a Wi-Fi Direct group in its
//! *legacy* mode — an ordinary WPA2 network any station joins, with no
//! negotiation and no internet sharing behind it — and the joining side joins
//! it as a station, on a second station interface of the same card.
//!
//! That second interface is the one thing that needs root, once
//! (`cursio wifi install`): a card that allows two stations (MediaTek's MT7921:
//! `#{ managed, P2P-client } <= 2, #channels <= 2`) still only gets one until
//! someone creates it. After that NetworkManager drives it for the user.
//!
//! ## One channel
//!
//! A card serving two networks on two channels splits its time between them,
//! and measured worse than the router alone. The host's group takes the
//! channel of the host's own Wi-Fi, so the link is worth having only when the
//! joining machine's Wi-Fi is on that channel too; [`Ability::freq_mhz`] is
//! how the two compare.
//!
//! The module that decides who hosts and when is `cursio_core::direct_session`;
//! this crate only does what it is told on the machine it runs on. Every call
//! blocks — on NetworkManager over D-Bus, or on WinRT — so callers run them
//! off any runtime thread.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

use std::net::Ipv4Addr;

/// What can go wrong bringing a direct link up or down.
#[derive(Debug, thiserror::Error)]
pub enum DirectError {
    #[error("{0}")]
    Unsupported(String),
    #[error("NetworkManager: {0}")]
    NetworkManager(String),
    #[error("Wi-Fi Direct: {0}")]
    WiFiDirect(String),
    #[error("joining the direct group failed: {0}")]
    Join(String),
}

pub type Result<T> = std::result::Result<T, DirectError>;

/// What this machine can do toward a direct link right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ability {
    /// It can run a group another machine joins.
    pub host: bool,
    /// It can join a group while staying on its own network.
    pub join: bool,
    /// The frequency its own Wi-Fi is on, if it is on Wi-Fi.
    pub freq_mhz: Option<u32>,
    /// Why it can do neither, in words a person can act on.
    pub why_not: Option<String>,
}

/// Read what this machine can do now.
pub fn ability() -> Ability {
    #[cfg(target_os = "linux")]
    return linux::ability();
    #[cfg(windows)]
    return windows::ability();
    #[cfg(not(any(target_os = "linux", windows)))]
    Ability {
        why_not: Some("direct Wi-Fi links are not built for this platform".into()),
        ..Ability::default()
    }
}

/// A group this machine runs. Every holder shares one: a card runs one group,
/// and any number of machines may join it. It stops when the last is dropped.
pub struct HostedGroup {
    #[cfg(windows)]
    inner: std::sync::Arc<windows::Group>,
}

impl HostedGroup {
    /// The network name joining machines look for.
    pub fn ssid(&self) -> String {
        #[cfg(windows)]
        return self.inner.ssid.clone();
        #[cfg(not(windows))]
        unreachable!("no group is hosted on this platform")
    }

    pub fn passphrase(&self) -> String {
        #[cfg(windows)]
        return self.inner.passphrase.clone();
        #[cfg(not(windows))]
        unreachable!("no group is hosted on this platform")
    }

    /// Whether the system still runs the group.
    pub fn is_up(&self) -> bool {
        #[cfg(windows)]
        return self.inner.is_up();
        #[cfg(not(windows))]
        false
    }
}

/// Run a group, or share the one already running.
pub fn host() -> Result<HostedGroup> {
    #[cfg(windows)]
    return Ok(HostedGroup {
        inner: windows::host()?,
    });
    #[cfg(not(windows))]
    Err(DirectError::Unsupported(
        "this machine cannot run a direct Wi-Fi group yet; only Windows does".into(),
    ))
}

/// This machine on another's group. Leaves it when dropped; NetworkManager
/// also takes it down by itself if this process goes, since the connection
/// is bound to it.
pub struct Joined {
    /// This machine's address on the group.
    pub local: Ipv4Addr,
    /// The host's address on the group: where its mesh listener answers.
    pub host: Ipv4Addr,
    #[cfg(target_os = "linux")]
    inner: linux::Station,
}

impl Joined {
    /// Whether this machine is still on the group.
    pub fn is_up(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.inner.is_up();
        #[cfg(not(target_os = "linux"))]
        false
    }
}

/// Join the group `ssid`, beside this machine's own network.
pub fn join(ssid: &str, passphrase: &str) -> Result<Joined> {
    #[cfg(target_os = "linux")]
    {
        let (inner, local, host) = linux::join(ssid, passphrase)?;
        Ok(Joined { local, host, inner })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ssid, passphrase);
        Err(DirectError::Unsupported(
            "this machine cannot join a direct Wi-Fi group yet; only Linux does".into(),
        ))
    }
}

/// The centre frequency of Wi-Fi channel `channel`, in MHz.
pub fn channel_freq_mhz(channel: u32) -> Option<u32> {
    match channel {
        1..=13 => Some(2407 + 5 * channel),
        14 => Some(2484),
        32..=177 => Some(5000 + 5 * channel),
        _ => None,
    }
}

/// Whether two machines' Wi-Fi allow a direct link between them without the
/// card splitting its time across two channels: the same frequency, or one
/// of them not on Wi-Fi at all.
pub fn same_channel(a: Option<u32>, b: Option<u32>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// A fresh name and passphrase for a group: random each time, and sent to
/// the joining machine only over the mesh's own encrypted session.
pub fn fresh_credentials() -> (String, String) {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let mut pick = |n: usize| -> String {
        (0..n)
            .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
            .collect()
    };
    // "DIRECT-" is what Wi-Fi Direct groups are named, so a person scanning
    // sees what this is rather than a stranger's network.
    (format!("DIRECT-{}-Cursio", pick(2)), pick(20))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_map_to_their_centre_frequencies() {
        assert_eq!(channel_freq_mhz(1), Some(2412));
        assert_eq!(channel_freq_mhz(14), Some(2484));
        assert_eq!(channel_freq_mhz(36), Some(5180));
        assert_eq!(channel_freq_mhz(149), Some(5745));
        assert_eq!(channel_freq_mhz(0), None);
    }

    #[test]
    fn two_channels_rule_out_a_link_but_a_wired_end_does_not() {
        assert!(same_channel(Some(5745), Some(5745)));
        assert!(!same_channel(Some(5180), Some(5745)));
        assert!(same_channel(None, Some(5745)));
    }

    #[test]
    fn credentials_are_named_as_a_direct_group_and_differ_each_time() {
        let (ssid, pass) = fresh_credentials();
        assert!(ssid.starts_with("DIRECT-") && ssid.len() <= 32);
        assert!(pass.len() >= 8 && pass.len() <= 63);
        assert_ne!(fresh_credentials().1, pass);
    }
}
