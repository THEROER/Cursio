//! The session loop: everything a running link does after its greeting.
//!
//! The `select!` arms share one state — the cursor's role, the outbox, the
//! hosts a reconfiguration may restart — so that state is one struct and each
//! arm is a method on it. What an arm decides comes back as a [`Turn`]: keep
//! going, end cleanly, or end with an error, which `?` carries from a failed
//! send.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cursio_protocol::{Message, ModuleId, PeerId};
use cursio_transport::{Transport, TransportError, TransportTx};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::module_host::{self, LinkTx, ModuleFailure, ModuleHost, Router};
use crate::reconfigure::{Hosts, Reconf};
use crate::session::forward::{
    drain_received_input, forward_captured, inbound_channel, next_inbound, recv_captured,
    recv_maybe, Incoming,
};
use crate::session::latency::{InteractiveProbe, LatencyWindow};
use crate::session::{
    CursorRole, CursorSetup, InputReceiver, Ownership, OwnershipCmd, SessionError,
    HEARTBEAT_INTERVAL, PEER_TIMEOUT, SESSION_DRAIN_DEADLINE, SLOW_TURN_WARN,
};
use crate::session_outbox::SessionOutbox;

/// What one arm of the loop decided. `Break` ends the session cleanly.
type Turn = Result<ControlFlow<()>, SessionError>;

const GO_ON: Turn = Ok(ControlFlow::Continue(()));

/// [`super::run_session`] for a link that follows this node's settings while it runs:
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
    captured: Option<&mut InputReceiver>,
    on_input: F,
    commands: Option<&mut UnboundedReceiver<OwnershipCmd>>,
    report_ownership: G,
    modules: Vec<Box<dyn crate::module::SessionModule>>,
    poll_status: H,
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
    let outbox = SessionOutbox::new(tx.fork());

    // Each module on its own thread, sending through its own handle onto the
    // queue. Started before the reader so nothing it is sent can arrive before
    // it has an inbox; each one's `start` runs on its own thread, so a module
    // listing devices no longer holds the first input event.
    let (failures_tx, failures) = tokio::sync::mpsc::unbounded_channel();
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
    let (reconf, settings, stopped) = match live {
        Some(live) => {
            let (reconf, settings, stopped) = Reconf::new(live);
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
    let (inbound_tx, inbound_rx) = inbound_channel();
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

    // Power and network state. Separate from the heartbeat because the two ask
    // different questions at different rates: liveness every five seconds,
    // "did anything about this machine change" every thirty. Sharing the
    // heartbeat's tick would read sysfs six times more often than anything
    // needs, on every link.
    let mut status_ticker = tokio::time::interval(crate::node_status::POLL_INTERVAL);
    // The first tick of a tokio interval fires immediately, which here would
    // re-announce the reading the greeting just carried.
    status_ticker.tick().await;

    let mut session = SessionLoop {
        role: CursorRole::new(cursor.start_active),
        cursor,
        outbox,
        tx,
        router,
        hosts,
        reconf,
        settings,
        stopped,
        failures,
        failures_tx,
        inbound_rx,
        held_back: None,
        captured,
        commands,
        on_input,
        report_ownership,
        poll_status,
        driving_mark: crate::update::quiet::DrivingMark::default(),
        driven_mark: crate::presence::DrivenMark::new(peer),
        peer,
        nonce: 0,
        ticker: tokio::time::interval(HEARTBEAT_INTERVAL),
        status_ticker,
        probe: InteractiveProbe::new(peer),
        inject_latency: LatencyWindow::new("received->injected"),
        inject_call: LatencyWindow::new("injector call"),
        forward_latency: LatencyWindow::new("captured->queued"),
        epoch,
        last_heard,
    };
    session.tell(false, None);
    tracing::info!(am_active = session.role.active(), "initial ownership");

    let result = session.run().await;
    session.finish().await;
    reader.abort();
    tracing::info!("session teardown complete");
    result
}

struct SessionLoop<'s, F, G, H> {
    cursor: CursorSetup,
    // Ownership: is THIS machine the active one (holds the shared cursor)?
    // Peers start in opposite states so exactly one is active from the outset.
    role: CursorRole,
    outbox: SessionOutbox,
    tx: LinkTx,
    router: Router,
    hosts: Vec<ModuleHost>,
    reconf: Option<Reconf<'s>>,
    settings: Option<tokio::sync::watch::Receiver<crate::live::LiveSettings>>,
    stopped: Option<UnboundedReceiver<Vec<ModuleId>>>,
    failures: UnboundedReceiver<ModuleFailure>,
    failures_tx: Option<UnboundedSender<ModuleFailure>>,
    inbound_rx: crate::queue::Receiver<Incoming>,
    // A message that ended a batch of received input, handled on the next
    // turn so it keeps its place after the input read before it.
    held_back: Option<Incoming>,
    captured: Option<&'s mut InputReceiver>,
    commands: Option<&'s mut UnboundedReceiver<OwnershipCmd>>,
    on_input: F,
    report_ownership: G,
    poll_status: H,
    // A machine driving a peer's screen is not one to restart for an update:
    // the user's hand is on this mouse. Every report passes through `tell`, so
    // the mark follows each change without a line at each call site.
    driving_mark: crate::update::quiet::DrivingMark,
    // And which peer: a call's voice follows the user to that screen.
    driven_mark: crate::presence::DrivenMark,
    peer: Option<PeerId>,
    nonce: u64,
    ticker: tokio::time::Interval,
    status_ticker: tokio::time::Interval,
    probe: InteractiveProbe,
    inject_latency: LatencyWindow,
    // The injector's own share of the figure above. What is left over is
    // queueing, and the two call for different fixes.
    inject_call: LatencyWindow,
    forward_latency: LatencyWindow,
    epoch: tokio::time::Instant,
    last_heard: Arc<AtomicU64>,
}

