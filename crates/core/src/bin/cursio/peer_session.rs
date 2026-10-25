//! One connection to one peer, from the first byte to the session ending:
//! identify, pair or recognise, secure, and run the session on it.

use crate::input::inject;
use crate::input::make_injector;
use crate::input::set_cursor_hidden;
use crate::input::set_injecting;
use crate::input::HubLink;
use crate::input::PersistentCapture;
use crate::input::SharedCapture;
use crate::mesh::SharedNodeStatus;
use cursio_core::input_hub::Placement;
use cursio_core::pairing_window::PairingWindow;
use cursio_core::session::{self};
use cursio_core::NodeIdentity;
use cursio_protocol::ServiceOffer;
use cursio_transport::pairing::{Role, TrustStore};
use cursio_transport::Transport;
use std::time::Duration;

/// Run one peer session on the local task set, keeping the registry's view of
/// that peer up to date for its whole life.
///
/// `expected` is the peer's id when we dialled it; `None` for an inbound
/// connection, where the identity is only known after the handshake. Either way
/// the peer ends up `Offline` when the session finishes, so the next sweep can
/// reconnect it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_peer_session<T: cursio_transport::Transport + 'static>(
    transport: T,
    identity: NodeIdentity,
    role: Role,
    expected: Option<cursio_protocol::PeerId>,
    registry: cursio_core::mesh::SharedRegistry,
    // Only consulted for an inbound connection: a peer we dialled is one we
    // already trust, so it never needs a password.
    pairing: PairingWindow,
    control: cursio_core::control::NodeState,
    // Shared with every other mesh session: capture is one machine-wide
    // resource (one portal permission, one grab), so sessions all attach to
    // it rather than each building their own. See [`PersistentCapture`].
    capture: SharedCapture,
    // Machine-wide too, and for the same reason: every link greets with the
    // same reading, and every link should announce the same change.
    status: SharedNodeStatus,
    // What the user has this node set to, followed by the link while it runs:
    // where the machine stands, which modules and whether input is on.
    live: cursio_core::live::LiveConfig,
    // Shared rather than cloned: the registry is built once per process and read
    // by every session. `Rc` because sessions are deliberately single-threaded
    // (see the mesh module's threading note).
    modules: std::rc::Rc<cursio_core::module::ModuleRegistry>,
    offer: ServiceOffer,
    // The device's address for a Bluetooth connection, so the peer that
    // greets on it is remembered as that radio's.
    bluetooth: Option<String>,
    // The peer's address for a network connection, whose neighbour-table
    // entry gives the hardware address a wake-up is sent to.
    network: Option<std::net::IpAddr>,
) {
    let shape = cursio_core::registry::LinkShape {
        dialled: matches!(role, Role::Initiator),
        path: cursio_core::control::LinkPath::of(transport.kind()),
    };
    tokio::task::spawn_local(async move {
        if let Some(id) = expected {
            registry
                .borrow_mut()
                .set_state(id, cursio_core::registry::PeerState::Online);
        }

        // Who we're actually talking to. For an inbound connection this stays
        // unknown until the handshake fires the callback below, so we track it
        // here and use it for the cleanup afterwards.
        let identified = std::rc::Rc::new(std::cell::RefCell::new(expected));
        let seen = identified.clone();
        let claimed = std::rc::Rc::new(std::cell::Cell::new(None));
        let claimed_for_cb = claimed.clone();
        let superseded = std::rc::Rc::new(tokio::sync::Notify::new());
        let superseded_for_cb = superseded.clone();
        let reg_for_cb = registry.clone();
        let pairing_for_cb = pairing.clone();
        let live_for_cb = live.clone();
        // A connection that joined a running session hands over to it and
        // leaves nothing of its own to clean up.
        let joined = std::rc::Rc::new(std::cell::Cell::new(false));
        let joined_for_cb = joined.clone();
        let resumable = registry.borrow().resumable_tags();
        let offered_tags = resumable.clone();
        let mut on_identified =
            move |id: cursio_protocol::PeerId,
                  name: &str,
                  peer_tags: &[cursio_protocol::SessionTag]| {
                *seen.borrow_mut() = Some(id);
                if let Some(ip) = network {
                    tokio::task::spawn_blocking(move || cursio_core::wake::learn(id, ip));
                }
                let mut reg = reg_for_cb.borrow_mut();
                if let Some(address) = &bluetooth {
                    // Kept past a restart, like the network address after a
                    // dial: best-effort, a failed write must not stop the link.
                    if reg.note_bluetooth(address, id, shape.dialled) {
                        if let Ok(mut store) = TrustStore::load() {
                            if let Err(e) = store.remember_bluetooth(&id, address) {
                                tracing::debug!(error = %e, "could not remember the peer's bluetooth address");
                            }
                        }
                    }
                }
                if let cursio_core::registry::Greeted::Join(session) =
                    reg.greeted(id, peer_tags, &offered_tags)
                {
                    reg.set_state(id, cursio_core::registry::PeerState::Online);
                    tracing::info!(peer = %name, "the peer called back; resuming the session");
                    joined_for_cb.set(true);
                    return cursio_core::link::Admit::Join(session);
                }
                let Some(token) = reg.claim_link(id, shape, superseded_for_cb.clone()) else {
                    tracing::info!(peer = %name, ?shape, "already linked to this peer the way both sides keep; closing this second link");
                    return cursio_core::link::Admit::Refuse;
                };
                claimed_for_cb.set(Some(token));
                // An inbound peer we already trust is simply now online. One we
                // don't know is refused by the key lookup right after this, so it
                // never becomes a mesh member here.
                if reg.is_known(&id) {
                    reg.set_state(id, cursio_core::registry::PeerState::Online);
                    tracing::info!(peer = %name, "peer online");
                    cursio_core::health::link_restored(id);
                } else if pairing_for_cb.is_open() {
                    tracing::info!(peer = %name, %id, "unknown machine dialled in; pairing it");
                } else if live_for_cb.is_forgotten(&id) {
                    // It still trusts us and calls every sweep; one line says why
                    // it gets nowhere, the rest would only bury the log.
                    if cursio_core::forget::note_refused_call(id) {
                        tracing::info!(peer = %name, "a machine removed from this mesh keeps calling; refusing it until it is paired again");
                    }
                } else {
                    tracing::warn!(peer = %name, %id, "untrusted peer dialled in; not admitted");
                }
                cursio_core::link::Admit::Session
            };

        // Whichever way the call went, the password only matters on a first
        // meeting: `link::establish` uses the trust store when it has an entry
        // and reaches for this when it does not. Both roles get it because
        // either can be the one that dialled a stranger — the mesh calls a
        // machine it has never met while `--pair` is open, and used to leave
        // this empty on the assumption that a dialled peer is always trusted.
        let held = std::rc::Rc::new(std::cell::RefCell::new(None));
        let held_for_cb = held.clone();
        let reg_for_secured = registry.clone();
        let identified_for_secured = identified.clone();
        let control_for_secured = control.clone();
        let mut on_secured = move |resume: &cursio_transport::resume::ResumeHandle| {
            *held_for_cb.borrow_mut() = Some(resume.clone());
            if let Some(id) = *identified_for_secured.borrow() {
                reg_for_secured
                    .borrow_mut()
                    .hold_session(id, resume.clone());
                let control = control_for_secured.clone();
                resume.on_paths(move |paths| control.link_paths(&id, paths));
            }
        };
        let session = run_pairing_and_session(
            transport,
            identity,
            &pairing,
            &control,
            role,
            false,
            capture.as_ref().as_ref(),
            &status,
            &live,
            &modules,
            offer,
            Some(&mut on_identified),
            resumable,
            &mut on_secured,
        );
        // Dropping the session is how a phone already leaves its Bluetooth
        // link for the network; a link replaced by a better one ends the same
        // way, since the peer is closing its side of it too.
        let result = tokio::select! {
            result = session => result,
            () = superseded.notified() => {
                tracing::info!("a better link to this peer came up; closing this one");
                Ok(())
            }
        };

        let who = *identified.borrow();
        if joined.get() {
            match &result {
                Ok(()) => tracing::debug!("the connection now carries the running session"),
                Err(e) => {
                    tracing::info!(error = %e, "a connection could not join the running session")
                }
            }
            return;
        }
        // The session is over, whichever way: nothing may join it now.
        if let Some(resume) = held.borrow_mut().take() {
            resume.end("the session ended");
            if let Some(id) = who {
                registry.borrow_mut().session_ended(id, resume.tag());
            }
        }
        let unlinked = who.is_none_or(|id| registry.borrow_mut().release_link(id, claimed.get()));
        match &result {
            Ok(()) => tracing::info!("peer session ended"),
            Err(e) if !unlinked => {
                tracing::debug!(error = %e, "a second link to a peer still linked ended");
            }
            Err(e) if who.is_some_and(|id| live.is_forgotten(&id)) => {
                tracing::debug!(error = %e, "refused a machine removed from this mesh");
            }
            Err(e) => {
                tracing::warn!(error = %e, "peer session ended with error");
                // Only a peer that was greeted: a stranger refused at the
                // door, or a socket that closed before `Hello`, is not a link
                // the user has to hear dropped.
                let known = who.and_then(|id| {
                    let reg = registry.borrow();
                    reg.get(&id).map(|peer| (id, peer.display_name.clone()))
                });
                if let Some((id, name)) = known {
                    cursio_core::health::link_dropped(id, &name, &e.to_string());
                }
            }
        }

        // Offline, never removed: the machine still exists, it's just not here
        // right now. This is the whole point of the registry. Not while another
        // link to it is up: that one is the peer's link now.
        if let Some(id) = who.filter(|_| unlinked) {
            registry.borrow_mut().mark_offline(id);
            control.link_down(&id);
        }
    });
}

