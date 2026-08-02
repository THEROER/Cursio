//! Session handshake and liveness.
//!
//! Once a [`Transport`] is connected (and, for a first meeting, pairing has
//! run), a session begins with a [`Hello`] exchange — each side advertises its
//! protocol version and identity, and a version mismatch is rejected up front.
//! After that the session runs a heartbeat loop so both ends detect a dead link
//! promptly, until either side sends [`Message::Bye`] or the link drops.

use std::time::Duration;

use cursio_protocol::{
    DeviceKind, Hello, Message, ModuleAdvert, Negotiated, PeerId, PROTOCOL_VERSION,
};
use cursio_transport::{Transport, TransportError};

/// Identity and capabilities learned from the peer's [`Hello`].
#[derive(Debug, Clone)]
pub struct PeerHello {
    pub peer_id: PeerId,
    pub display_name: String,
    /// What sort of machine the peer is. Consulted before handing it the shared
    /// cursor — a phone has no screen edge to enter.
    pub device_kind: DeviceKind,
    /// Modules the peer advertised. Raw; use [`PeerHello::negotiate`] to decide
    /// what actually runs.
    pub modules: Vec<ModuleAdvert>,
    /// Where the peer stands in the layout, which is what decides the edge the
    /// cursor crosses to reach it.
    pub position: cursio_protocol::Position,
    /// What the peer can carry for the mesh, and what quality it wants from a
    /// duty the mesh places. See [`cursio_protocol::ServiceOffer`].
    pub service: cursio_protocol::ServiceOffer,
}

impl PeerHello {
    /// Which of *our* modules run on this link, and why the rest don't.
    pub fn negotiate(&self, local: &[ModuleAdvert]) -> Negotiated {
        cursio_protocol::negotiate(local, &self.modules)
    }
}

/// Errors during the session handshake or run loop.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("protocol version mismatch: local {local}, peer {peer}")]
    VersionMismatch { local: u16, peer: u16 },
    #[error("expected Hello, got a different message")]
    UnexpectedMessage,
    #[error("peer ended the session")]
    PeerLeft,
    #[error("peer stopped responding (no traffic for {0:?})")]
    PeerTimeout(Duration),
}

/// How often to send a heartbeat while a session is idle.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// How often the loop wakes up to let modules flush, when nothing else does.
///
/// Sized just under the audio chunk cadence (~10 ms) so a steady stream is
/// pumped smoothly rather than in bursts. Without it the loop only woke on
/// inbound traffic or the 5 s heartbeat, which turned continuous audio into
/// one clump every five seconds.
const MODULE_PUMP_INTERVAL: Duration = Duration::from_millis(5);

/// Warn when a single module pump takes at least this long.
///
/// Input, audio and clipboard all share the session loop, so anything blocking
/// here is felt as a freeze of *everything* at once. Set well above a normal
/// pump (sub-millisecond) but below what a listener notices (~50 ms), so the
/// log names the culprit instead of leaving "it froze" to guesswork.
const SLOW_PUMP_WARN: Duration = Duration::from_millis(50);

/// How long to wait for *any* inbound traffic before declaring the peer gone.
///
/// A machine that loses power (or has its cable pulled) never sends a FIN, so
/// the link looks perfectly healthy until the OS TCP timeout fires — minutes
/// later. Until then every send queues into the void and the session appears
/// frozen. Noticing ourselves turns that into a few seconds.
///
/// Three heartbeats: one missed beat is ordinary jitter, three is gone.
const PEER_TIMEOUT: Duration = Duration::from_secs(15);

/// Perform the opening [`Hello`] exchange and return the peer's identity.
///
/// Both sides send their own `Hello` and read the other's; the order is
/// symmetric because each `send` is independent of the peer's. A protocol
/// version mismatch is a hard error so incompatible builds fail fast.
pub async fn handshake<T: Transport + ?Sized>(
    transport: &mut T,
    local: &crate::NodeIdentity,
    device_kind: DeviceKind,
    modules: &[ModuleAdvert],
    position: cursio_protocol::Position,
    service: cursio_protocol::ServiceOffer,
) -> Result<PeerHello, SessionError> {
    transport
        .send(&Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            peer_id: local.peer_id,
            display_name: local.display_name.clone(),
            device_kind,
            modules: modules.to_vec(),
            position,
            service,
        }))
        .await?;

    let hello = match transport.recv().await? {
        Message::Hello(h) => h,
        _ => return Err(SessionError::UnexpectedMessage),
    };

    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(SessionError::VersionMismatch {
            local: PROTOCOL_VERSION,
            peer: hello.protocol_version,
        });
    }

    Ok(PeerHello {
        peer_id: hello.peer_id,
        display_name: hello.display_name,
        device_kind: hello.device_kind,
        modules: hello.modules,
        position: hello.position,
        service: hello.service,
    })
}