impl<'s, F, G, H> SessionLoop<'s, F, G, H>
where
    F: FnMut(cursio_protocol::InputEvent),
    G: FnMut(Ownership),
    H: FnMut() -> Option<cursio_protocol::NodeStatus>,
{
    async fn run(&mut self) -> Result<(), SessionError> {
        // When the current turn woke, so a stall with no owner still leaves a
        // mark. Without it a blocking call in a spot nobody thought to
        // instrument is invisible — which is how a 2.6 s freeze once reached
        // the user with a clean log. Set as each arm starts; see
        // `SLOW_TURN_WARN`.
        let mut working_since: Option<tokio::time::Instant> = None;
        let mut prefer_local = true;
        loop {
            if let Some(error) = self.router.error() {
                return Err(SessionError::Transport(TransportError::Other(error)));
            }
            if self.inbound_rx.stats().failed
                || self.captured.as_ref().is_some_and(|rx| rx.stats().failed)
            {
                return Err(SessionError::Transport(TransportError::Other(
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

            let turn = tokio::select! {
                // Ordered, not random. The branches are already written most-
                // to least- urgent, and an ownership change queued alongside
                // input has to be applied first: the same events mean
                // different things either side of it — forwarded to the peer,
                // or acting locally — and letting the race decide drops the
                // ones read too early. The timers above it fire on their own
                // schedule, so nothing starves.
                biased;

                result = self.outbox.finished() => {
                    Err(SessionError::Transport(result.err().unwrap_or(TransportError::Closed)))
                }
                _ = self.ticker.tick() => {
                    working_since = Some(tokio::time::Instant::now());
                    self.heartbeat().await
                }
                _ = self.status_ticker.tick() => {
                    working_since = Some(tokio::time::Instant::now());
                    self.status().await
                }
                Some(failure) = self.failures.recv() => {
                    tracing::warn!(module = failure.module, error = %failure.error, "module lost the link; ending session");
                    Err(SessionError::from(failure.error))
                }
                latest = settings_changed(&mut self.settings) => {
                    working_since = Some(tokio::time::Instant::now());
                    self.apply_settings(latest).await
                }
                Some(ids) = stopped_maybe(&mut self.stopped) => {
                    if let Some((r, hosts)) = self.reconfigurable() {
                        r.stopped(ids, hosts);
                    }
                    GO_ON
                }
                cmd = recv_maybe(&mut self.commands) => {
                    working_since = Some(tokio::time::Instant::now());
                    self.command(cmd).await
                }
                local = recv_captured(&mut self.captured),
                    if prefer_local || (self.inbound_rx.is_empty() && self.held_back.is_none()) => {
                    prefer_local = false;
                    working_since = Some(tokio::time::Instant::now());
                    self.captured_input(local).await
                }
                incoming = next_inbound(&mut self.held_back, &mut self.inbound_rx),
                    if !prefer_local || self.captured.as_ref().is_none_or(|rx| rx.is_empty()) => {
                    prefer_local = true;
                    working_since = Some(tokio::time::Instant::now());
                    self.inbound(incoming).await
                }
            };
            match turn? {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(()) => return Ok(()),
            }
        }
    }

    /// Bounded: a module wedged in a blocking call is left behind with a
    /// warning naming it, rather than holding up the reconnect. Each module's
    /// handle onto the link is closed either way, so the link closes when
    /// this session is done with it.
    async fn finish(self) {
        let Self {
            mut outbox,
            tx,
            hosts,
            mut report_ownership,
            mut driving_mark,
            mut driven_mark,
            ..
        } = self;
        driving_mark.set(false);
        driven_mark.set(false);
        report_ownership(Ownership {
            active: true,
            panic: false,
            driving: false,
            entry: None,
        });
        tracing::info!("session ending; shutting down modules");
        module_host::shut_down(hosts, module_host::MODULE_SHUTDOWN_GRACE).await;

        // The prioritized sender only *queues* on `send`, so a final
        // `Bye`/shutdown message may still be waiting. Flush it to the wire
        // before dropping `tx`, or the drainer task gets cancelled with the
        // goodbye still queued. No-op for a plain (synchronous) sender.
        outbox.finish(SESSION_DRAIN_DEADLINE).await;
        tx.close().await;
    }

    /// Report the role as it now stands.
    fn tell(&mut self, panic: bool, entry: Option<crate::session::Entry>) {
        let driving = self.role.driving();
        self.driving_mark.set(driving);
        self.driven_mark.set(driving);
        (self.report_ownership)(Ownership {
            active: self.role.active(),
            panic,
            driving,
            entry,
        });
    }

    /// Whether this side forwards its input on the link. A link that follows
    /// no settings always does, as it always has.
    fn sharing_input(&self) -> bool {
        self.reconf.as_ref().is_none_or(|r| r.input_on())
    }

    /// The settings this link follows and where a change starts and stops
    /// modules; `None` on a link that follows none.
    fn reconfigurable(&mut self) -> Option<(&mut Reconf<'s>, Hosts<'_>)> {
        let (r, failures) = (self.reconf.as_mut()?, self.failures_tx.as_ref()?);
        Some((
            r,
            Hosts {
                hosts: &mut self.hosts,
                router: &self.router,
                tx: &self.tx,
                failures,
            },
        ))
    }
}

/// The arms of the loop, most- to least- urgent, as `run` selects them.
impl<F, G, H> SessionLoop<'_, F, G, H>
where
    F: FnMut(cursio_protocol::InputEvent),
    G: FnMut(Ownership),
    H: FnMut() -> Option<cursio_protocol::NodeStatus>,
{
    async fn heartbeat(&mut self) -> Turn {
        self.inject_latency.report_if_due();
        self.inject_call.report_if_due();
        self.forward_latency.report_if_due();
        self.probe.report();
        // Has the peer gone quiet? Checked on the heartbeat tick, so detection
        // costs nothing extra and lands within one interval of the deadline.
        let heard_at = Duration::from_millis(self.last_heard.load(Ordering::Relaxed));
        let silent_for = self.epoch.elapsed().saturating_sub(heard_at);
        // A resumable link that lost its connection is waiting for another;
        // the silence is the gap, not the peer leaving. The link ends the
        // session itself if none comes back.
        if self.tx.holding() {
            return GO_ON;
        }
        if silent_for >= PEER_TIMEOUT {
            tracing::warn!(?silent_for, "peer stopped responding; ending session");
            return Err(SessionError::PeerTimeout(silent_for));
        }
        // Degrading, but not dead yet. Worth saying out loud: input and audio
        // both feel broken well before the link actually drops, and without
        // this the log looks perfectly healthy right up to the timeout —
        // leaving "it froze for a few seconds" with nothing to point at.
        if silent_for >= HEARTBEAT_INTERVAL * 2 {
            tracing::warn!(
                ?silent_for,
                "peer is slow to respond; input and audio may lag or stutter"
            );
        }
        self.nonce = self.nonce.wrapping_add(1) & !cursio_protocol::HEARTBEAT_ECHO;
        let nonce = self.nonce;
        let sent_at = std::time::Instant::now();
        self.outbox.send(&Message::Heartbeat { nonce }).await?;
        self.probe.sent(nonce, sent_at);
        tracing::trace!(nonce, "sent heartbeat");
        GO_ON
    }

    async fn status(&mut self) -> Turn {
        // `poll_status` reads the machine-wide monitor and answers `None`
        // unless something changed by the protocol's own standard — so a
        // plugged-in desktop sends nothing here for the life of the session.
        if let Some(status) = (self.poll_status)() {
            tracing::info!(%status, "local status changed; telling peer");
            self.outbox.send(&Message::NodeStatus(status)).await?;
        }
        GO_ON
    }

    async fn apply_settings(&mut self, latest: crate::live::LiveSettings) -> Turn {
        if self.failures_tx.is_none() {
            return GO_ON;
        }
        let Some(r) = self.reconf.as_mut() else {
            return GO_ON;
        };
        if r.peer_forgotten(&latest) {
            // Said rather than just dropped, so the peer ends its side as a
            // goodbye instead of timing out and raising a dropped link.
            // Teardown unmounts and stops modules as usual.
            tracing::info!("this machine was removed from the mesh; closing the link");
            if self.role.driving() {
                self.role.seize();
                self.tell(false, None);
            }
            let _ = self.outbox.send(&Message::Bye).await;
            return Ok(ControlFlow::Break(()));
        }
        let was_on = r.input_on();
        let arranged = r.outgoing(&latest);
        if let Some((msg, restart)) = r.local(&latest) {
            tracing::info!("settings changed; telling the peer");
            self.outbox.send(&msg).await?;
            if let Some((r, hosts)) = self.reconfigurable() {
                r.reconcile(&restart, false, hosts);
            }
        }
        for msg in arranged {
            tracing::info!("asking the peer to take its new place on the desk");
            self.outbox.send(&msg).await?;
        }
        if was_on && !self.sharing_input() && self.role.driving() {
            // Input switched off while it was driving the peer: bring the
            // cursor home, since nothing will be forwarded to drive it there
            // any more.
            self.role.seize();
            tracing::info!("input sharing turned off; the cursor is back on this machine");
            self.tell(false, None);
            self.outbox.send(&Message::Grab { active: false }).await?;
        }
        GO_ON
    }
}

/// The next batch of modules whose old instance finished stopping; pending
/// forever on a link that follows no settings.
async fn stopped_maybe(
    channel: &mut Option<UnboundedReceiver<Vec<ModuleId>>>,
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

/// The input arms: this machine's own hotkey and capture, and what the peer
/// sent.
impl<F, G, H> SessionLoop<'_, F, G, H>
where
    F: FnMut(cursio_protocol::InputEvent),
    G: FnMut(Ownership),
    H: FnMut() -> Option<cursio_protocol::NodeStatus>,
{
    async fn command(&mut self, cmd: Option<OwnershipCmd>) -> Turn {
        match cmd {
            // Hotkey: flip which machine is active. Works from EITHER side — if
            // we're active we hand the cursor over; if we're dormant we take
            // it. Either way we flip ourselves and tell the peer to take the
            // opposite state, so the "exactly one active" invariant holds. (The
            // dormant side can still run the hotkey because capture keeps
            // reading even while its input is grabbed.)
            // A peer that cannot hold the cursor cannot be handed it. Obeying
            // the hotkey here would grab this machine's input and stream it to
            // a phone that has nothing to inject it into — the machine stops
            // responding to its own keyboard, and the only thing that could
            // give control back is the peer that was never able to take it.
            Some(OwnershipCmd::Toggle | OwnershipCmd::HandoffToPeer { .. })
                if !self.sharing_input() =>
            {
                tracing::debug!("input sharing is off; the cursor stays on this machine");
            }
            Some(OwnershipCmd::Toggle) if !self.cursor.peer_takes_cursor => {
                tracing::info!("ignoring the ownership hotkey: this peer cannot hold the cursor");
            }
            Some(OwnershipCmd::Toggle) => {
                self.role.toggle();
                let am_active = self.role.active();
                tracing::info!(
                    am_active,
                    driving = self.role.driving(),
                    "hotkey toggled ownership"
                );
                self.tell(false, None);
                // Peer takes the opposite of our new state.
                self.outbox
                    .send(&Message::Grab { active: !am_active })
                    .await?;
            }
            // Edge-crossing handoff: only the active side hands off (the cursor
            // left *us*). If we're already dormant, ignore it — the crossing
            // belongs to the peer's screen, not ours. Same reasoning as the
            // hotkey: the cursor cannot cross an edge into a machine that has
            // no screen behind it.
            Some(OwnershipCmd::HandoffToPeer { edge, .. }) if !self.cursor.peer_takes_cursor => {
                tracing::trace!(?edge, "edge crossing ignored: peer cannot hold the cursor");
            }
            Some(OwnershipCmd::HandoffToPeer {
                edge,
                pos_norm,
                entry,
                ..
            }) => {
                // Only the machine holding the cursor can send it away.
                //
                // This used to accept an idle machine too, reasoning that one
                // which has handed nothing over is the one the user is sitting
                // at. True at the start of a session and false ever after: a
                // machine that gave the cursor back is also idle, and its own
                // pointer drifting into the edge then read as a second handoff
                // — leaving it convinced it drove a peer nobody was at, with no
                // way to get the cursor back. Ctrl+Alt+G is how an idle machine
                // takes control, on every platform.
                if self.role.active() {
                    self.role.hand_over();
                    tracing::info!(?edge, pos_norm, "cursor crossed peer edge; handing off");
                    self.tell(false, None);
                    // Peer becomes active, entering on its opposite edge.
                    let msg = Message::CrossEdge {
                        edge: edge.opposite(),
                        pos_norm,
                        entry,
                    };
                    self.outbox.send(&msg).await?;
                } else {
                    tracing::debug!(
                        ?edge,
                        driving = self.role.driving(),
                        "edge crossing while not holding the cursor; ignoring"
                    );
                }
            }
            // Emergency: unconditionally take control back here. Become active
            // (so local input acts locally again and nothing inbound injects),
            // tell the peer to stand down, and let the caller restore
            // grab/cursor via on_ownership. Best-effort on the peer notify —
            // the whole point is that our own side recovers even if the link
            // is dead.
            Some(OwnershipCmd::Panic) => {
                tracing::warn!("PANIC: forcing control back to this machine");
                self.role.panic();
                self.tell(true, None);
                let _ = self.outbox.send(&Message::Grab { active: false }).await;
            }
            None => self.commands = None,
        }
        GO_ON
    }

    async fn captured_input(&mut self, local: Option<crate::session::CapturedInput>) -> Turn {
        match local {
            // Forwarded only while actually driving the peer. The active side's
            // input already acts locally; so does the input of a machine that
            // has handed nothing over, which is what lets a second keyboard
            // stay usable on its own machine while the first drives across.
            Some(event) => {
                if self.role.driving() && self.sharing_input() {
                    forward_captured(
                        &mut self.outbox,
                        event,
                        self.captured.as_deref_mut(),
                        &mut self.forward_latency,
                    )
                    .await?;
                }
            }
            None => {
                if self.captured.as_ref().is_some_and(|rx| rx.stats().failed) {
                    return Err(SessionError::Transport(TransportError::Other(
                        "captured input queue overloaded".into(),
                    )));
                }
                self.captured = None;
            }
        }
        GO_ON
    }
}

/// What the peer sent the session itself.
impl<F, G, H> SessionLoop<'_, F, G, H>
where
    F: FnMut(cursio_protocol::InputEvent),
    G: FnMut(Ownership),
    H: FnMut() -> Option<cursio_protocol::NodeStatus>,
{
    async fn inbound(&mut self, incoming: Option<Incoming>) -> Turn {
        // Reader task ended; the link dropped.
        let Some((message, read_at)) = incoming else {
            return Err(SessionError::Transport(TransportError::Closed));
        };
        match message {
            Message::Heartbeat { nonce } => {
                tracing::trace!(nonce, "peer heartbeat");
                if nonce & cursio_protocol::HEARTBEAT_ECHO != 0 {
                    self.probe.echo(nonce, std::time::Instant::now());
                } else {
                    let echo = Message::Heartbeat {
                        nonce: nonce | cursio_protocol::HEARTBEAT_ECHO,
                    };
                    self.outbox.send(&echo).await?;
                }
            }
            // The peer's power or network changed. Nothing routes by it yet,
            // but the window shows it: only the greeting's reading used to be
            // kept, so a network a phone reported a moment after connecting
            // never appeared.
            Message::NodeStatus(status) => {
                tracing::info!(%status, "peer status changed");
                if let Some(peer) = self.peer {
                    crate::metrics::note_peer_status(peer, status);
                }
            }
            // Inject only while active (we hold the cursor).
            Message::Input(event) if self.role.active() => self.inject(event, read_at),
            Message::Input(_) => {}
            // A peer that cannot hold the cursor claiming it anyway.
            //
            // Nothing we ship sends this, so it means a peer that has
            // misdescribed itself or a build we don't know. Obeying it would
            // freeze this machine's own input with no way back, so it is
            // refused — and said out loud, because a phone whose taps do
            // nothing is otherwise a silent mystery.
            Message::Grab { active: false } if !self.cursor.peer_takes_cursor => {
                tracing::warn!(
                    "peer claimed the cursor but is not a machine that can hold one; ignoring"
                );
            }
            Message::Grab { active: false } if !self.sharing_input() => {
                tracing::info!(
                    "peer took the cursor, but input sharing is off here; this machine keeps its own"
                );
            }
            Message::Grab { active } => {
                // The peer is handing the cursor to us (active=true) or taking
                // it (active=false). Adopt the state it dictates; the sender
                // flips itself to the opposite at the same time, preserving the
                // "exactly one active" invariant.
                if active != self.role.active() {
                    self.role.adopt(active);
                    // Named for what it is: a Grab message, NOT motion. The old
                    // wording ("peer moved the cursor") sent me hunting through
                    // the motion path while diagnosing a frozen cursor.
                    tracing::info!(
                        am_active = self.role.active(),
                        "peer handed ownership over (Grab)"
                    );
                    self.tell(false, None);
                }
            }
            Message::CrossEdge {
                edge,
                pos_norm,
                entry,
            } => {
                // The peer's cursor crossed the edge toward us. Record that
                // peer even when we are already active: an idle second machine
                // can begin driving the screen that already holds the shared
                // cursor, and ignoring that crossing loses who must regain
                // control on the way back. That was the one-way mouse failure
                // after a successful first round trip.
                let was_active = self.role.active();
                self.role.take_over();
                if !was_active {
                    tracing::info!(?edge, pos_norm, ?entry, "cursor crossed in from peer");
                    self.tell(
                        false,
                        Some(crate::session::Entry {
                            edge,
                            pos_norm,
                            at: entry,
                        }),
                    );
                } else {
                    tracing::info!(?edge, pos_norm, "peer began driving the cursor held here");
                }
            }
            Message::Bye => {
                tracing::info!("peer sent Bye; closing session");
                return Err(SessionError::PeerLeft);
            }
            Message::Reconfigure(change) => {
                tracing::info!(
                    modules = change.modules.len(),
                    position = ?change.position,
                    restart = ?change.restart,
                    "peer changed its settings"
                );
                match self.reconfigurable() {
                    Some((r, hosts)) => {
                        let restart = r.peer(change);
                        r.reconcile(&restart, true, hosts);
                    }
                    None => {
                        tracing::debug!("this link follows no settings; keeping what it agreed")
                    }
                }
            }
            Message::Arrange(request) => match self.reconf.as_mut() {
                Some(r) => {
                    tracing::info!(?request, "the peer arranged this machine on the desk");
                    r.arrange(request);
                }
                None => tracing::debug!("this link follows no settings; arrangement ignored"),
            },
            // Module traffic never gets here — the reader routes it — so this
            // is something neither side's session speaks.
            other => {
                tracing::debug!(
                    message = module_host::message_kind(&other),
                    "ignoring unexpected in-session message"
                );
            }
        }
        GO_ON
    }

    fn inject(&mut self, event: cursio_protocol::InputEvent, read_at: std::time::Instant) {
        let batch = drain_received_input(event, read_at, &mut self.inbound_rx, &mut self.held_back);
        crate::update::quiet::note_input_activity();
        for (event, read_at) in batch {
            tracing::trace!(?event, "peer input");
            let started = std::time::Instant::now();
            (self.on_input)(event);
            self.inject_call.record(started.elapsed());
            self.inject_latency.record(read_at.elapsed());
        }
    }
}
