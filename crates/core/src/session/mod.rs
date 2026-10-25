//! Session handshake and liveness.
//!
//! Once a [`Transport`] is connected (and, for a first meeting, pairing has
//! run), a session begins with a [`Hello`] exchange — each side advertises its
//! protocol version and identity, and a version mismatch is rejected up front.
//! The greeting is preceded by the update lane's frozen preamble, so a
//! mismatch is learned from a message every build can read, and ends in
//! [`SessionError::UpdateOnly`] — a link that can still carry a new build —
//! rather than in a decode error.
//! After that the session runs a heartbeat loop so both ends detect a dead link
//! promptly, until either side sends [`Message::Bye`] or the link drops.

use std::time::Duration;

use cursio_protocol::{Message, ModuleId, PeerId, PROTOCOL_VERSION};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cursio_transport::{Transport, TransportError, TransportTx};

use crate::module_host::{self, LinkTx, ModuleHost};
use crate::session::forward::drain_received_input;
use crate::session::forward::forward_captured;
use crate::session::forward::inbound_channel;
use crate::session::forward::next_inbound;
use crate::session::forward::recv_captured;
use crate::session::forward::recv_maybe;
use crate::session::latency::InteractiveProbe;
use crate::session::latency::LatencyWindow;

mod forward;
mod handshake;
mod latency;
mod role;
#[cfg(test)]
mod test_support;

pub use forward::{input_channel, CapturedInput, InputReceiver, InputSender};
pub use handshake::{handshake, handshake_as, local_device_kind, PeerHello};
pub use role::{CursorRole, CursorSetup, Ownership, OwnershipCmd};

/// Errors during the session handshake or run loop.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// A peer from before the update lane: it greeted with its old `Hello`
    /// and cannot read anything newer, so it has to be updated by hand once.
    #[error(
        "protocol version mismatch: local {local}, peer {peer}; the peer is too old to be \
         updated over the mesh — install this version on it by hand once"
    )]
    VersionMismatch { local: u16, peer: u16 },
    /// The peer speaks a different session protocol but the same update lane:
    /// the link can carry builds and nothing else.
    #[error("peer {} speaks protocol {} (this build {}); only updates can cross", .0.peer_id, .0.protocol_version, PROTOCOL_VERSION)]
    UpdateOnly(cursio_protocol::update::Preamble),
    #[error("expected Hello, got a different message")]
    UnexpectedMessage,
    #[error("peer ended the session")]
    PeerLeft,
    #[error("peer stopped responding (no traffic for {0:?})")]
    PeerTimeout(Duration),
}

/// How often to send a heartbeat while a session is idle.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Warn when one turn of the session loop works for at least this long.
///
/// The loop now holds only input, ownership, the heartbeat and node status, so
/// a slow turn here is exactly a stall the cursor felt — no module can cause
/// one any more, which is what makes the warning worth reading when it fires.
/// Timed from the moment a turn wakes, not from the previous turn: with no
/// module ticking it, an idle loop waits in `select!` for up to a heartbeat,
/// and measuring the gap between turns reported that idleness as a
/// five-second stall every five seconds.
const SLOW_TURN_WARN: Duration = Duration::from_millis(250);

const SESSION_DRAIN_DEADLINE: Duration = Duration::from_secs(2);

/// How long to wait for *any* inbound traffic before declaring the peer gone.
///
/// A machine that loses power (or has its cable pulled) never sends a FIN, so
/// the link looks perfectly healthy until the OS TCP timeout fires — minutes
/// later. Until then every send queues into the void and the session appears
/// frozen. Noticing ourselves turns that into a few seconds.
///
/// Three heartbeats: one missed beat is ordinary jitter, three is gone.
const PEER_TIMEOUT: Duration = Duration::from_secs(15);