/// What sort of machine *this* build runs on, as far as the cursor is concerned.
///
/// Desktop/laptop is not distinguishable without poking at hardware, and nothing
/// depends on the difference today — [`DeviceKind::takes_cursor`] treats them
/// alike.
///
/// A mobile build says so, and that one word decides how the whole session is
/// arranged: the peer keeps the cursor permanently, this side stays dormant,
/// and staying dormant is exactly what makes its input stream to the peer
/// rather than act locally. A phone that claimed to be a desktop would take the
/// cursor half the time and freeze the machine it was supposed to be driving.
#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn local_device_kind() -> DeviceKind {
    DeviceKind::Phone
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn local_device_kind() -> DeviceKind {
    DeviceKind::Desktop
}

/// A request to move the shared cursor to the other machine, delivered to a
/// running session from outside (e.g. a hotkey handler in the binary).
///
/// A request to change ownership, fed to [`run_session`] on its command channel.
///
/// [`Toggle`](OwnershipCmd::Toggle) is the symmetric hotkey handoff (flip
/// whichever side we're on). [`HandoffToPeer`](OwnershipCmd::HandoffToPeer) is
/// the *directional* edge-crossing handoff: it only makes sense from the active
/// side (the cursor left us across the peer-facing edge), and it carries the
/// crossing geometry so the peer knows where the cursor enters. The caller
/// (which owns the screen topology) is responsible for only emitting
/// `HandoffToPeer` for a crossing on the edge that actually borders the peer.
#[derive(Debug, Clone, Copy)]
pub enum OwnershipCmd {
    /// Flip which machine is active (hand the shared cursor to the other side).
    Toggle,
    /// Hand the cursor to the peer because it crossed the peer-facing edge.
    /// `edge` is *our* edge that was crossed; `pos_norm` (`0.0`..=`1.0`) is where
    /// along it. Ignored unless we're currently active.
    HandoffToPeer {
        edge: cursio_protocol::Edge,
        pos_norm: f32,
    },
    /// Emergency release: unconditionally take control back to THIS machine —
    /// become active, tell the peer to go dormant, and let the caller drop any
    /// grab/held keys and restore the cursor. The panic hotkey. Meant to work
    /// even if the peer is wedged, so its own grab release happens directly in
    /// the capture path (not only here); this variant handles the session-side
    /// cleanup and best-effort notifies the peer.
    Panic,
}

/// Who holds the shared cursor and who is driving whom.
///
/// Three booleans that have to move together, extracted because getting them
/// out of step is not a subtle bug: the machine the user is sitting at stops
/// reaching the other one, and the only way back is to shove the cursor by hand
/// from the far end. Every transition below is one line in a log somewhere, and
/// each was wrong at least once before the tests underneath pinned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRole {
    /// This machine holds the shared cursor.
    active: bool,
    /// This machine's own input is being streamed to the peer.
    driving: bool,
    /// We hold the cursor because the peer sent it, so the user is over there.
    peer_driving: bool,
}

impl CursorRole {
    /// The state a session starts in. Nobody has handed anything anywhere.
    pub fn new(start_active: bool) -> Self {
        Self {
            active: start_active,
            driving: false,
            peer_driving: false,
        }
    }

    pub fn active(self) -> bool {
        self.active
    }

    pub fn driving(self) -> bool {
        self.driving
    }

    /// Send the cursor to the peer, by the hotkey or across an edge.
    ///
    /// Whether that makes us the driver depends on how we came to hold it:
    /// returning what was lent leaves us idle, while sending our own cursor
    /// across means the user is here and our input follows it.
    pub fn hand_over(&mut self) {
        self.active = false;
        self.driving = !self.peer_driving;
        self.peer_driving = false;
    }

    /// Record that the peer sent its cursor here.
    ///
    /// The peer is driving — unless we were driving *it*, in which case this is
    /// the cursor we lent coming home and we are still the user's machine. This
    /// transition also matters when we already hold the shared cursor: an idle
    /// second machine may start driving us without changing which screen is
    /// active, and the next crossing must still return the cursor to that
    /// machine rather than turn both sides into drivers.
    pub fn take_over(&mut self) {
        self.active = true;
        self.peer_driving = !self.driving;
        self.driving = false;
    }

    /// The hotkey, which flips whichever side we are on.
    pub fn toggle(&mut self) {
        if self.active {
            self.hand_over();
        } else {
            self.take_over();
        }
    }

