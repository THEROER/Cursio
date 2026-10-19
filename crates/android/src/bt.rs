//! This phone reaching its computers over Bluetooth when the network does not.
//!
//! The link is the mesh's own: an RFCOMM socket to the computer's Cursio
//! service, with the same handshake and Noise session as over Wi-Fi (see
//! `cursio_core::bluetooth_link`). Android hands an app a Bluetooth socket
//! only as Java streams, so `BtLink.connect` pumps it into one end of a socket
//! pair made here and the node reads the other end like any socket.
//!
//! The phone dials and the computer listens, because Android keeps the
//! phone's own address from apps. When to dial is the core's
//! [`cursio_core::bluetooth_link::Dialer`], shared with the computers'.

use std::os::fd::{IntoRawFd, OwnedFd};

use cursio_core::bluetooth_link::Paired;
use jni::objects::{JObject, JString, JValue};

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