/// Run the session loop until the peer leaves or the link drops.
///
/// Ownership model — **one global active machine** (MWB shared cursor):
/// - **Active** (`am_active`): this machine holds the shared cursor. Local input
///   acts normally (not grabbed); inbound peer input is injected here; our own
///   captured input is *not* forwarded (it already acts locally).
/// - **Dormant**: local input is grabbed and streamed to the active peer;
///   inbound input is *not* injected (we're not the one holding the cursor).
///   Reached only by *handing the cursor over*, never at startup — see
///   [`Ownership::initial`].
/// - **Idle**: what the machine without the cursor does before anyone has
///   handed anything anywhere. Its own keyboard and mouse act on it, normally.
///
/// Exactly one side is active at any moment. The two sides start in opposite
/// states (`start_active`) and every handoff flips *both* atomically: the
/// hotkey sends [`Message::Grab`] to make the peer active, and going active
/// ourselves happens only when the peer hands it to us. This makes the
/// both-own deadlock (which caused an input feedback loop) impossible.
///
/// The parameters wire this crate to platform code without depending on it:
/// - `cursor` — how the shared cursor is arranged on this link: who starts with
///   it, and whether the peer is the sort of machine that can hold it at all.
///   Build it with [`CursorSetup::negotiate`], which both sides run to reach
///   complementary answers.
/// - `captured` — local input events; forwarded only while dormant.
/// - `on_input` — inject an inbound event; called only while active.
/// - `commands` — hotkey toggle requests; `None` for a session that never
///   initiates a handoff.
/// - `on_ownership` — called on every change so the caller can grab/ungrab and
///   hide/show the cursor to match.
/// - `modules` — subsystems multiplexed onto the same channel. Each runs on a
///   thread of its own ([`crate::module_host`]) with its own handle onto the
///   link, and the reader routes its messages straight to it, so nothing a
///   module does — however long it waits — reaches this loop. Input
///   deliberately stays hard-wired here rather than becoming a module: its
///   ownership/grab/edge logic is ordering-sensitive and `!Send`, and it is
///   what this loop now exists for.
///
/// Takes the transport **by value** (`Box<dyn Transport>`) so it can
/// [`split`](Transport::split) it: the receive half runs in its own task and is
/// therefore never cancelled. That matters because `recv` is not cancel-safe
/// for the encrypted transport — cancelling it mid-await (which a plain
/// `select!` over `recv` does whenever a send wins) can desynchronise the Noise
/// cipher and silently drop all later inbound messages.
#[allow(clippy::too_many_arguments)]
pub async fn run_session<F, G, H>(
    transport: Box<dyn Transport>,
    cursor: CursorSetup,
    captured: Option<&mut InputReceiver>,
    on_input: F,
    commands: Option<&mut tokio::sync::mpsc::UnboundedReceiver<OwnershipCmd>>,
    on_ownership: G,
    modules: Vec<Box<dyn crate::module::SessionModule>>,
    poll_status: H,
) -> Result<(), SessionError>
where
    F: FnMut(cursio_protocol::InputEvent),
    G: FnMut(Ownership),
    H: FnMut() -> Option<cursio_protocol::NodeStatus>,
{
    run_session_live(
        transport,
        cursor,
        captured,
        on_input,
        commands,
        on_ownership,
        modules,
        poll_status,
        None,
        None,
    )
    .await
}