    /// Adopt the state the peer dictates with [`Message::Grab`].
    pub fn adopt(&mut self, active: bool) {
        if active {
            self.take_over();
        } else {
            // Told to stand down: the user is over there, so we hold nothing
            // and drive nobody.
            self.active = false;
            self.driving = false;
            self.peer_driving = false;
        }
    }

    /// Emergency: take everything back to this machine.
    pub fn panic(&mut self) {
        self.active = true;
        self.driving = false;
        self.peer_driving = false;
    }
}

/// How a session treats the shared cursor, decided before the loop starts.
///
/// Two machines with screens pass the cursor back and forth, and which one
/// starts with it is settled by comparing peer ids — deterministic, and each
/// side computes the opposite answer. But not every peer is such a machine: a
/// phone acting as a touchpad has no screen for the cursor to enter and no
/// injector to drive it ([`DeviceKind::takes_cursor`]), so there is nothing to
/// pass back and forth. It only ever *produces* input.
///
/// Handing the cursor to a peer that cannot hold it is not a cosmetic mistake.
/// The machine that gives it up grabs its own input and streams it away, so it
/// stops responding to its own keyboard and mouse — and the only thing that
/// could hand control back is the peer that was never able to take it. The
/// session would have to be killed to recover. That is why this is settled up
/// front and enforced in the loop, rather than left to the id comparison that
/// knows nothing about what a peer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSetup {
    /// Whether this machine begins holding the shared cursor.
    pub start_active: bool,
    /// Whether the peer can ever hold it at all.
    pub peer_takes_cursor: bool,
}

impl CursorSetup {
    /// Work out the arrangement for a link between these two machines.
    ///
    /// Both sides run this and reach complementary answers, which is what keeps
    /// "exactly one machine is active" true without negotiating it over the
    /// wire.
    pub fn negotiate(
        local_kind: DeviceKind,
        local_id: PeerId,
        peer_kind: DeviceKind,
        peer_id: PeerId,
    ) -> Self {
        let peer_takes_cursor = peer_kind.takes_cursor();
        let start_active = match (local_kind.takes_cursor(), peer_takes_cursor) {
            // The ordinary case: two machines with screens. Compare ids, not
            // who dialled whom — the latter flips when the same two machines
            // reconnect the other way round, landing the cursor somewhere
            // different each time.
            (true, true) => local_id < peer_id,
            // Only we can hold it, so we hold it, always.
            (true, false) => true,
            // We cannot hold it: this machine is the input source. Staying
            // dormant is exactly what makes its input stream to the peer.
            (false, _) => false,
        };
        CursorSetup {
            start_active,
            peer_takes_cursor,
        }
    }
}