/// Called once a session learns who the peer is (from the Hello handshake).
/// The mesh uses it to mark an inbound peer online — until the handshake, a
/// peer that dialled *us* is just an anonymous socket. Not `Send`: it reaches
/// the mesh's own `Rc` state, which never leaves this thread. Given the
/// sessions the peer holds, it answers whether this connection becomes a
/// session, closes, or joins one already running.
pub(crate) type IdentifiedCallback<'a> = dyn FnMut(cursio_protocol::PeerId, &str, &[cursio_protocol::SessionTag]) -> cursio_core::link::Admit
    + 'a;

/// Shared tail of `serve`/`connect`: Hello handshake, pair on first meeting,
/// bring up a Noise-encrypted channel, then run heartbeats over it.
///
/// The Hello handshake and pairing run over the plaintext transport — that's
/// safe, since SPAKE2 never sends the password. Once we hold the shared key
/// (freshly paired, or recovered from the trust store), we wrap the transport
/// in [`EncryptedTransport`] so all subsequent traffic (heartbeats today; input
/// and video later) is confidential and integrity-protected.
// Down from 13 arguments to 10, and the rest are session plumbing rather than
// per-module knobs — a new module adds nothing here.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_pairing_and_session<T: cursio_transport::Transport + 'static>(
    transport: T,
    identity: NodeIdentity,
    // Consulted only for a stranger, and told how the meeting went: a code
    // is spent by one success, and every window shuts after a few failures.
    pairing: &PairingWindow,
    // Told what this link agreed once it is up, and forgets it when it ends.
    control: &cursio_core::control::NodeState,
    role: Role,
    test_input: bool,
    // Capture is created ONCE by the caller and shared by every peer session —
    // see [`PersistentCapture`] for why it must not be rebuilt per connection.
    capture: Option<&PersistentCapture>,
    // Machine-wide power/network state: greeted with, then polled by the
    // session loop so a change reaches every peer.
    status: &SharedNodeStatus,
    // Where this machine stands, which modules the user has on, and whether
    // input is shared — as of the greeting, and followed while the link runs.
    // The edge to *this* peer is derived from the two positions; both the
    // cursor handoff and the window-drag trigger read that answer back.
    live: &cursio_core::live::LiveConfig,
    // Everything this node can run. Built once by the caller; a session takes
    // what negotiation agreed on. Replaces the five per-module flags that used
    // to be threaded through here.
    modules: &cursio_core::module::ModuleRegistry,
    offer: ServiceOffer,
    on_identified: Option<&mut IdentifiedCallback<'_>>,
    // The sessions this node can resume, for the greeting.
    resumable: Vec<cursio_protocol::SessionTag>,
    // Told the session's resume handle once the link is secured.
    on_secured: &mut dyn FnMut(&cursio_transport::resume::ResumeHandle),
) -> anyhow::Result<()> {
    // Handshake, pairing, Noise and module negotiation are the same five steps
    // for every node, so they live in `link` — the app and a phone run this
    // exact sequence rather than a second copy of it.
    let password = pairing.password();
    // Subscribed before reading, so a change made while the link comes up is
    // still seen by the session and sent on.
    let settings = live.subscribe();
    let greeted = settings.borrow().clone();
    let can_send_input = capture.is_some();
    let sends_input = can_send_input && greeted.input;
    let plan = cursio_core::link::Handshake {
        identity: &identity,
        password: &password,
        role,
        registry: modules,
        position: greeted.position,
        service: offer,
        status: status.borrow().current(),
        sends_input,
        resumable,
    };
    let path = cursio_core::control::LinkPath::of(transport.kind());
    let identified = cursio_core::link::identify(transport, &plan).await?;
    // Report who this turned out to be before the slower half runs: for an
    // inbound connection this is the first moment we know at all, and the mesh
    // wants such a peer marked online without waiting out pairing.
    if let Some(cb) = on_identified {
        let peer = &identified.peer;
        match cb(peer.peer_id, &peer.display_name, &peer.resumable) {
            cursio_core::link::Admit::Session => {}
            cursio_core::link::Admit::Refuse => return Ok(()),
            cursio_core::link::Admit::Join(session) => {
                return cursio_core::link::join(identified, &plan, &session).await;
            }
        }
    }
    let peer_id = identified.peer.peer_id;
    let negotiated = identified.negotiated().clone();
    let (link, paired_now) = pairing.secure(identified, &plan).await?;
    if paired_now {
        // Paired again after being removed: its links may stay up now.
        live.unforget(peer_id);
    }
    on_secured(&link.resume);
    let peer = link.peer;

    // Now that the peer has said where it stands, capture can watch the edge
    // between us, alongside every other peer's. This is what replaced
    // `--peer-edge`: both machines already know, and the flag only existed
    // because the barrier had to be armed before anyone had spoken. From here
    // this session hears only the input and handoffs that concern its peer.
    let alone = capture.is_none_or(|c| !c.hub.machine().linked);
    let hub_link = capture.map(|capture| {
        let placement = Placement {
            edge: link.peer_edge,
            distance: greeted.position.distance_to(peer.position),
            takes_cursor: peer.device_kind.takes_cursor(),
        };
        let (ports, machine) = capture.hub.attach(peer.peer_id, placement);
        capture.apply(machine);
        HubLink {
            capture,
            token: ports.token,
            events: ports.events,
            commands: ports.commands,
        }
    });
    let token = hub_link.as_ref().map(|l| l.token);
    let hub = capture.map(|c| c.hub.clone());
    let (skipped, inapplicable) = cursio_core::control::link_skips(&negotiated, peer.device_kind);
    control.link_up(
        peer.peer_id,
        &peer.display_name,
        cursio_core::control::LinkView {
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
    let mut secure = link.transport;
    let built = link.modules;
    let live_link = cursio_core::reconfigure::LinkLive {
        registry: modules,
        local: &identity,
        peer: peer.clone(),
        advertised: cursio_core::link::advertise(modules, sends_input),
        position: greeted.position,
        settings,
        can_send_input,
        on_change: {
            let control = control.clone();
            let (id, name) = (peer.peer_id, peer.display_name.clone());
            let takes_cursor = peer.device_kind.takes_cursor();
            let peer_kind = peer.device_kind;
            Box::new(move |state| {
                if state.edge_changed {
                    match state.peer_edge {
                        Some(edge) => {
                            tracing::info!(peer = %name, ?edge, "this peer now borders another edge")
                        }
                        None => {
                            tracing::info!(peer = %name, "this peer no longer borders any edge")
                        }
                    }
                }
                // Distance can change with the edge staying put, and it decides
                // which of two peers beyond one edge the cursor reaches.
                if let (Some(hub), Some(token)) = (hub.as_ref(), token) {
                    let placement = Placement {
                        edge: state.peer_edge,
                        distance: state.position.distance_to(state.peer_position),
                        takes_cursor,
                    };
                    cursio_input::set_watched_edges(hub.place(token, placement).edges);
                }
                control.link_reconfigured(
                    &id,
                    state.peer_position,
                    state
                        .negotiated
                        .agreed
                        .iter()
                        .map(|id| id.to_string())
                        .collect(),
                    cursio_core::control::link_skips(&state.negotiated, peer_kind),
                );
            })
        },
    };

    // Test mode: stream a synthetic mouse pattern to the peer, then finish.
    // Lets us verify remote injection before the capture side exists.
    if test_input {
        tracing::info!("sending synthetic test input (a square) to the peer");
        send_test_pattern(secure.as_mut()).await?;
        let _ = secure.send(&cursio_protocol::Message::Bye).await;
        tracing::info!("test input sent; closing");
        return Ok(());
    }

    // Set up an input injector so the peer's input events replay on this
    // machine. On platforms without a backend yet, events are just logged.
    //
    // Shared (Rc/RefCell) because two closures below need it: one injects peer
    // events, the other releases whatever is still held when ownership changes.
    // Both run on this single-threaded async context, never concurrently.
    let injector = std::rc::Rc::new(std::cell::RefCell::new(make_injector()));

    // This machine's real mouse/keyboard, as the hub routes them to this link.
    // The hotkey filter and edge-crossing forwarder are already running inside
    // the persistent capture.
    let mut hub_link = hub_link;
    let (captured, commands) = match hub_link.as_mut() {
        Some(l) => (Some(&mut l.events), Some(&mut l.commands)),
        None => (None, None),
    };

    // Who starts with the shared cursor was settled while the link came up —
    // see `CursorSetup`, which weighs what sort of machine the peer is before
    // falling back to comparing ids.
    //
    // That comparison knows only this link. With another peer already linked,
    // losing it would start this machine driving the newcomer: the cursor would
    // leave the screen the user is working on because a machine switched on.
    // So only a first link may start that way; a later one starts with the
    // cursor held here, which the peer — which either agrees or also holds its
    // own — cannot turn into both sides streaming to each other.
    let mut cursor_setup = link.cursor;
    if !alone && cursor_setup.peer_takes_cursor {
        cursor_setup.start_active = true;
    }

    tracing::info!(
        start_active = cursor_setup.start_active,
        peer_kind = ?peer.device_kind,
        peer_takes_cursor = cursor_setup.peer_takes_cursor,
        "session up — sharing input; press Ctrl+Alt+G to toggle control (Ctrl-C to stop)"
    );
    let inject_ref = injector.clone();
    let release_ref = injector.clone();
    let relay = capture.map(|c| c.hub.clone());
    // Level with the reading the greeting carried, so this link announces only
    // what changes from here — and shares the monitor with every other session,
    // so a change reaches all of them.
    let status_ref = status.clone();
    let mut status_cursor = status_ref.borrow().cursor();
    let result = session::run_session_live(
        secure,
        cursor_setup,
        captured,
        move |event| {
            // A peer moving the cursor held here — a phone used as a touchpad —
            // is one more mouse on this machine, so it follows the cursor the
            // way the local one does: across an edge to the machine beyond.
            // Once this machine drives another, injecting here would move a
            // pointer that is parked against the edge and hidden.
            let relayed = relay
                .as_ref()
                .is_some_and(|hub| hub.route_event(session::CapturedInput::from(event)));
            if !relayed {
                inject(inject_ref.borrow_mut().as_mut(), event);
            }
        },
        commands,
        move |ownership| {
            // Whatever the peer was holding down stops being ours to finish the
            // moment control moves: the matching key-up/button-up will be
            // delivered to the *other* machine, so anything still pressed here
            // would stay pressed forever. A stuck right-button opens a context
            // menu that swallows input on both machines; a stuck left-button is
            // worse. Release before doing anything else with the new state.
            if let Some(inj) = release_ref.borrow_mut().as_mut() {
                if let Err(e) = inj.release_all() {
                    tracing::warn!(error = %e, "failed to release held input on ownership change");
                }
            }
            if ownership.panic {
                // Emergency: drop to a safe idle. Barriers fully down (not just
                // ungrabbed-but-armed) so nothing can re-capture, and show the
                // cursor. The hotkey path already set Idle directly; do it again
                // idempotently in case this arrives first.
                if let (Some(c), Some(token)) = (capture, token) {
                    c.hub.note_driving(token, false);
                    // `Idle` deliberately still observes edges on macOS so the
                    // initially non-owning machine can start a handoff. Panic is
                    // different: it promises that barriers are DOWN. Every edge
                    // stays down until a link changes, otherwise a mouse still
                    // moving toward the edge immediately hands control away
                    // again (the repeated PANIC -> crossing loop in the logs).
                    cursio_input::set_watched_edges(c.hub.disarm().edges);
                    c.grab.set_mode(cursio_input::InputMode::Idle);
                    if c.hidden.replace(false) {
                        set_cursor_hidden(c.cursor.borrow_mut().as_mut(), false);
                    }
                }
                set_injecting(release_ref.borrow_mut().as_mut(), true);
                tracing::warn!("panic recovery: input returned to this machine, barriers down");
                return;
            }
            // Two states, because there is one shared cursor and every machine
            // can move it. Holding it is Active; not holding it is Dormant —
            // local input is grabbed and streamed to whichever machine does.
            //
            // A third state sorted by `driving` was tried and is what made a
            // mouse stop working: a machine that had handed the cursor back
            // landed in Idle, where its pointer acts locally and moves nothing
            // shared. The user then had one mouse that drove both screens and
            // one that drove neither. Which side the user is sitting at is not
            // knowable here and does not need to be — an unheld cursor is
            // driven from wherever the hand is.
            //
            // Which of the two this *machine* is in is the hub's answer, not
            // this link's: another link may be driving its peer while this one
            // holds nothing, and each writing its own view over the grab left
            // it saying whatever the last report said.
            //
            // Cursor visibility before the injector, and both before the log
            // line. On macOS showing the cursor also warps it back from the
            // corner it was parked in, and the injector re-seeds its shadow
            // position from the real pointer the moment it goes active — in the
            // other order it seeds from the parking corner, and the peer's next
            // movement teleports the pointer there.
            if let (Some(c), Some(token)) = (capture, token) {
                // Before `apply`: the mode change it makes is what the backend
                // places the pointer on, and every handoff overwrites this, so
                // a hotkey takeback never inherits an older crossing's spot.
                cursio_input::enter_at(
                    ownership
                        .entry
                        .map(|(edge, pos_norm)| cursio_input::EdgeCrossing { edge, pos_norm }),
                );
                c.apply(c.hub.note_driving(token, ownership.driving));
                // The cursor just left across the peer-facing edge, so this
                // machine's own pointer is pinned against it — a `pos_norm` of
                // exactly 0.0 or 0.9989 in the log is that. Left there, the next
                // small movement is outward and reads as another crossing, and
                // two handoffs with no return between them leave this machine
                // believing it drives a peer the user is not sitting at. Windows
                // already does this from its hook; macOS had no equivalent.
                if !ownership.active {
                    if let Some(cursor) = c.cursor.borrow_mut().as_mut() {
                        cursor.nudge_off_peer_edge();
                    }
                }
            }
            set_injecting(release_ref.borrow_mut().as_mut(), ownership.active);
            tracing::info!(active = ownership.active, "ownership changed");
        },
        built,
        move || status_ref.borrow_mut().poll(&mut status_cursor),
        Some(live_link),
        Some(peer_id),
    )
    .await;

    // TEMP hang-diagnosis (2026-07-21): if this line never prints after a link
    // drop, the hang is inside run_session (its teardown or dropping the modules,
    // e.g. an audio thread's blocking join). Remove once fixed.
    tracing::info!("run_session returned to supervisor; cleaning up session");

    // Session over (peer left, link dropped, or error): release any keys/buttons
    // still held via the injector, so a key that was down when the connection
    // died doesn't stick on this machine.
    if let Some(inj) = injector.borrow_mut().as_mut() {
        if let Err(e) = inj.release_all() {
            tracing::warn!(error = %e, "failed to release held input on session end");
        }
    }
    tracing::info!("released held input; parking capture");
    // Park the capture, but do NOT tear it down: it outlives this session now.
    //
    // This used to send `Shutdown`, because each session built its own capture
    // and leaving one running made the next a duplicate (two Wayland portal
    // sessions both reporting the same barrier hit — one crossing arrived four
    // times after four reconnects). With a single [`PersistentCapture`] owned by
    // the caller that reasoning inverts: killing the backend here would end the
    // portal session we deliberately keep, so the next peer would have to
    // re-acquire it — the permission re-prompt and reconnect hang this design
    // exists to avoid.
    //
    // What it rests in is decided by the peers still linked: with none, `Idle`
    // — no grab, no barriers, backend alive and ready for the next peer; with
    // others, whatever they need, minus this peer's edge.
    drop(hub_link);

    match result {
        Ok(()) | Err(session::SessionError::PeerLeft) => {
            tracing::info!("session ended");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Stream a synthetic mouse pattern (a square, then a left click) to the peer
/// as `Message::Input` events, so remote injection can be tested before the
/// capture side exists. The peer's cursor should trace the square.
pub(crate) async fn send_test_pattern<T: Transport + ?Sized>(
    transport: &mut T,
) -> anyhow::Result<()> {
    use cursio_protocol::{InputEvent, Message, MouseButton};

    let step = 20;
    let reps = 5; // 5 * 20 = 100px per side
    let legs = [(step, 0), (0, step), (-step, 0), (0, -step)];
    for (dx, dy) in legs {
        for _ in 0..reps {
            transport
                .send(&Message::Input(InputEvent::PointerMotion { dx, dy }))
                .await?;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    // A left click to round it off.
    transport
        .send(&Message::Input(InputEvent::PointerButton {
            button: MouseButton::Left,
            pressed: true,
        }))
        .await?;
    transport
        .send(&Message::Input(InputEvent::PointerButton {
            button: MouseButton::Left,
            pressed: false,
        }))
        .await?;
    Ok(())
}
