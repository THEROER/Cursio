//! Tell the sessions the moment this machine's network goes, instead of
//! letting each path discover it by its own silence.
//!
//! NetworkManager already knows: a laptop's Wi-Fi deauthenticating (the
//! BCM4331 under b43 does it hourly) moves its state below "connected" at
//! once, while a session's path needs three seconds of silence to give up.
//! Those seconds are the input and sound the user feels stop, so the state
//! change ends the network paths at once
//! ([`cursio_transport::resume::network_lost`]), and the mesh dials them back
//! the moment it can.
//!
//! On its own thread with a blocking connection: zbus' blocking API on a
//! runtime thread is a deadlock waiting to happen.

/// `NM_STATE_CONNECTED_LOCAL`: below it this machine has no network at all.
#[cfg(any(target_os = "linux", test))]
const NM_STATE_CONNECTED_LOCAL: u32 = 50;

/// Start watching NetworkManager, if it runs here.
#[cfg(target_os = "linux")]
pub fn start() {
    let spawned = std::thread::Builder::new()
        .name("cursio-network-watch".into())
        .spawn(|| {
            if let Err(e) = watch() {
                tracing::info!(error = %e, "not watching NetworkManager; a dropped network is noticed by the sessions' own checks");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the network watcher");
    }
}

#[cfg(target_os = "linux")]
fn watch() -> zbus::Result<()> {
    let bus = zbus::blocking::Connection::system()?;
    let manager = zbus::blocking::Proxy::new(
        &bus,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )?;
    let mut previous: u32 = manager.get_property("State")?;
    tracing::debug!(state = previous, "watching NetworkManager");
    for signal in manager.receive_signal("StateChanged")? {
        let Ok(state) = signal.body().deserialize::<u32>() else {
            continue;
        };
        if lost(previous, state) {
            tracing::info!(
                state,
                "NetworkManager reports the network gone; leaving the network paths now"
            );
            cursio_transport::resume::network_lost();
        }
        previous = state;
    }
    Ok(())
}

/// The network was there and is not any more.
#[cfg(any(target_os = "linux", test))]
fn lost(previous: u32, now: u32) -> bool {
    previous >= NM_STATE_CONNECTED_LOCAL && now < NM_STATE_CONNECTED_LOCAL
}

#[cfg(not(target_os = "linux"))]
pub fn start() {
    tracing::debug!("no network watcher on this platform; a dropped network is noticed by the sessions' own checks");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fall_below_connected_counts_as_the_network_going() {
        // NM_STATE_CONNECTED_GLOBAL to DISCONNECTING, then DISCONNECTED.
        assert!(lost(70, 30));
        assert!(!lost(30, 20), "already gone is not newly lost");
        assert!(!lost(60, 70), "gaining connectivity is not losing it");
        assert!(!lost(70, 50), "local-only still reaches the LAN");
    }
}