/// Reports this machine's new **active** state after a change, so the caller can
/// match local input handling to it.
///
/// Exactly one machine is active at a time (a hard invariant). When
/// `active = true` this machine holds the shared cursor: local input acts
/// normally and inbound peer input is injected here. When `active = false` this
/// machine is dormant: its local input is grabbed and streamed to the (active)
/// peer, and nothing inbound is injected.
#[derive(Debug, Clone, Copy)]
pub struct Ownership {
    /// Whether this machine now holds the shared cursor.
    pub active: bool,
    /// Set when this change came from the emergency panic release, not a normal
    /// handoff. The caller should recover to a *safe idle* — ungrab, drop
    /// barriers entirely, show the cursor — rather than the usual active setup
    /// (which on Wayland would re-arm barriers). Normal handoffs leave this
    /// `false`.
    pub panic: bool,
    /// Whether this machine is streaming its input to the peer.
    ///
    /// Not the opposite of [`active`](Self::active), and that is the whole
    /// point. A machine drives the peer only when the user *at this machine*
    /// sent the cursor across — by the hotkey or by shoving it off the peer
    /// edge. A machine that has handed nothing over is one the user may not
    /// even be sitting at, and grabbing its keyboard because it does not hold
    /// the shared cursor is what made a Mac's trackpad drive a Windows cursor
    /// from the moment the link came up, while the Mac's own pointer never
    /// moved again.
    ///
    /// Never set by something the peer said: taking the cursor back tells us to
    /// stop driving, never to start.
    pub driving: bool,
}

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
/// - `modules` — subsystems multiplexed onto the same channel (audio today,
///   window transfer next). They are started before the loop, pumped once per
///   iteration, and offered any inbound message this loop doesn't handle
///   itself. Input deliberately stays hard-wired here rather than becoming a
///   module: its ownership/grab/edge logic is intertwined with the loop state
///   above, and it already works.
///
/// Takes the transport **by value** (`Box<dyn Transport>`) so it can
/// [`split`](Transport::split) it: the receive half runs in its own task and is
/// therefore never cancelled. That matters because `recv` is not cancel-safe
/// for the encrypted transport — cancelling it mid-await (which a plain
/// `select!` over `recv` does whenever a send wins) can desynchronise the Noise
/// cipher and silently drop all later inbound messages.
#[allow(clippy::too_many_arguments)]
pub async fn run_session<F, G>(
    transport: Box<dyn Transport>,
    cursor: CursorSetup,
    // Borrowed, not owned: the capture that feeds these outlives any single
    // session (it holds a portal permission that must not be re-acquired per
    // connection), so a session reads from them and hands them back.
    mut captured: Option<&mut tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::InputEvent>>,
    mut on_input: F,
    mut commands: Option<&mut tokio::sync::mpsc::UnboundedReceiver<OwnershipCmd>>,
    mut on_ownership: G,
    mut modules: Vec<Box<dyn crate::module::SessionModule>>,
) -> Result<(), SessionError>
where
    F: FnMut(cursio_protocol::InputEvent),
    G: FnMut(Ownership),
{
    tracing::debug!("splitting transport");
    // Prioritized split: the tx half queues frames by priority and a background
    // drainer writes them, so a bulk transfer (clipboard image, file) can never
    // stall input on the shared channel. Falls back to a plain split for
    // transports that don't override it.
    let (mut tx, mut rx) = transport.split_prioritized();
    tracing::debug!("transport split; spawning reader");

    // Reader task: owns the receive half and never gets cancelled, so recv
    // state stays consistent. It forwards every decoded message to the main
    // task over a channel.
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        tracing::debug!("session reader task started");
        // On any recv error the loop ends and `inbound_tx` drops, which the
        // main task observes as the channel closing (a dropped link).
        while let Ok(msg) = rx.recv().await {
            tracing::trace!("reader got a message");
            if inbound_tx.send(msg).is_err() {
                break; // main task gone
            }
        }
    });

    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    let mut nonce: u64 = 0;

    // Paces the loop for modules that produce a steady stream of their own.
    //
    // Modules are pumped at the top of the loop, then the `select!` below parks
    // until *something* wakes it. With no inbound traffic the only wakeup was
    // the 5 s heartbeat — so audio was pumped in bursts five seconds apart,
    // queueing in between and arriving as a clump. Observed exactly that:
    // alternating "stalled ~4.5 s" / "resumed ~5.0 s" lines, with the jitter
    // buffer overflowing on every burst.
    //
    // A short tick keeps the loop turning at roughly the audio chunk rate, so
    // pumping stays smooth. It costs nothing when idle: the modules simply have
    // nothing to send.
    let mut pace = tokio::time::interval(MODULE_PUMP_INTERVAL);
    // If a tick is missed (a long pump, a slow send), fire once and carry on
    // rather than replaying the backlog as a burst — bursts are the very thing
    // this is here to prevent.
    pace.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Liveness: when did we last hear *anything* from the peer? Any inbound
    // message counts, not just heartbeats — real traffic proves the link just
    // as well, so a busy session never trips the timeout.
    let mut last_seen = tokio::time::Instant::now();

    // Ownership: is THIS machine the active one (holds the shared cursor)?
    // Peers start in opposite states so exactly one is active from the outset.
    let mut role = CursorRole::new(cursor.start_active);
    let mut am_active = role.active();
    on_ownership(Ownership {
        active: am_active,
        panic: false,
        driving: role.driving(),
    });
    tracing::info!(am_active, "initial ownership");

    // Bring each subsystem up on the shared channel before the loop starts.
    for m in modules.iter_mut() {
        tracing::info!(module = m.name(), "starting session module");
        if let Err(e) = m.start(tx.as_mut()).await {
            return Err(SessionError::from(e));
        }
    }

    let result = loop {
        // Let each module flush its outbound work. Modules send a bounded
        // amount per call (see `SessionModule::pump`): draining fully here
        // would starve the inbound branch below, which is exactly how a peer's
        // audio advertisements once got silently ignored.
        let mut pump_err = None;
        for m in modules.iter_mut() {
            // Time each pump: everything (input, audio, clipboard) shares this
            // one loop, so a module that blocks here freezes all of them. A
            // threshold log turns "it froze" into "this module took N ms".
            let started = tokio::time::Instant::now();
            let result = m.pump(tx.as_mut()).await;
            let took = started.elapsed();
            if took >= SLOW_PUMP_WARN {
                tracing::warn!(
                    module = m.name(),
                    took_ms = took.as_millis() as u64,
                    "module pump blocked the session loop"
                );
            }
            if let Err(e) = result {
                pump_err = Some((m.name(), e));
                break;
            }
        }
        if let Some((name, e)) = pump_err {
            tracing::warn!(module = name, error = %e, "module pump failed");
            break Err(SessionError::from(e));
        }

        // NOTE: deliberately *not* `biased!` here. The inbound branch is written
        // last, so biased polling would starve it — the opposite of what the
        // audio path needs. Module sending happens above, outside the
        // `select!`, so a busy module can't crowd out inbound messages anyway.
        tokio::select! {
            // Ordered, not random. The branches are already written most- to
            // least- urgent, and an ownership change queued alongside input has
            // to be applied first: the same events mean different things either
            // side of it — forwarded to the peer, or acting locally — and
            // letting the race decide drops the ones read too early. The timers
            // above it fire on their own schedule, so nothing starves.
            biased;

            // Just a wakeup: the work happens in the pump phase above, so this
            // arm intentionally does nothing.
            _ = pace.tick() => {}
            _ = ticker.tick() => {
                // Has the peer gone quiet? Checked on the heartbeat tick, so
                // detection costs nothing extra and lands within one interval
                // of the deadline.
                let silent_for = last_seen.elapsed();
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
                nonce = nonce.wrapping_add(1);
                if let Err(e) = tx.send(&Message::Heartbeat { nonce }).await {
                    break Err(SessionError::from(e));
                }
                tracing::trace!(nonce, "sent heartbeat");
            }
            cmd = recv_maybe(&mut commands) => {
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
                    Some(OwnershipCmd::Toggle) if !cursor.peer_takes_cursor => {
                        tracing::info!(
                            "ignoring the ownership hotkey: this peer cannot hold the cursor"
                        );
                    }
                    Some(OwnershipCmd::Toggle) => {
                        role.toggle();
                        am_active = role.active();
                        tracing::info!(am_active, driving = role.driving(), "hotkey toggled ownership");
                        on_ownership(Ownership { active: am_active, panic: false, driving: role.driving() });
                        // Peer takes the opposite of our new state.
                        if let Err(e) = tx.send(&Message::Grab { active: !am_active }).await {
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
                        // Honoured unless we are already driving the peer. Being
                        // inactive is not enough to refuse: a machine that has
                        // handed nothing over is exactly the one the user is
                        // sitting at, and shoving its cursor at the peer's edge
                        // is how they say so.
                        if !role.driving() {
                            role.hand_over();
                            am_active = role.active();
                            tracing::info!(?edge, pos_norm, "cursor crossed peer edge; handing off");
                            on_ownership(Ownership { active: am_active, panic: false, driving: role.driving() });
                            // Peer becomes active, entering on its opposite edge.
                            let msg = Message::CrossEdge { edge: edge.opposite(), pos_norm };
                            if let Err(e) = tx.send(&msg).await {
                                break Err(SessionError::from(e));
                            }
                        } else {
                            tracing::trace!(?edge, "edge crossing while already driving; ignoring");
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
                        on_ownership(Ownership { active: am_active, panic: true, driving: role.driving() });
                        let _ = tx.send(&Message::Grab { active: false }).await;
                    }
                    None => commands = None,
                }
            }
            local = recv_maybe(&mut captured) => {
                match local {
                    // Forwarded only while actually driving the peer. The
                    // active side's input already acts locally; so does the
                    // input of a machine that has handed nothing over, which is
                    // what lets a second keyboard stay usable on its own
                    // machine while the first drives across.
                    Some(event) => {
                        if role.driving() {
                            // `captured` is an Option<&mut _>, so reborrow through
                            // it rather than handing over an &mut &mut.
                            if let Err(e) =
                                forward_captured(tx.as_mut(), event, captured.as_deref_mut()).await
                            {
                                break Err(e);
                            }
                        }
                    }
                    None => captured = None,
                }
            }
            incoming = inbound_rx.recv() => {
                // Anything at all from the peer means it's still there.
                if incoming.is_some() {
                    last_seen = tokio::time::Instant::now();
                }
                match incoming {
                    Some(Message::Heartbeat { nonce }) => tracing::trace!(nonce, "peer heartbeat"),
                    Some(Message::Input(event)) => {
                        // Inject only while active (we hold the cursor).
                        if am_active {
                            tracing::trace!(?event, "peer input");
                            on_input(event);
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
                            on_ownership(Ownership { active: am_active, panic: false, driving: role.driving() });
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
                            on_ownership(Ownership { active: am_active, panic: false, driving: role.driving() });
                        } else {
                            tracing::info!(?edge, pos_norm, "peer began driving the cursor held here");
                        }
                    }
                    Some(Message::Bye) => {
                        tracing::info!("peer sent Bye; closing session");
                        break Err(SessionError::PeerLeft);
                    }
                    // Not a message the session handles itself: offer it to the
                    // modules, first claim wins. This is the multiplexing point
                    // — audio chunks and advertisements land here.
                    Some(other) => {
                        let claimed = modules.iter_mut().any(|m| m.handle_message(&other));
                        if !claimed {
                            tracing::debug!(?other, "ignoring unexpected in-session message");
                        }
                    }
                    None => {
                        // Reader task ended; the link dropped.
                        break Err(SessionError::Transport(TransportError::Closed));
                    }
                }
            }
        }
    };

    // TEMP hang-diagnosis tracing (2026-07-21): after a link drop the Windows
    // session stopped progressing entirely. These lines pin which teardown step
    // blocks. Remove once the hang is fixed.
    tracing::info!("session ending; shutting down modules");
    // Let each module tear down (best-effort — the link may already be dead,
    // which is why modules must not treat send errors here as fatal).
    for m in modules.iter_mut() {
        tracing::info!(module = m.name(), "module shutdown begin");
        m.shutdown(tx.as_mut()).await;
        tracing::info!(module = m.name(), "module shutdown done");
    }

    // The prioritized sender only *queues* on `send`, so a final `Bye`/shutdown
    // message may still be waiting. Flush it to the wire before dropping `tx`,
    // or the drainer task gets cancelled with the goodbye still queued. No-op
    // for a plain (synchronous) sender.
    tracing::info!("flushing tx");
    tx.flush().await;
    tracing::info!("tx flushed; aborting reader");

    reader.abort();
    tracing::info!("session teardown complete; returning (modules drop next)");
    result
}