/// [`run_session`] for a link that follows this node's settings while it runs:
/// a module switched on or off, a new position, input turned off. See
/// [`crate::reconfigure`].
///
/// Input turned off is the one change the loop itself carries out. The cursor
/// comes home if it was away, the hotkey and edge crossings are ignored, and a
/// peer handing this machine the dormant side is refused — being dormant with
/// nothing to forward would leave this machine's own keyboard grabbed and
/// going nowhere.
#[allow(clippy::too_many_arguments)]
pub async fn run_session_live<F, G, H>(
    transport: Box<dyn Transport>,
    cursor: CursorSetup,
    // Borrowed, not owned: the capture that feeds these outlives any single
    // session (it holds a portal permission that must not be re-acquired per
    // connection), so a session reads from them and hands them back.
    mut captured: Option<&mut InputReceiver>,
    mut on_input: F,
    mut commands: Option<&mut tokio::sync::mpsc::UnboundedReceiver<OwnershipCmd>>,
    mut report_ownership: G,
    modules: Vec<Box<dyn crate::module::SessionModule>>,
    mut poll_status: H,
    live: Option<crate::reconfigure::LinkLive<'_>>,
    // Who is on the other end, so what it later reports about its power and
    // network replaces what its greeting said wherever the node shows it.
    peer: Option<PeerId>,
) -> Result<(), SessionError>
where
    F: FnMut(cursio_protocol::InputEvent),
    G: FnMut(Ownership),
    H: FnMut() -> Option<cursio_protocol::NodeStatus>,
{
    tracing::debug!("splitting transport");
    // Prioritized split: the tx half queues frames by priority and a background
    // drainer writes them, so a bulk transfer (clipboard image, file) can never
    // stall input on the shared channel. Falls back to a plain split for
    // transports that don't override it.
    let (tx, mut rx) = transport.split_prioritized();
    let tx = LinkTx::new(tx);
    let mut outbox = crate::session_outbox::SessionOutbox::new(tx.fork());

    // Each module on its own thread, sending through its own handle onto the
    // queue. Started before the reader so nothing it is sent can arrive before
    // it has an inbox; each one's `start` runs on its own thread, so a module
    // listing devices no longer holds the first input event.
    let (failures_tx, mut failures) = tokio::sync::mpsc::unbounded_channel();
    let mut hosts = Vec::with_capacity(modules.len());
    for module in modules {
        let name = module.name();
        match ModuleHost::spawn(module, tx.fork(), failures_tx.clone()) {
            Ok(host) => hosts.push(host),
            Err(e) => {
                tracing::error!(module = name, error = %e, "could not start the module's thread; continuing without it");
            }
        }
    }
    let (mut reconf, mut settings, mut stopped) = match live {
        Some(live) => {
            let (reconf, settings, stopped) = crate::reconfigure::Reconf::new(live);
            (Some(reconf), Some(settings), Some(stopped))
        }
        None => (None, None, None),
    };
    // Kept only while modules can still be started on this link.
    let failures_tx = reconf.as_ref().map(|_| failures_tx);

    // Liveness: when did we last hear *anything* from the peer? Any inbound
    // message counts, not just heartbeats — module traffic proves the link just
    // as well, so a busy session never trips the timeout. Stamped by the
    // reader, because module traffic no longer passes through this loop.
    let epoch = tokio::time::Instant::now();
    let last_heard = Arc::new(AtomicU64::new(0));

    // Reader task: owns the receive half and never gets cancelled, so recv
    // state stays consistent. It routes each module's messages straight to
    // that module and forwards only the session's own vocabulary here, so a
    // flood of file or window traffic never sits between two input events.
    let router = module_host::router_for(&hosts);
    let reader_router = router.clone();
    let (inbound_tx, mut inbound_rx) = inbound_channel();
    let reader_heard = last_heard.clone();
    let reader = tokio::spawn(async move {
        tracing::debug!("session reader task started");
        // On any recv error the loop ends and `inbound_tx` drops, which the
        // main task observes as the channel closing (a dropped link).
        while let Ok(msg) = rx.recv().await {
            reader_heard.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
            let session_message = reader_router.route(msg);
            if let Some(error) = reader_router.error() {
                tracing::error!(%error, "session receive queue failed");
                inbound_tx.abort();
                break;
            }
            if let Some(msg) = session_message {
                if inbound_tx.send((msg, std::time::Instant::now())).is_err() {
                    break;
                }
            }
        }
    });

    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    let mut nonce: u64 = 0;
    let mut interactive_probe = InteractiveProbe::new(peer);

    // Power and network state. Separate from the heartbeat because the two ask
    // different questions at different rates: liveness every five seconds,
    // "did anything about this machine change" every thirty. Sharing the
    // heartbeat's tick would read sysfs six times more often than anything
    // needs, on every link.
    let mut status_ticker = tokio::time::interval(crate::node_status::POLL_INTERVAL);
    // The first tick of a tokio interval fires immediately, which here would
    // re-announce the reading the greeting just carried.
    status_ticker.tick().await;

    // Ownership: is THIS machine the active one (holds the shared cursor)?
    // Peers start in opposite states so exactly one is active from the outset.
    // A machine driving a peer's screen is not one to restart for an update:
    // the user's hand is on this mouse. Every report passes through here, so
    // the mark follows each change without a line at each call site.
    let mut driving_mark = crate::update::quiet::DrivingMark::default();
    // And which peer: a call's voice follows the user to that screen.
    let mut driven_mark = crate::presence::DrivenMark::new(peer);
    let mut on_ownership = |ownership: Ownership| {
        driving_mark.set(ownership.driving);
        driven_mark.set(ownership.driving);
        report_ownership(ownership);
    };
    let mut role = CursorRole::new(cursor.start_active);
    let mut am_active = role.active();
    on_ownership(Ownership {
        active: am_active,
        panic: false,
        driving: role.driving(),
        entry: None,
    });
    tracing::info!(am_active, "initial ownership");

    // When the current turn woke, so a stall with no owner still leaves a
    // mark. Without it a blocking call in a spot nobody thought to instrument
    // is invisible — which is how a 2.6 s freeze once reached the user with a
    // clean log. Set as each arm starts; see `SLOW_TURN_WARN`.
    let mut working_since: Option<tokio::time::Instant> = None;

    // A message that ended a batch of received input, handled on the next
    // turn so it keeps its place after the input read before it.
    let mut held_back: Option<(Message, std::time::Instant)> = None;

    let mut inject_latency = LatencyWindow::new("received->injected");
    // The injector's own share of the figure above. What is left over is
    // queueing, and the two call for different fixes.
    let mut inject_call = LatencyWindow::new("injector call");
    let mut forward_latency = LatencyWindow::new("captured->queued");

    let mut prefer_local = true;
    let result = loop {
        if let Some(error) = router.error() {
            break Err(SessionError::Transport(TransportError::Other(error)));
        }
        if inbound_rx.stats().failed || captured.as_ref().is_some_and(|rx| rx.stats().failed) {
            break Err(SessionError::Transport(TransportError::Other(
                "input receive queue overloaded".into(),
            )));
        }
        if let Some(woke) = working_since.take() {
            let turn = woke.elapsed();
            if turn >= SLOW_TURN_WARN {
                tracing::warn!(
                    took_ms = turn.as_millis() as u64,
                    "session loop turn was slow; input and cursor handoffs waited"
                );
            }
        }

        tokio::select! {
            // Ordered, not random. The branches are already written most- to
            // least- urgent, and an ownership change queued alongside input has
            // to be applied first: the same events mean different things either
            // side of it — forwarded to the peer, or acting locally — and
            // letting the race decide drops the ones read too early. The timers
            // above it fire on their own schedule, so nothing starves.
            biased;

            result = outbox.finished() => {
                break Err(SessionError::Transport(result.err().unwrap_or(TransportError::Closed)));
            }

            _ = ticker.tick() => {
                working_since = Some(tokio::time::Instant::now());
                inject_latency.report_if_due();
                inject_call.report_if_due();
                forward_latency.report_if_due();
                interactive_probe.report();
                // Has the peer gone quiet? Checked on the heartbeat tick, so
                // detection costs nothing extra and lands within one interval
                // of the deadline.
                let heard_at = Duration::from_millis(last_heard.load(Ordering::Relaxed));
                let silent_for = epoch.elapsed().saturating_sub(heard_at);
                // A resumable link that lost its connection is waiting for
                // another; the silence is the gap, not the peer leaving. The
                // link ends the session itself if none comes back.
                if tx.holding() {
                    continue;
                }
                if silent_for >= PEER_TIMEOUT {
                    tracing::warn!(?silent_for, "peer stopped responding; ending session");
                    break Err(SessionError::PeerTimeout(silent_for));
                }
                // Degrading, but not dead yet. Worth saying out loud: input and
                // audio both feel broken well before the link actually drops,
                // and without this the log looks perfectly healthy right up to
                // the timeout — leaving "it froze for a few seconds" with
                // nothing to point at.
                if silent_for >= HEARTBEAT_INTERVAL * 2 {
                    tracing::warn!(
                        ?silent_for,
                        "peer is slow to respond; input and audio may lag or stutter"
                    );
                }
                nonce = nonce.wrapping_add(1) & !cursio_protocol::HEARTBEAT_ECHO;
                let sent_at = std::time::Instant::now();
                if let Err(e) = outbox.send(&Message::Heartbeat { nonce }).await {
                    break Err(SessionError::from(e));
                }
                interactive_probe.sent(nonce, sent_at);
                tracing::trace!(nonce, "sent heartbeat");
            }
            _ = status_ticker.tick() => {
                working_since = Some(tokio::time::Instant::now());
                // `poll_status` reads the machine-wide monitor and answers
                // `None` unless something changed by the protocol's own
                // standard — so a plugged-in desktop sends nothing here for the
                // life of the session.
                if let Some(status) = poll_status() {
                    tracing::info!(%status, "local status changed; telling peer");
                    if let Err(e) = outbox.send(&Message::NodeStatus(status)).await {
                        break Err(SessionError::from(e));
                    }
                }
            }
            Some(failure) = failures.recv() => {
                tracing::warn!(module = failure.module, error = %failure.error, "module lost the link; ending session");
                break Err(SessionError::from(failure.error));
            }
            latest = settings_changed(&mut settings) => {
                working_since = Some(tokio::time::Instant::now());
                let (Some(r), Some(failures_tx)) = (reconf.as_mut(), failures_tx.as_ref()) else {
                    continue;
                };
                if r.peer_forgotten(&latest) {
                    // Said rather than just dropped, so the peer ends its side
                    // as a goodbye instead of timing out and raising a dropped
                    // link. Teardown below unmounts and stops modules as usual.
                    tracing::info!("this machine was removed from the mesh; closing the link");
                    if role.driving() {
                        role.seize();
                        am_active = role.active();
                        on_ownership(Ownership { active: am_active, panic: false, driving: role.driving(), entry: None });
                    }
                    let _ = outbox.send(&Message::Bye).await;
                    break Ok(());
                }
                let was_on = r.input_on();
                if let Some((msg, restart)) = r.local(&latest) {
                    tracing::info!("settings changed; telling the peer");
                    if let Err(e) = outbox.send(&msg).await {
                        break Err(SessionError::from(e));
                    }
                    r.reconcile(
                        &restart,
                        false,
                        crate::reconfigure::Hosts {
                            hosts: &mut hosts,
                            router: &router,
                            tx: &tx,
                            failures: failures_tx,
                        },
                    );
                }
                if was_on && !r.input_on() && role.driving() {
                    // Input switched off while it was driving the peer: bring
                    // the cursor home, since nothing will be forwarded to
                    // drive it there any more.
                    role.seize();
                    am_active = role.active();
                    tracing::info!("input sharing turned off; the cursor is back on this machine");
                    on_ownership(Ownership { active: am_active, panic: false, driving: role.driving(), entry: None });
                    if let Err(e) = outbox.send(&Message::Grab { active: false }).await {
                        break Err(SessionError::from(e));
                    }
                }
            }
            Some(ids) = stopped_maybe(&mut stopped) => {
                if let (Some(r), Some(failures_tx)) = (reconf.as_mut(), failures_tx.as_ref()) {
                    r.stopped(
                        ids,
                        crate::reconfigure::Hosts {
                            hosts: &mut hosts,
                            router: &router,
                            tx: &tx,
                            failures: failures_tx,
                        },
                    );
                }
            }
            cmd = recv_maybe(&mut commands) => {
                working_since = Some(tokio::time::Instant::now());
                match cmd {
                    // Hotkey: flip which machine is active. Works from EITHER
                    // side — if we're active we hand the cursor over; if we're
                    // dormant we take it. Either way we flip ourselves and tell
                    // the peer to take the opposite state, so the "exactly one
                    // active" invariant holds. (The dormant side can still run
                    // the hotkey because capture keeps reading even while its
                    // input is grabbed.)
                    // A peer that cannot hold the cursor cannot be handed it.
                    // Obeying the hotkey here would grab this machine's input
                    // and stream it to a phone that has nothing to inject it
                    // into — the machine stops responding to its own keyboard,
                    // and the only thing that could give control back is the
                    // peer that was never able to take it.
                    Some(OwnershipCmd::Toggle | OwnershipCmd::HandoffToPeer { .. })
                        if !sharing_input(&reconf) =>
                    {
                        tracing::debug!("input sharing is off; the cursor stays on this machine");
                    }
                    Some(OwnershipCmd::Toggle) if !cursor.peer_takes_cursor => {
                        tracing::info!(
                            "ignoring the ownership hotkey: this peer cannot hold the cursor"
                        );
                    }
                    Some(OwnershipCmd::Toggle) => {
                        role.toggle();
                        am_active = role.active();
                        tracing::info!(am_active, driving = role.driving(), "hotkey toggled ownership");
                        on_ownership(Ownership { active: am_active, panic: false, driving: role.driving(), entry: None });
                        // Peer takes the opposite of our new state.
                        if let Err(e) = outbox.send(&Message::Grab { active: !am_active }).await {
                            break Err(SessionError::from(e));
                        }
                    }
                    // Edge-crossing handoff: only the active side hands off (the
                    // cursor left *us*). If we're already dormant, ignore it —
                    // the crossing belongs to the peer's screen, not ours.
                    // Same reasoning as the hotkey: the cursor cannot cross an
                    // edge into a machine that has no screen behind it.
                    Some(OwnershipCmd::HandoffToPeer { edge, .. }) if !cursor.peer_takes_cursor => {
                        tracing::trace!(?edge, "edge crossing ignored: peer cannot hold the cursor");
                    }
                    Some(OwnershipCmd::HandoffToPeer { edge, pos_norm }) => {
                        // Only the machine holding the cursor can send it away.
                        //
                        // This used to accept an idle machine too, reasoning
                        // that one which has handed nothing over is the one the
                        // user is sitting at. True at the start of a session and
                        // false ever after: a machine that gave the cursor back
                        // is also idle, and its own pointer drifting into the
                        // edge then read as a second handoff — leaving it
                        // convinced it drove a peer nobody was at, with no way
                        // to get the cursor back. Ctrl+Alt+G is how an idle
                        // machine takes control, on every platform.
                        if role.active() {
                            role.hand_over();
                            am_active = role.active();
                            tracing::info!(?edge, pos_norm, "cursor crossed peer edge; handing off");
                            on_ownership(Ownership { active: am_active, panic: false, driving: role.driving(), entry: None });
                            // Peer becomes active, entering on its opposite edge.
                            let msg = Message::CrossEdge { edge: edge.opposite(), pos_norm };
                            if let Err(e) = outbox.send(&msg).await {
                                break Err(SessionError::from(e));
                            }
                        } else {
                            tracing::debug!(
                                ?edge,
                                driving = role.driving(),
                                "edge crossing while not holding the cursor; ignoring"
                            );
                        }
                    }
                    // Emergency: unconditionally take control back here. Become
                    // active (so local input acts locally again and nothing
                    // inbound injects), tell the peer to stand down, and let the
                    // caller restore grab/cursor via on_ownership. Best-effort on
                    // the peer notify — the whole point is that our own side
                    // recovers even if the link is dead.
                    Some(OwnershipCmd::Panic) => {
                        tracing::warn!("PANIC: forcing control back to this machine");
                        role.panic();
                        am_active = role.active();
                        on_ownership(Ownership { active: am_active, panic: true, driving: role.driving(), entry: None });
                        let _ = outbox.send(&Message::Grab { active: false }).await;
                    }
                    None => commands = None,
                }
            }
            local = recv_captured(&mut captured),
                if prefer_local || (inbound_rx.is_empty() && held_back.is_none()) => {
                prefer_local = false;
                working_since = Some(tokio::time::Instant::now());
                match local {
                    // Forwarded only while actually driving the peer. The
                    // active side's input already acts locally; so does the
                    // input of a machine that has handed nothing over, which is
                    // what lets a second keyboard stay usable on its own
                    // machine while the first drives across.
                    Some(event) => {
                        if role.driving() && sharing_input(&reconf) {
                            // `captured` is an Option<&mut _>, so reborrow through
                            // it rather than handing over an &mut &mut.
                            if let Err(e) =
                                forward_captured(&mut outbox, event, captured.as_deref_mut(), &mut forward_latency).await
                            {
                                break Err(e);
                            }
                        }
                    }
                    None => {
                        if captured.as_ref().is_some_and(|rx| rx.stats().failed) {
                            break Err(SessionError::Transport(TransportError::Other("captured input queue overloaded".into())));
                        }
                        captured = None;
                    },
                }
            }
            incoming = next_inbound(&mut held_back, &mut inbound_rx),
                if !prefer_local || captured.as_ref().is_none_or(|rx| rx.is_empty()) => {
                prefer_local = true;
                working_since = Some(tokio::time::Instant::now());
                let (incoming, read_at) = match incoming {
                    Some((msg, read_at)) => (Some(msg), read_at),
                    None => (None, std::time::Instant::now()),
                };
                match incoming {
                    Some(Message::Heartbeat { nonce }) => {
                        tracing::trace!(nonce, "peer heartbeat");
                        if nonce & cursio_protocol::HEARTBEAT_ECHO != 0 {
                            interactive_probe.echo(nonce, std::time::Instant::now());
                        }
                        if nonce & cursio_protocol::HEARTBEAT_ECHO == 0 {
                            let echo = Message::Heartbeat {
                                nonce: nonce | cursio_protocol::HEARTBEAT_ECHO,
                            };
                            if let Err(e) = outbox.send(&echo).await {
                                break Err(SessionError::from(e));
                            }
                        }
                    }
                    // The peer's power or network changed. Nothing routes by it
                    // yet, but the window shows it: only the greeting's reading
                    // used to be kept, so a network a phone reported a moment
                    // after connecting never appeared.
                    Some(Message::NodeStatus(status)) => {
                        tracing::info!(%status, "peer status changed");
                        if let Some(peer) = peer {
                            crate::metrics::note_peer_status(peer, status);
                        }
                    }
                    Some(Message::Input(event)) => {
                        // Inject only while active (we hold the cursor).
                        if am_active {
                            let batch = drain_received_input(event, read_at, &mut inbound_rx, &mut held_back);
                            crate::update::quiet::note_input_activity();
                            for (event, read_at) in batch {
                                tracing::trace!(?event, "peer input");
                                let started = std::time::Instant::now();
                                on_input(event);
                                inject_call.record(started.elapsed());
                                inject_latency.record(read_at.elapsed());
                            }
                        }
                    }
                    // A peer that cannot hold the cursor claiming it anyway.
                    //
                    // Nothing we ship sends this, so it means a peer that has
                    // misdescribed itself or a build we don't know. Obeying it
                    // would freeze this machine's own input with no way back,
                    // so it is refused — and said out loud, because a phone
                    // whose taps do nothing is otherwise a silent mystery.
                    Some(Message::Grab { active: false }) if !cursor.peer_takes_cursor => {
                        tracing::warn!(
                            "peer claimed the cursor but is not a machine that can hold one; ignoring"
                        );
                    }
                    Some(Message::Grab { active: false }) if !sharing_input(&reconf) => {
                        tracing::info!(
                            "peer took the cursor, but input sharing is off here; this machine keeps its own"
                        );
                    }
                    Some(Message::Grab { active }) => {
                        // The peer is handing the cursor to us (active=true) or
                        // taking it (active=false). Adopt the state it dictates;
                        // the sender flips itself to the opposite at the same
                        // time, preserving the "exactly one active" invariant.
                        if active != am_active {
                            role.adopt(active);
                            am_active = role.active();
                            // Named for what it is: a Grab message, NOT motion.
                            // The old wording ("peer moved the cursor") sent me
                            // hunting through the motion path while diagnosing a
                            // frozen cursor.
                            tracing::info!(am_active, "peer handed ownership over (Grab)");
                            on_ownership(Ownership { active: am_active, panic: false, driving: role.driving(), entry: None });
                        }
                    }
                    Some(Message::CrossEdge { edge, pos_norm }) => {
                        // The peer's cursor crossed the edge toward us. Record
                        // that peer even when we are already active: an idle
                        // second machine can begin driving the screen that
                        // already holds the shared cursor, and ignoring that
                        // crossing loses who must regain control on the way
                        // back. That was the one-way mouse failure after a
                        // successful first round trip.
                        let was_active = am_active;
                        role.take_over();
                        am_active = role.active();
                        if !was_active {
                            tracing::info!(?edge, pos_norm, "cursor crossed in from peer");
                            on_ownership(Ownership {
                                active: am_active,
                                panic: false,
                                driving: role.driving(),
                                entry: Some((edge, pos_norm)),
                            });
                        } else {
                            tracing::info!(?edge, pos_norm, "peer began driving the cursor held here");
                        }
                    }
                    Some(Message::Bye) => {
                        tracing::info!("peer sent Bye; closing session");
                        break Err(SessionError::PeerLeft);
                    }
                    Some(Message::Reconfigure(change)) => {
                        tracing::info!(
                            modules = change.modules.len(),
                            position = ?change.position,
                            restart = ?change.restart,
                            "peer changed its settings"
                        );
                        match (reconf.as_mut(), failures_tx.as_ref()) {
                            (Some(r), Some(failures_tx)) => {
                                let restart = r.peer(change);
                                r.reconcile(
                                    &restart,
                                    true,
                                    crate::reconfigure::Hosts {
                                        hosts: &mut hosts,
                                        router: &router,
                                        tx: &tx,
                                        failures: failures_tx,
                                    },
                                );
                            }
                            _ => tracing::debug!("this link follows no settings; keeping what it agreed"),
                        }
                    }
                    // Module traffic never gets here — the reader routes it — so
                    // this is something neither side's session speaks.
                    Some(other) => {
                        tracing::debug!(message = module_host::message_kind(&other), "ignoring unexpected in-session message");
                    }
                    None => {
                        // Reader task ended; the link dropped.
                        break Err(SessionError::Transport(TransportError::Closed));
                    }
                }
            }
        }
    };

    // Bounded: a module wedged in a blocking call is left behind with a
    // warning naming it, rather than holding up the reconnect. Each module's
    // handle onto the link is closed either way, so the link closes when this
    // session is done with it.
    on_ownership(Ownership {
        active: true,
        panic: false,
        driving: false,
        entry: None,
    });
    tracing::info!("session ending; shutting down modules");
    module_host::shut_down(hosts, module_host::MODULE_SHUTDOWN_GRACE).await;

    // The prioritized sender only *queues* on `send`, so a final `Bye`/shutdown
    // message may still be waiting. Flush it to the wire before dropping `tx`,
    // or the drainer task gets cancelled with the goodbye still queued. No-op
    // for a plain (synchronous) sender.
    outbox.finish(SESSION_DRAIN_DEADLINE).await;
    tx.close().await;
    reader.abort();
    tracing::info!("session teardown complete");
    result
}

