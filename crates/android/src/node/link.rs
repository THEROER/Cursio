//! One link to a computer: from the greeting through pairing to the session,
//! and the bookkeeping that hands the peer back when it ends.

use crate::node::lock;
use crate::node::serve::publish;
use crate::node::NodeCtx;
use crate::node::PHONE_POSITION;
use cursio_core::control::LinkView;
use cursio_core::registry::PeerState;
use cursio_protocol::{PeerId, ServiceOffer};
use cursio_transport::pairing::Role;
use cursio_transport::Transport;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;

pub(super) fn spawn_session<T: Transport + 'static>(
    transport: T,
    ctx: Rc<NodeCtx>,
    role: Role,
    expected: Option<PeerId>,
    addr: Option<SocketAddr>,
) {
    tokio::task::spawn_local(async move {
        if let Some(id) = expected {
            ctx.registry.borrow_mut().set_state(id, PeerState::Online);
        }
        let who = std::cell::Cell::new(expected);
        let hold = LinkHold::default();
        let result = tokio::select! {
            result = run_link(transport, &ctx, role, addr, &who, &hold) => result,
            () = hold.superseded.notified() => {
                tracing::info!("a better link to this computer came up; closing this one");
                Ok(())
            }
        };
        match &result {
            Ok(()) => tracing::info!("peer session ended"),
            Err(e) => tracing::warn!(error = %e, "peer session ended with error"),
        }
        end_link(&ctx, who.get(), &hold);
        publish(&ctx);
    });
}

/// A session's hold on its peer's one link; see
/// [`PeerRegistry::claim_link`](cursio_core::registry::PeerRegistry::claim_link).
#[derive(Default)]
pub(super) struct LinkHold {
    token: std::cell::Cell<Option<u64>>,
    /// Woken when a better link to the same computer replaces this one.
    pub(super) superseded: Rc<tokio::sync::Notify>,
    /// This connection joined a running session, which now owns it.
    joined: std::cell::Cell<bool>,
    /// The session this connection started, ended with it.
    pub(super) session: RefCell<Option<cursio_transport::resume::ResumeHandle>>,
}

/// Take the computer offline after a session to it ended — unless another
/// link to it is up, which is then its link.
pub(super) fn end_link(ctx: &NodeCtx, who: Option<PeerId>, hold: &LinkHold) {
    if hold.joined.get() {
        return;
    }
    let Some(id) = who else { return };
    if let Some(session) = hold.session.borrow_mut().take() {
        session.end("the session ended");
        ctx.registry.borrow_mut().session_ended(id, session.tag());
    }
    if ctx.registry.borrow_mut().release_link(id, hold.token.get()) {
        ctx.registry.borrow_mut().mark_offline(id);
        ctx.control.link_down(&id);
    }
}

