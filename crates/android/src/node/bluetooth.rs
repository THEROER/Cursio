//! The Bluetooth standby beside the network path: dialled to each paired
//! computer, and let go once the network carries the session again.

use crate::node::link::end_link;
use crate::node::link::run_link;
use crate::node::link::LinkHold;
use crate::node::serve::publish;
use crate::node::NodeCtx;
use cursio_core::registry::PeerState;
use cursio_protocol::PeerId;
use cursio_transport::pairing::Role;
use cursio_transport::Transport;
use std::rc::Rc;

/// Open Bluetooth links to paired computers the network has not reached for
/// a while; see [`crate::bt`].
pub(super) async fn dial_bluetooth(ctx: &Rc<NodeCtx>) {
    // A phone's radio clashes with nothing the core knows of, so only the
    // person's own "off" holds it back.
    if ctx.control.running().bluetooth.mesh == cursio_core::config::BluetoothMesh::Off {
        return;
    }
    let now = std::time::Instant::now();
    let offline: Vec<PeerId> = ctx
        .registry
        .borrow()
        .all()
        .filter(|peer| !matches!(peer.state, PeerState::Online | PeerState::Connecting))
        .map(|peer| peer.peer_id)
        .collect();
    let gone_for = {
        let mut since = ctx.unreachable_since.borrow_mut();
        since.retain(|id, _| offline.contains(id));
        for id in &offline {
            since.entry(*id).or_insert(now);
        }
        since.values().map(|at| now.duration_since(*at)).max()
    };
    let standby = ctx.standby() && ctx.registry.borrow().wants_bluetooth_standby();
    if gone_for.is_none() && !standby {
        return;
    }
    let paired = tokio::task::spawn_blocking(crate::bt::paired_computers)
        .await
        .unwrap_or_default();
    let due = ctx
        .bluetooth
        .borrow_mut()
        .due(&paired, gone_for, standby, now);
    for device in due {
        let address = device.address.clone();
        let opened = tokio::task::spawn_blocking(move || crate::bt::connect(&address)).await;
        let fd = match opened {
            Ok(Ok(fd)) => fd,
            Ok(Err(why)) => {
                tracing::debug!(device = %device.name, %why, "no mesh over bluetooth to this device");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "the bluetooth dial did not finish");
                continue;
            }
        };
        let transport = match cursio_core::bluetooth_link::transport(fd) {
            Ok(transport) => transport,
            Err(e) => {
                tracing::warn!(error = %e, "cannot use the bluetooth link");
                continue;
            }
        };
        tracing::info!(device = %device.name, "reached a computer over bluetooth");
        ctx.bluetooth.borrow_mut().set_linked(&device.address, true);
        spawn_bluetooth_session(transport, Rc::clone(ctx), device.address);
    }
}

/// A session over Bluetooth that ends itself once the network reaches the
/// same computer again, so the ordinary dial takes over at network speed.
pub(super) fn spawn_bluetooth_session<T: Transport + 'static>(
    transport: T,
    ctx: Rc<NodeCtx>,
    address: String,
) {
    tokio::task::spawn_local(async move {
        let who = std::cell::Cell::new(None);
        let hold = LinkHold::default();
        // The session this connection starts moves to the network when the
        // computer calls back there; the Bluetooth path is then let go of,
        // not the session.
        let ended = tokio::select! {
            result = run_link(transport, &ctx, Role::Initiator, None, &who, &hold) => result,
            () = leave_bluetooth_once_the_network_carries(&hold, &address, &ctx) => Ok(()),
            () = hold.superseded.notified() => {
                tracing::info!("the network link to this computer is up; leaving bluetooth");
                Ok(())
            }
        };
        if let Err(e) = &ended {
            tracing::warn!(error = %e, "bluetooth session ended with error");
        }
        ctx.bluetooth.borrow_mut().set_linked(&address, false);
        end_link(&ctx, who.get(), &hold);
        publish(&ctx);
    });
}

/// Let the Bluetooth path go once a network path carries the session, which
/// happens when either side dials over the network again and the connection
/// joins it. Never resolves: the session runs on in `run_link`. Kept as a
/// standby instead when the user asked for one.
pub(super) async fn leave_bluetooth_once_the_network_carries(
    hold: &LinkHold,
    address: &str,
    ctx: &NodeCtx,
) {
    let mut released = false;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if released {
            continue;
        }
        let Some(session) = hold.session.borrow().clone() else {
            continue;
        };
        let paths = session.paths();
        if paths.network && paths.bluetooth && !ctx.standby() {
            tracing::info!("the network carries the session again; leaving bluetooth");
            session.release(
                cursio_transport::resume::PathKind::Bluetooth,
                "the network carries the session",
            );
            ctx.bluetooth.borrow_mut().set_linked(address, false);
            released = true;
        }
    }
}