/// Forward one just-received captured event to the peer, **coalescing pointer
/// motion** to keep latency low under a flood of events.
///
/// A fast mouse move — or dragging a window — produces hundreds of tiny
/// `PointerMotion` events per second (and evdev even splits X and Y into
/// separate ones). Sending each as its own Noise frame lets an unbounded queue
/// build up faster than the link drains it, so latency grows without bound.
///
/// Instead, we drain everything already waiting in the capture channel
/// (`try_recv`, non-blocking) and merge consecutive motion into a single event
/// (summing `dx`/`dy`, which is exact for relative deltas). Any non-motion event
/// (button, key, scroll) first flushes the accumulated motion so ordering is
/// preserved, then is sent as-is. The net effect: one motion frame per drain
/// instead of hundreds, with identical resulting cursor movement.
async fn forward_captured(
    tx: &mut dyn cursio_transport::TransportTx,
    first: cursio_protocol::InputEvent,
    captured: Option<&mut tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::InputEvent>>,
) -> Result<(), SessionError> {
    use cursio_protocol::InputEvent;
    use tokio::sync::mpsc::error::TryRecvError;

    // Accumulated relative motion not yet sent.
    let mut acc_dx: i32 = 0;
    let mut acc_dy: i32 = 0;

    // Send whatever motion has accumulated (if any) as one event.
    async fn flush(
        tx: &mut dyn cursio_transport::TransportTx,
        dx: &mut i32,
        dy: &mut i32,
    ) -> Result<(), SessionError> {
        if *dx != 0 || *dy != 0 {
            tx.send(&Message::Input(InputEvent::PointerMotion {
                dx: *dx,
                dy: *dy,
            }))
            .await?;
            *dx = 0;
            *dy = 0;
        }
        Ok(())
    }

    // Handle one event: fold motion into the accumulator, or flush + send.
    async fn handle(
        tx: &mut dyn cursio_transport::TransportTx,
        event: InputEvent,
        dx: &mut i32,
        dy: &mut i32,
    ) -> Result<(), SessionError> {
        match event {
            InputEvent::PointerMotion { dx: ex, dy: ey } => {
                *dx += ex;
                *dy += ey;
                Ok(())
            }
            other => {
                // Preserve order: pending motion goes out before this event.
                //
                // Logged because motion is coalesced and non-motion is not: if a
                // button release were ever dropped or reordered here, the peer
                // would be left holding a button down — which looks exactly like
                // "the cursor stopped moving after I clicked".
                if matches!(other, InputEvent::PointerButton { .. }) {
                    tracing::debug!(event = ?other, pending_dx = *dx, pending_dy = *dy,
                        "forwarding pointer button to peer");
                }
                flush(tx, dx, dy).await?;
                tx.send(&Message::Input(other))
                    .await
                    .map_err(SessionError::from)
            }
        }
    }

    handle(tx, first, &mut acc_dx, &mut acc_dy).await?;

    // Drain everything already queued without blocking, coalescing as we go.
    if let Some(rx) = captured {
        loop {
            match rx.try_recv() {
                Ok(event) => handle(tx, event, &mut acc_dx, &mut acc_dy).await?,
                Err(TryRecvError::Empty) => break,
                // Sender gone: send what we have, then stop draining. The
                // caller's next recv sees the closed channel and clears it.
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    // Send any leftover coalesced motion.
    flush(tx, &mut acc_dx, &mut acc_dy).await?;
    Ok(())
}

/// Await the next item from an optional channel, or stay pending forever if
/// there's no channel — so the `select!` arm is inert rather than busy-looping
/// on `None`. Used for both the capture and the ownership-command channels.
async fn recv_maybe<T>(
    channel: &mut Option<&mut tokio::sync::mpsc::UnboundedReceiver<T>>,
) -> Option<T> {
    match channel {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
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
    run_session(transport, cursor, None, |_| {}, None, |_| {}, Vec::new()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> PeerId {
        PeerId::from_bytes([byte; 16])
    }

    /// Two machines with screens: the lower id starts with the cursor, and each
    /// side reaches the opposite answer without asking the other.
    ///
    /// This is the invariant the whole ownership model rests on — two actives
    /// means both machines inject each other's input, which is the feedback
    /// loop the design exists to make impossible.
    #[test]
    fn two_desktops_agree_on_exactly_one_holder() {
        let low = id(1);
        let high = id(2);
        let a = CursorSetup::negotiate(DeviceKind::Desktop, low, DeviceKind::Laptop, high);
        let b = CursorSetup::negotiate(DeviceKind::Laptop, high, DeviceKind::Desktop, low);
        assert!(a.start_active);
        assert!(!b.start_active);
        assert!(a.peer_takes_cursor && b.peer_takes_cursor);
    }

    /// A phone peer never gets the cursor, whichever way the ids fall.
    ///
    /// The ids alone decided this before, so half of all phone pairings put the
    /// desktop into the dormant state: it grabs its own keyboard and mouse,
    /// streams them to a device with nothing to inject them into, and the only
    /// thing that could hand control back is that same device. The user's
    /// machine stops responding and the session has to be killed. Whichever id
    /// the phone happens to have must not be able to cause that.
    #[test]
    fn a_phone_peer_never_takes_the_cursor() {
        for phone_id in [id(0), id(255)] {
            let setup =
                CursorSetup::negotiate(DeviceKind::Desktop, id(128), DeviceKind::Phone, phone_id);
            assert!(
                setup.start_active,
                "the desktop must hold the cursor against a phone with id {phone_id:?}"
            );
            assert!(!setup.peer_takes_cursor);
        }
    }

    /// The phone's own view: dormant, which is precisely what makes its input
    /// stream to the peer rather than act locally.
    #[test]
    fn a_phone_stays_dormant_so_its_input_is_forwarded() {
        let setup = CursorSetup::negotiate(DeviceKind::Phone, id(1), DeviceKind::Desktop, id(2));
        assert!(!setup.start_active);
        assert!(setup.peer_takes_cursor, "the desktop can hold the cursor");

        // And the two sides still agree: the desktop holds it.
        let desktop = CursorSetup::negotiate(DeviceKind::Desktop, id(2), DeviceKind::Phone, id(1));
        assert!(desktop.start_active);
    }

    /// A headless box has no screen either, so it is treated like the phone.
    #[test]
    fn a_headless_peer_is_not_a_cursor_destination() {
        let setup = CursorSetup::negotiate(DeviceKind::Desktop, id(9), DeviceKind::Headless, id(1));
        assert!(setup.start_active);
        assert!(!setup.peer_takes_cursor);
    }
}

#[cfg(test)]
mod cursor_role_tests {
    use super::CursorRole;

    /// Two machines, one user, replayed as the pair of state machines they
    /// really are. Written because the alternative — reading it back off two
    /// logs side by side — is how three separate versions of this shipped
    /// broken.
    struct Pair {
        /// Where the user is sitting.
        here: CursorRole,
        there: CursorRole,
    }

    impl Pair {
        fn new() -> Self {
            Self {
                here: CursorRole::new(true),
                there: CursorRole::new(false),
            }
        }

        /// The cursor leaves `here` across the shared edge.
        fn push_across(&mut self) {
            self.here.hand_over();
            self.there.take_over();
        }

        /// And back the other way.
        fn push_back(&mut self) {
            self.there.hand_over();
            self.here.take_over();
        }
    }

    /// Nobody streams anything until a cursor has actually been sent
    /// somewhere. Both keyboards work on their own machines.
    #[test]
    fn a_fresh_link_leaves_both_keyboards_at_home() {
        let pair = Pair::new();
        assert!(!pair.here.driving());
        assert!(!pair.there.driving());
        assert!(pair.here.active());
        assert!(!pair.there.active());
    }

    /// Sending the cursor across makes this machine the driver: the user is
    /// here, so this keyboard is what should reach the other one.
    #[test]
    fn sending_the_cursor_across_starts_driving() {
        let mut pair = Pair::new();
        pair.push_across();
        assert!(pair.here.driving(), "the user's machine drives");
        assert!(pair.there.active(), "the far machine holds the cursor");
        assert!(!pair.there.driving(), "and drives nobody");
    }

    /// The failure the whole type exists for. Pushing the cursor back must
    /// return the *user's* machine to driving on the next crossing — getting it
    /// backwards left that machine idle, its hooks down, unable to reach the
    /// peer until the cursor was shoved home by hand from the far end.
    #[test]
    fn the_cursor_can_be_sent_across_twice_in_a_row() {
        let mut pair = Pair::new();
        pair.push_across();
        pair.push_back();
        assert!(pair.here.active(), "the cursor came home");
        assert!(!pair.here.driving(), "and nothing is being driven");
        assert!(!pair.there.driving(), "least of all by the empty machine");

        // The second crossing is the one that used to break.
        pair.push_across();
        assert!(pair.here.driving(), "the user's machine drives again");
        assert!(pair.there.active());
    }

    /// Peer ids choose an initial holder, not which physical mouse the user is
    /// allowed to use. The initially idle machine may send its own cursor into
    /// the already-active screen; that screen must remember who is driving so
    /// the first crossing back returns control instead of starting a second
    /// driver.
    #[test]
    fn the_initially_idle_machine_can_drive_and_get_control_back() {
        let mut holder = CursorRole::new(true);
        let mut source = CursorRole::new(false);

        source.hand_over();
        holder.take_over();
        assert!(holder.active());
        assert!(source.driving());

        holder.hand_over();
        source.take_over();
        assert!(source.active(), "the source got its cursor back");
        assert!(!source.driving(), "the source no longer streams input");
        assert!(
            !holder.driving(),
            "the holder did not become a second driver"
        );
    }

    /// Ten round trips, because a state machine that survives one and not five
    /// is a state machine with a flag it forgot to clear.
    #[test]
    fn many_round_trips_never_leave_both_or_neither_in_charge() {
        let mut pair = Pair::new();
        for trip in 0..10 {
            pair.push_across();
            assert!(pair.here.driving(), "trip {trip}: nobody driving outbound");
            assert!(!pair.there.driving(), "trip {trip}: both driving");
            pair.push_back();
            assert!(!pair.here.driving(), "trip {trip}: still driving inbound");
            assert!(!pair.there.driving(), "trip {trip}: far side left driving");
            assert!(pair.here.active(), "trip {trip}: cursor did not come home");
        }
    }

    /// Exactly one machine holds the cursor at every point, which is the
    /// invariant an input feedback loop violates.
    #[test]
    fn exactly_one_machine_holds_the_cursor() {
        let mut pair = Pair::new();
        for step in 0..6 {
            assert_ne!(
                pair.here.active(),
                pair.there.active(),
                "step {step}: both or neither hold it"
            );
            if step % 2 == 0 {
                pair.push_across();
            } else {
                pair.push_back();
            }
        }
    }

    /// The hotkey has to work from either side and leave the same state a
    /// crossing would.
    #[test]
    fn the_hotkey_matches_what_an_edge_crossing_does() {
        let mut hotkey = Pair::new();
        hotkey.here.toggle();
        hotkey.there.adopt(true);

        let mut edge = Pair::new();
        edge.push_across();

        assert_eq!(hotkey.here.driving(), edge.here.driving());
        assert_eq!(hotkey.here.active(), edge.here.active());
        assert_eq!(hotkey.there.active(), edge.there.active());
    }

    /// Being told to stand down is never a reason to start driving: the peer
    /// saying so means the user is over there.
    #[test]
    fn standing_down_never_starts_driving() {
        let mut role = CursorRole::new(true);
        role.adopt(false);
        assert!(!role.active());
        assert!(!role.driving());
    }

    /// The panic hotkey exists for a wedged link, so it must reach a state this
    /// machine can be used from whatever it was doing.
    #[test]
    fn panic_returns_everything_to_this_machine() {
        let mut role = CursorRole::new(false);
        role.hand_over();
        assert!(role.driving());
        role.panic();
        assert!(role.active());
        assert!(!role.driving());
    }
}