/// Whether this side forwards its input on the link. A link that follows no
/// settings always does, as it always has.
fn sharing_input(reconf: &Option<crate::reconfigure::Reconf<'_>>) -> bool {
    reconf.as_ref().is_none_or(|r| r.input_on())
}

/// The next batch of modules whose old instance finished stopping; pending
/// forever on a link that follows no settings.
async fn stopped_maybe(
    channel: &mut Option<tokio::sync::mpsc::UnboundedReceiver<Vec<ModuleId>>>,
) -> Option<Vec<ModuleId>> {
    match channel {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Resolves when this node's settings change. A node shutting down drops the
/// sender, which is not a change, so that pends too.
async fn settings_changed(
    settings: &mut Option<tokio::sync::watch::Receiver<crate::live::LiveSettings>>,
) -> crate::live::LiveSettings {
    if let Some(rx) = settings {
        if rx.changed().await.is_ok() {
            return rx.borrow_and_update().clone();
        }
    }
    std::future::pending().await
}

/// Run the session loop, ignoring any inbound input events.
///
/// Thin wrapper over [`run_session`] for callers that only need liveness (no
/// input capture or injection), e.g. tests or a liveness-only role.
pub async fn run_heartbeat(transport: Box<dyn Transport>) -> Result<(), SessionError> {
    // Ownership is irrelevant with no capture or injection: stay dormant and
    // let the peer keep the cursor it already has.
    let cursor = CursorSetup {
        start_active: false,
        peer_takes_cursor: true,
    };
    // Liveness only: nothing here watches the machine, so it never has a status
    // change to report.
    run_session(
        transport,
        cursor,
        None,
        |_| {},
        None,
        |_| {},
        Vec::new(),
        || None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::forward::forward_captured;
    use crate::session::forward::merge_received;
    use crate::session::forward::CAPTURED_INPUT_BATCH;
    use crate::session::latency::LatencyWindow;
    use crate::session::test_support::motion;
    use crate::session::test_support::RecordingSender;
    #[tokio::test]
    async fn capture_forwarding_yields_before_exhausting_a_large_nonmergeable_backlog() {
        let (sender, mut receiver) = input_channel();
        for _ in 0..1000 {
            sender
                .send(CapturedInput::from(cursio_protocol::InputEvent::Key {
                    key: 1,
                    pressed: true,
                }))
                .unwrap();
        }
        let first = receiver.try_recv().unwrap();
        let mut output = RecordingSender(Vec::new());
        forward_captured(
            &mut output,
            first,
            Some(&mut receiver),
            &mut LatencyWindow::new("test"),
        )
        .await
        .unwrap();
        assert!(!receiver.is_empty());
        assert!(output.0.len() <= CAPTURED_INPUT_BATCH);
    }

    #[test]
    fn relative_motion_overflow_splits_instead_of_losing_displacement() {
        let now = std::time::Instant::now();
        let mut batch = vec![(motion(i32::MAX, 0), now)];
        merge_received(&mut batch, motion(1, 0), now);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].0, motion(i32::MAX, 0));
        assert_eq!(batch[1].0, motion(1, 0));
    }

    struct BlockedWire {
        incoming: tokio::sync::mpsc::Receiver<Message>,
        entered: Arc<tokio::sync::Notify>,
    }
    struct BlockedSend(Arc<tokio::sync::Notify>);
    struct ReceiveWire(tokio::sync::mpsc::Receiver<Message>);
    #[async_trait::async_trait]
    impl TransportTx for BlockedSend {
        async fn send(&mut self, _: &Message) -> Result<(), TransportError> {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    #[async_trait::async_trait]
    impl cursio_transport::TransportRx for ReceiveWire {
        async fn recv(&mut self) -> Result<Message, TransportError> {
            self.0.recv().await.ok_or(TransportError::Closed)
        }
    }
    #[async_trait::async_trait]
    impl Transport for BlockedWire {
        async fn send(&mut self, _: &Message) -> Result<(), TransportError> {
            std::future::pending().await
        }
        async fn recv(&mut self) -> Result<Message, TransportError> {
            self.incoming.recv().await.ok_or(TransportError::Closed)
        }
        fn kind(&self) -> &'static str {
            "blocked-test"
        }
        fn split(
            self: Box<Self>,
        ) -> (Box<dyn TransportTx>, Box<dyn cursio_transport::TransportRx>) {
            (
                Box::new(BlockedSend(self.entered)),
                Box::new(ReceiveWire(self.incoming)),
            )
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_sender_does_not_prevent_receiving_goodbye_or_boundless_flush() {
        let (send, incoming) = tokio::sync::mpsc::channel(1);
        let entered = Arc::new(tokio::sync::Notify::new());
        let wire = BlockedWire {
            incoming,
            entered: entered.clone(),
        };
        let start = tokio::time::Instant::now();
        let (result, ()) = tokio::join!(run_heartbeat(Box::new(wire)), async {
            entered.notified().await;
            send.send(Message::Bye).await.unwrap();
        });
        assert!(matches!(result, Err(SessionError::PeerLeft)));
        assert!(start.elapsed() <= SESSION_DRAIN_DEADLINE + Duration::from_millis(1));
    }
}
