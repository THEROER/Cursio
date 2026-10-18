//! This phone reaching its computers over Bluetooth when the network does not.
//!
//! The link is the mesh's own: an RFCOMM socket to the computer's Cursio
//! service, with the same handshake and Noise session as over Wi-Fi (see
//! `cursio_core::bluetooth_link`). Android hands an app a Bluetooth socket
//! only as Java streams, so `BtLink.connect` pumps it into one end of a socket
//! pair made here and the node reads the other end like any socket.
//!
//! The phone dials and the computer listens, because Android keeps the
//! phone's own address from apps. [`Dialer`] decides when: only computers the
//! phone is paired with, only once the network has failed to reach a known
//! computer for [`NETWORK_GRACE`], and not more often than [`RETRY`] per
//! device, since each attempt is a few seconds of radio time.

use std::collections::{HashMap, HashSet};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::time::{Duration, Instant};

use jni::objects::{JObject, JString, JValue};

/// How long a known computer must have been unreachable over the network
/// before Bluetooth is tried. Long enough that a reconnect after the Wi-Fi
/// blinks happens on Wi-Fi, short enough that walking out of range is felt
/// as a pause rather than as the phone vanishing.
pub const NETWORK_GRACE: Duration = Duration::from_secs(20);

/// The least time between two attempts at the same device.
pub const RETRY: Duration = Duration::from_secs(30);

/// A computer this phone is paired with over Bluetooth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub address: String,
    pub name: String,
}

/// When to open a Bluetooth link, kept apart from the radio so it is tested.
#[derive(Default)]
pub struct Dialer {
    tried: HashMap<String, Instant>,
    linked: HashSet<String>,
}

impl Dialer {
    /// The devices to try now. `unreachable_for` is how long the longest-gone
    /// known computer has been off the network, `None` while all are on it.
    pub fn due(
        &mut self,
        paired: &[Paired],
        unreachable_for: Option<Duration>,
        now: Instant,
    ) -> Vec<Paired> {
        if !unreachable_for.is_some_and(|gone| gone >= NETWORK_GRACE) {
            return Vec::new();
        }
        let due: Vec<Paired> = paired
            .iter()
            .filter(|device| !self.linked.contains(&device.address))
            .filter(|device| {
                self.tried
                    .get(&device.address)
                    .is_none_or(|at| now.duration_since(*at) >= RETRY)
            })
            .cloned()
            .collect();
        for device in &due {
            self.tried.insert(device.address.clone(), now);
        }
        due
    }

    /// A link to `address` is up, or has ended.
    pub fn set_linked(&mut self, address: &str, up: bool) {
        if up {
            self.linked.insert(address.to_string());
        } else {
            self.linked.remove(address);
        }
    }
}

/// The computers this phone is paired with, or none without the permission
/// or an adapter. Asked of the app each time: pairing happens in the system's
/// settings, and nothing tells the node about it.
pub fn paired_computers() -> Vec<Paired> {
    let listed = crate::jvm::with_native(|env, class| {
        let result = env.call_static_method(class, "btComputers", "()Ljava/lang/String;", &[])?;
        let text: JString = result.l()?.into();
        let text: String = env.get_string(&text)?.into();
        Ok(text)
    });
    match listed {
        Ok(text) => parse_paired(&text),
        Err(e) => {
            tracing::debug!(error = %e, "cannot list paired computers");
            Vec::new()
        }
    }
}

fn parse_paired(text: &str) -> Vec<Paired> {
    text.lines()
        .filter_map(|line| {
            let (address, name) = line.split_once('\t')?;
            Some(Paired {
                address: address.to_string(),
                name: name.to_string(),
            })
        })
        .collect()
}

/// Open the mesh's channel on `address`; blocks for as long as Android takes
/// to connect (seconds), so call it off the node's thread. The answer is this
/// end of the socket pair the app pumps the RFCOMM stream through.
pub fn connect(address: &str) -> Result<OwnedFd, String> {
    let (ours, theirs) = std::os::unix::net::UnixStream::pair()
        .map_err(|e| format!("cannot make a socket pair: {e}"))?;
    // Ownership of `theirs` passes to the app, which adopts the descriptor
    // and closes it when the Bluetooth stream ends.
    let theirs = theirs.into_raw_fd();
    let refused = crate::jvm::with_native(|env, class| {
        let address = JObject::from(env.new_string(address)?);
        let result = env.call_static_method(
            class,
            "btConnect",
            "(Ljava/lang/String;I)Ljava/lang/String;",
            &[JValue::Object(&address), JValue::Int(theirs)],
        )?;
        let refusal = result.l()?;
        if refusal.is_null() {
            return Ok(None);
        }
        let refusal: JString = refusal.into();
        let refusal: String = env.get_string(&refusal)?.into();
        Ok(Some(refusal))
    });
    let refused = match refused {
        Ok(refused) => refused,
        Err(e) => {
            // The app never got as far as adopting it.
            // SAFETY: `theirs` came from `into_raw_fd` above and nothing else
            // took ownership of it.
            drop(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(theirs) });
            return Err(e);
        }
    };
    match refused {
        None => Ok(OwnedFd::from(ours)),
        Some(why) => Err(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc(address: &str) -> Paired {
        Paired {
            address: address.into(),
            name: "desk".into(),
        }
    }

    #[test]
    fn leaves_bluetooth_alone_while_the_network_reaches_everyone() {
        let mut dialer = Dialer::default();
        assert!(dialer.due(&[pc("A")], None, Instant::now()).is_empty());
    }

    #[test]
    fn waits_out_a_short_network_blip_before_trying_bluetooth() {
        let mut dialer = Dialer::default();
        let now = Instant::now();
        assert!(dialer
            .due(&[pc("A")], Some(Duration::from_secs(5)), now)
            .is_empty());
        assert_eq!(
            dialer.due(&[pc("A")], Some(NETWORK_GRACE), now),
            vec![pc("A")]
        );
    }

    #[test]
    fn does_not_retry_a_device_sooner_than_the_retry_interval() {
        let mut dialer = Dialer::default();
        let now = Instant::now();
        let gone = Some(NETWORK_GRACE);
        assert_eq!(dialer.due(&[pc("A")], gone, now).len(), 1);
        assert!(dialer
            .due(&[pc("A")], gone, now + Duration::from_secs(10))
            .is_empty());
        assert_eq!(dialer.due(&[pc("A")], gone, now + RETRY).len(), 1);
    }

    #[test]
    fn does_not_dial_a_device_it_already_has_a_link_to() {
        let mut dialer = Dialer::default();
        dialer.set_linked("A", true);
        let due = dialer.due(&[pc("A"), pc("B")], Some(NETWORK_GRACE), Instant::now());
        assert_eq!(due, vec![pc("B")]);
    }

    #[test]
    fn reads_the_apps_list_of_paired_computers() {
        assert_eq!(
            parse_paired("AA:BB\tNocturne\nCC:DD\tdark\n"),
            vec![
                Paired {
                    address: "AA:BB".into(),
                    name: "Nocturne".into()
                },
                Paired {
                    address: "CC:DD".into(),
                    name: "dark".into()
                },
            ]
        );
        assert!(parse_paired("").is_empty());
    }
}