/// Greet, pair if this is a first meeting, secure, and run the session.
pub(super) async fn run_link<T: Transport + 'static>(
    transport: T,
    ctx: &NodeCtx,
    role: Role,
    // `None` over Bluetooth, which has no address the network could dial.
    addr: Option<SocketAddr>,
    who: &std::cell::Cell<Option<PeerId>>,
    hold: &LinkHold,
) -> anyhow::Result<()> {
    let password = ctx.pairing.password();
    let plan = cursio_core::link::Handshake {
        identity: &ctx.identity,
        password: &password,
        role,
        registry: &ctx.modules,
        position: PHONE_POSITION,
        service: ServiceOffer::none(),
        status: ctx.status.borrow().current(),
        // The touchpad: a phone has no capture, but its touch surface is input
        // for whichever peer it is pointed at.
        sends_input: true,
        resumable: ctx.registry.borrow().resumable_tags(),
    };
    let path = cursio_core::control::LinkPath::of(transport.kind());
    let identified = cursio_core::link::identify(transport, &plan).await?;
    let peer_id = identified.peer.peer_id;
    let peer_name = identified.peer.display_name.clone();
    let negotiated = identified.negotiated().clone();
    who.set(Some(peer_id));
    let greeted =
        ctx.registry
            .borrow_mut()
            .greeted(peer_id, &identified.peer.resumable, &plan.resumable);
    if let cursio_core::registry::Greeted::Join(session) = greeted {
        hold.joined.set(true);
        ctx.registry
            .borrow_mut()
            .set_state(peer_id, PeerState::Online);
        tracing::info!(peer = %peer_name, "the computer called back; resuming the session");
        return cursio_core::link::join(identified, &plan, &session).await;
    }
    let shape = cursio_core::registry::LinkShape {
        dialled: matches!(role, Role::Initiator),
        path,
    };
    let claimed = ctx
        .registry
        .borrow_mut()
        .claim_link(peer_id, shape, hold.superseded.clone());
    match claimed {
        Some(token) => hold.token.set(Some(token)),
        None => {
            tracing::info!(peer = %peer_name, ?shape, "already linked to this computer the way both sides keep; closing this second link");
            return Ok(());
        }
    }
    if ctx.registry.borrow().is_known(&peer_id) {
        ctx.registry
            .borrow_mut()
            .set_state(peer_id, PeerState::Online);
        tracing::info!(peer = %peer_name, "peer online");
    } else if ctx.pairing.is_open() {
        tracing::info!(peer = %peer_name, "unknown machine; pairing it");
    } else {
        tracing::warn!(peer = %peer_name, "untrusted machine; not admitted");
    }

    let (link, paired_now) = ctx.pairing.secure(identified, &plan).await?;
    if paired_now {
        // The desktop waits for the next restart or for the peer to call; a
        // phone the user just paired should show and dial the machine at once.
        ctx.registry
            .borrow_mut()
            .admit(peer_id, peer_name.clone(), addr);
        ctx.registry
            .borrow_mut()
            .set_state(peer_id, PeerState::Online);
        tracing::info!(peer = %peer_name, "paired");
    }
    ctx.registry
        .borrow_mut()
        .hold_session(peer_id, link.resume.clone());
    *hold.session.borrow_mut() = Some(link.resume.clone());
    let peer = link.peer.clone();
    let (skipped, inapplicable) = cursio_core::control::link_skips(&negotiated, peer.device_kind);
    ctx.control.link_up(
        peer.peer_id,
        &peer.display_name,
        LinkView {
            position: peer.position,
            device_kind: peer.device_kind,
            status: peer.status,
            modules: negotiated.agreed.iter().map(|id| id.to_string()).collect(),
            skipped,
            inapplicable,
            since: std::time::SystemTime::now(),
            path,
            path_since: None,
        },
    );
    publish(ctx);

    // A phone never takes the cursor, so `CursorSetup` leaves this side
    // dormant for the life of the link, and a dormant session forwards its
    // capture channel to the peer: the touchpad is just a writer on it.
    //
    // Offered to the touchpad only when the peer has a cursor to move. Another
    // phone has none — it injects nothing — so listing it made a target whose
    // strokes went nowhere.
    let (input_tx, mut input_rx) = cursio_core::session::input_channel();
    if link.cursor.peer_takes_cursor {
        lock(&ctx.shared.inputs).insert(peer.peer_id, (peer.display_name.clone(), input_tx));
    }
    let mut status_cursor = ctx.status.borrow().cursor();
    let result = cursio_core::session::run_session_live(
        link.transport,
        link.cursor,
        Some(&mut input_rx),
        |_event| {},
        None,
        |_ownership| {},
        link.modules,
        || ctx.status.borrow_mut().poll(&mut status_cursor),
        None,
        Some(peer.peer_id),
    )
    .await;
    lock(&ctx.shared.inputs).remove(&peer.peer_id);
    match result {
        Ok(()) | Err(cursio_core::session::SessionError::PeerLeft) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
