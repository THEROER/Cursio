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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cursio_transport::{Transport, TransportError, TransportTx};

use crate::module_host::{self, LinkTx, ModuleHost};

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
    /// The peer's power and network state as of the greeting. Superseded by
    /// every [`Message::NodeStatus`] the session then reports.
    pub status: cursio_protocol::NodeStatus,
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

/// Warn when one turn of the session loop takes at least this long.
///
/// The loop now holds only input, ownership, the heartbeat and node status, so
/// a slow turn here is exactly a stall the cursor felt — no module can cause
/// one any more, which is what makes the warning worth reading when it fires.
/// Comfortably above the heartbeat's own work, so healthy idling never trips
/// it.
const SLOW_TURN_WARN: Duration = Duration::from_millis(250);

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
    status: cursio_protocol::NodeStatus,
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
            status,
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
        status: hello.status,
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

/// Which machine holds the shared cursor.
///
/// There is one cursor and every machine can move it, so there are two states
/// and not three: the machine holding it acts on itself, and every other
/// machine grabs its own input and streams it to the holder. Whichever mouse
/// the user reaches for drives the cursor, wherever it happens to be.
///
/// A third state was tried — "holds nothing and drives nobody", entered by a
/// machine that had handed the cursor back — and it is what made a mouse stop
/// working: that machine's pointer went local, so one mouse drove both screens
/// and the other drove neither. Which machine the user is sitting at cannot be
/// known here, and does not need to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRole {
    /// This machine holds the shared cursor.
    active: bool,
}

impl CursorRole {
    /// The state a session starts in. Nobody has handed anything anywhere.
    pub fn new(start_active: bool) -> Self {
        Self {
            active: start_active,
        }
    }

    pub fn active(self) -> bool {
        self.active
    }

    /// Whether this machine's own input is streamed to the peer.
    ///
    /// Exactly "does somebody else hold the cursor". A machine that is not
    /// holding it is the one whose mouse and keyboard have somewhere else to
    /// go.
    pub fn driving(self) -> bool {
        !self.active
    }

    /// Send the cursor to the peer, by the hotkey or across an edge.
    ///
    /// Refused unless we hold it: you cannot send away what you do not have,
    /// and without the check a pointer merely drifting into the edge of an
    /// unheld screen would drag the cursor off the screen the user is actually
    /// looking at.
    pub fn hand_over(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
    }

    /// Record that the peer sent its cursor here.
    pub fn take_over(&mut self) {
        self.active = true;
    }

    /// Take the cursor because the user asked for it here (the hotkey).
    ///
    /// Same destination as [`Self::take_over`], kept separate because the two
    /// read differently at the call sites and one of them is allowed from a
    /// machine that holds nothing.
    pub fn seize(&mut self) {
        self.active = true;
    }

    /// The hotkey, which flips whichever side we are on.
    pub fn toggle(&mut self) {
        if self.active {
            self.hand_over();
        } else {
            self.seize();
        }
    }

    /// Adopt the state the peer dictates with [`Message::Grab`].
    pub fn adopt(&mut self, active: bool) {
        self.active = active;
    }

    /// Emergency: take everything back to this machine.
    pub fn panic(&mut self) {
        self.active = true;
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

/// One locally captured input event, stamped when it was handed to sessions.
///
/// The stamp is what turns "the mouse lags" into a number on the sending side:
/// how long an event waited between capture handing it over and this session
/// giving it to the transport. The transport measures the rest of the way to
/// the socket itself.
#[derive(Debug, Clone, Copy)]
pub struct CapturedInput {
    pub event: cursio_protocol::InputEvent,
    pub at: std::time::Instant,
}

impl From<cursio_protocol::InputEvent> for CapturedInput {
    /// Stamped now: converting is the moment it is handed over.
    fn from(event: cursio_protocol::InputEvent) -> Self {
        Self {
            event,
            at: std::time::Instant::now(),
        }
    }
}

/// How often the input latency figures are logged, when there were any.
const INPUT_LATENCY_WINDOW: Duration = Duration::from_secs(30);

/// A p99 at or above this is logged at `warn`: a pointer that lags by this
/// much in one frame out of a hundred is visibly trailing the hand.
const INPUT_LATENCY_WARN: Duration = Duration::from_millis(20);

/// Samples kept per window. Beyond this the oldest are overwritten, which
/// keeps a gaming mouse's thousand events a second from growing memory while
/// still leaving thousands of samples behind each percentile.
const INPUT_LATENCY_SAMPLES: usize = 8192;

/// Input latency over one window, reported as percentiles.
///
/// Two of these per session, one per direction: from the reader taking an
/// `Input` off the wire to injecting it, and from capture handing an event
/// over to the transport accepting it. Both measure exactly the stretch the
/// session loop controls, which is where every lag this loop ever caused
/// lived.
pub(crate) struct LatencyWindow {
    what: &'static str,
    samples: Vec<Duration>,
    next: usize,
    total: u64,
    started: tokio::time::Instant,
}

impl LatencyWindow {
    pub(crate) fn new(what: &'static str) -> Self {
        Self {
            what,
            samples: Vec::new(),
            next: 0,
            total: 0,
            started: tokio::time::Instant::now(),
        }
    }

    pub(crate) fn record(&mut self, latency: Duration) {
        if self.samples.len() < INPUT_LATENCY_SAMPLES {
            self.samples.push(latency);
        } else {
            self.samples[self.next] = latency;
            self.next = (self.next + 1) % INPUT_LATENCY_SAMPLES;
        }
        self.total += 1;
    }

    /// `(p50, p99, max)` of the samples held, or `None` with none.
    pub(crate) fn percentiles(&self) -> Option<(Duration, Duration, Duration)> {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let last = *sorted.last()?;
        let at = |fraction: f64| {
            let idx = ((sorted.len() as f64 * fraction).ceil() as usize).clamp(1, sorted.len());
            sorted[idx - 1]
        };
        Some((at(0.50), at(0.99), last))
    }

    /// Log and restart the window once it has run its length. A window with
    /// no input says nothing and logs nothing, so an idle link stays quiet.
    fn report_if_due(&mut self) {
        if self.started.elapsed() < INPUT_LATENCY_WINDOW {
            return;
        }
        if let Some((p50, p99, max)) = self.percentiles() {
            let (p50_us, p99_us, max_us) = (
                p50.as_micros() as u64,
                p99.as_micros() as u64,
                max.as_micros() as u64,
            );
            if p99 >= INPUT_LATENCY_WARN {
                tracing::warn!(
                    path = self.what,
                    events = self.total,
                    p50_us,
                    p99_us,
                    max_us,
                    "input latency is high enough to feel"
                );
            } else {
                tracing::info!(
                    path = self.what,
                    events = self.total,
                    p50_us,
                    p99_us,
                    max_us,
                    "input latency"
                );
            }
        }
        *self = Self::new(self.what);
    }
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
    // Borrowed, not owned: the capture that feeds these outlives any single
    // session (it holds a portal permission that must not be re-acquired per
    // connection), so a session reads from them and hands them back.
    mut captured: Option<&mut tokio::sync::mpsc::UnboundedReceiver<CapturedInput>>,
    mut on_input: F,
    mut commands: Option<&mut tokio::sync::mpsc::UnboundedReceiver<OwnershipCmd>>,
    mut on_ownership: G,
    modules: Vec<Box<dyn crate::module::SessionModule>>,
    mut poll_status: H,
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
    let mut tx = LinkTx::new(tx);

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
    drop(failures_tx);

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
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let reader_heard = last_heard.clone();
    let reader = tokio::spawn(async move {
        tracing::debug!("session reader task started");
        // On any recv error the loop ends and `inbound_tx` drops, which the
        // main task observes as the channel closing (a dropped link).
        while let Ok(msg) = rx.recv().await {
            reader_heard.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
            if let Some(msg) = router.route(msg) {
                if inbound_tx.send((msg, std::time::Instant::now())).is_err() {
                    break; // main task gone
                }
            }
        }
    });

    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    let mut nonce: u64 = 0;

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
    let mut role = CursorRole::new(cursor.start_active);
    let mut am_active = role.active();
    on_ownership(Ownership {
        active: am_active,
        panic: false,
        driving: role.driving(),
    });
    tracing::info!(am_active, "initial ownership");

    // When the previous turn of the loop came round, so a stall with no owner
    // still leaves a mark. Without it a blocking call in a spot nobody thought
    // to instrument is invisible — which is how a 2.6 s freeze once reached
    // the user with a clean log.
    let mut last_turn: Option<tokio::time::Instant> = None;

    let mut inject_latency = LatencyWindow::new("received->injected");
    let mut forward_latency = LatencyWindow::new("captured->sent");

    let result = loop {
        if let Some(prev) = last_turn {
            let turn = prev.elapsed();
            if turn >= SLOW_TURN_WARN {
                tracing::warn!(
                    took_ms = turn.as_millis() as u64,
                    "session loop turn was slow; input, audio and file traffic all waited"
                );
            }
        }
        last_turn = Some(tokio::time::Instant::now());

        tokio::select! {
            // Ordered, not random. The branches are already written most- to
            // least- urgent, and an ownership change queued alongside input has
            // to be applied first: the same events mean different things either
            // side of it — forwarded to the peer, or acting locally — and
            // letting the race decide drops the ones read too early. The timers
            // above it fire on their own schedule, so nothing starves.
            biased;

            _ = ticker.tick() => {
                inject_latency.report_if_due();
                forward_latency.report_if_due();
                // Has the peer gone quiet? Checked on the heartbeat tick, so
                // detection costs nothing extra and lands within one interval
                // of the deadline.
                let heard_at = Duration::from_millis(last_heard.load(Ordering::Relaxed));
                let silent_for = epoch.elapsed().saturating_sub(heard_at);
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
            _ = status_ticker.tick() => {
                // `poll_status` reads the machine-wide monitor and answers
                // `None` unless something changed by the protocol's own
                // standard — so a plugged-in desktop sends nothing here for the
                // life of the session.
                if let Some(status) = poll_status() {
                    tracing::info!(%status, "local status changed; telling peer");
                    if let Err(e) = tx.send(&Message::NodeStatus(status)).await {
                        break Err(SessionError::from(e));
                    }
                }
            }
            Some(failure) = failures.recv() => {
                tracing::warn!(module = failure.module, error = %failure.error, "module lost the link; ending session");
                break Err(SessionError::from(failure.error));
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
                            on_ownership(Ownership { active: am_active, panic: false, driving: role.driving() });
                            // Peer becomes active, entering on its opposite edge.
                            let msg = Message::CrossEdge { edge: edge.opposite(), pos_norm };
                            if let Err(e) = tx.send(&msg).await {
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
                                forward_captured(&mut tx, event, captured.as_deref_mut(), &mut forward_latency).await
                            {
                                break Err(e);
                            }
                        }
                    }
                    None => captured = None,
                }
            }
            incoming = inbound_rx.recv() => {
                let (incoming, read_at) = match incoming {
                    Some((msg, read_at)) => (Some(msg), Some(read_at)),
                    None => (None, None),
                };
                match incoming {
                    Some(Message::Heartbeat { nonce }) => tracing::trace!(nonce, "peer heartbeat"),
                    // The peer's power or network changed. Logged rather than
                    // acted on: nothing routes by it yet, and a peer whose
                    // battery is draining is exactly the thing a user later
                    // finds in a log asking why a transfer moved.
                    Some(Message::NodeStatus(status)) => {
                        tracing::info!(%status, "peer status changed");
                    }
                    Some(Message::Input(event)) => {
                        // Inject only while active (we hold the cursor).
                        if am_active {
                            tracing::trace!(?event, "peer input");
                            on_input(event);
                            if let Some(read_at) = read_at {
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
    tracing::info!("session ending; shutting down modules");
    module_host::shut_down(hosts, module_host::MODULE_SHUTDOWN_GRACE).await;

    // The prioritized sender only *queues* on `send`, so a final `Bye`/shutdown
    // message may still be waiting. Flush it to the wire before dropping `tx`,
    // or the drainer task gets cancelled with the goodbye still queued. No-op
    // for a plain (synchronous) sender.
    tx.flush().await;
    tx.close().await;
    reader.abort();
    tracing::info!("session teardown complete");
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
    first: CapturedInput,
    captured: Option<&mut tokio::sync::mpsc::UnboundedReceiver<CapturedInput>>,
    latency: &mut LatencyWindow,
) -> Result<(), SessionError> {
    use cursio_protocol::InputEvent;
    use tokio::sync::mpsc::error::TryRecvError;

    /// Motion not yet sent, and when the oldest part of it was captured —
    /// what the latency figure charges a coalesced frame with, since that
    /// part waited longest.
    #[derive(Default)]
    struct Pending {
        dx: i32,
        dy: i32,
        oldest: Option<std::time::Instant>,
    }

    async fn flush(
        tx: &mut dyn cursio_transport::TransportTx,
        pending: &mut Pending,
        latency: &mut LatencyWindow,
    ) -> Result<(), SessionError> {
        if pending.dx != 0 || pending.dy != 0 {
            tx.send(&Message::Input(InputEvent::PointerMotion {
                dx: pending.dx,
                dy: pending.dy,
            }))
            .await?;
        }
        if let Some(oldest) = pending.oldest {
            latency.record(oldest.elapsed());
        }
        *pending = Pending::default();
        Ok(())
    }

    // Fold motion into the accumulator, or flush it and send the event.
    async fn handle(
        tx: &mut dyn cursio_transport::TransportTx,
        input: CapturedInput,
        pending: &mut Pending,
        latency: &mut LatencyWindow,
    ) -> Result<(), SessionError> {
        match input.event {
            InputEvent::PointerMotion { dx, dy } => {
                pending.dx += dx;
                pending.dy += dy;
                pending.oldest.get_or_insert(input.at);
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
                    tracing::debug!(event = ?other, pending_dx = pending.dx, pending_dy = pending.dy,
                        "forwarding pointer button to peer");
                }
                flush(tx, pending, latency).await?;
                tx.send(&Message::Input(other)).await?;
                latency.record(input.at.elapsed());
                Ok(())
            }
        }
    }

    let mut pending = Pending::default();
    handle(tx, first, &mut pending, latency).await?;

    // Drain everything already queued without blocking, coalescing as we go.
    if let Some(rx) = captured {
        loop {
            match rx.try_recv() {
                Ok(input) => handle(tx, input, &mut pending, latency).await?,
                Err(TryRecvError::Empty) => break,
                // Sender gone: send what we have, then stop draining. The
                // caller's next recv sees the closed channel and clears it.
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    // Send any leftover coalesced motion.
    flush(tx, &mut pending, latency).await
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

    fn id(byte: u8) -> PeerId {
        PeerId::from_bytes([byte; 16])
    }

    /// The figures a lag report quotes are the real percentiles of what was
    /// recorded: a single slow event shows as the max, not as the typical case.
    #[test]
    fn input_latency_reports_the_median_and_the_tail_separately() {
        let mut window = LatencyWindow::new("test");
        assert_eq!(window.percentiles(), None, "no input, nothing to say");
        for _ in 0..99 {
            window.record(Duration::from_micros(200));
        }
        window.record(Duration::from_millis(40));
        let (p50, p99, max) = window.percentiles().unwrap();
        assert_eq!(p50, Duration::from_micros(200));
        assert_eq!(p99, Duration::from_micros(200));
        assert_eq!(max, Duration::from_millis(40));
    }

    /// A flood of events keeps a bounded window of the most recent ones, so an
    /// hour of gaming does not grow memory and the report reflects now.
    #[test]
    fn input_latency_keeps_only_the_most_recent_samples() {
        let mut window = LatencyWindow::new("test");
        for _ in 0..INPUT_LATENCY_SAMPLES {
            window.record(Duration::from_millis(30));
        }
        for _ in 0..INPUT_LATENCY_SAMPLES {
            window.record(Duration::from_micros(100));
        }
        assert_eq!(window.samples.len(), INPUT_LATENCY_SAMPLES);
        let (_, p99, max) = window.percentiles().unwrap();
        assert_eq!(p99, Duration::from_micros(100));
        assert_eq!(max, Duration::from_micros(100));
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

    /// Whichever machine is not holding the cursor is streaming its input to
    /// the one that is — from the very first moment, with nothing handed
    /// anywhere yet.
    ///
    /// This is the whole model: one cursor, and every mouse can move it. The
    /// alternative was a machine that held nothing *and* streamed nothing, and
    /// it is what made a real mouse stop working — see [`CursorRole`].
    #[test]
    fn the_machine_without_the_cursor_streams_its_input() {
        let pair = Pair::new();
        assert!(pair.here.active(), "here starts with the cursor");
        assert!(!pair.here.driving(), "so it acts on itself");
        assert!(!pair.there.active());
        assert!(
            pair.there.driving(),
            "the far machine's mouse must reach the cursor, not its own desktop"
        );
    }

    /// Sending the cursor across swaps the two roles, and nothing else.
    #[test]
    fn sending_the_cursor_across_swaps_who_streams() {
        let mut pair = Pair::new();
        pair.push_across();
        assert!(pair.there.active(), "the far machine holds the cursor");
        assert!(pair.here.driving(), "and this one now streams to it");
        assert!(!pair.there.driving());
    }

    /// The failure this is all for: after a round trip, *both* mice still
    /// work. Sending the cursor back used to leave the returning machine
    /// holding nothing and driving nobody — a mouse that moved its own pointer
    /// and nothing else, with the only way out being to shove the cursor home
    /// by hand from the far end.
    #[test]
    fn a_round_trip_leaves_both_mice_working() {
        let mut pair = Pair::new();
        pair.push_across();
        pair.push_back();
        assert!(pair.here.active(), "the cursor came home");
        assert!(
            pair.there.driving(),
            "and the machine that returned it still reaches the cursor"
        );

        // The second crossing is the one that used to break.
        pair.push_across();
        assert!(pair.there.active());
        assert!(pair.here.driving(), "this machine streams again");
    }

    /// Peer ids choose an initial holder, not which physical mouse the user is
    /// allowed to use — but taking the cursor *back* is the hotkey's job, not
    /// the edge's.
    ///
    /// The edge cannot serve here: the cursor is not on this screen to be
    /// pushed off it, and honouring the crossing anyway meant a pointer merely
    /// drifting into the edge dragged the cursor away from the screen the user
    /// was looking at. Every backend but macOS already refused to report it.
    #[test]
    fn a_machine_without_the_cursor_takes_it_with_the_hotkey_not_the_edge() {
        let mut holder = CursorRole::new(true);
        let mut source = CursorRole::new(false);

        // The edge does nothing when the cursor is elsewhere.
        source.hand_over();
        assert!(!source.active(), "the edge must not pull the cursor over");
        assert!(holder.active(), "and the holder still holds it");

        // The hotkey does.
        source.toggle();
        holder.adopt(false);
        assert!(source.active());
        assert!(!holder.active());
        assert!(
            holder.driving(),
            "the machine that gave it up still reaches the cursor"
        );
    }

    #[test]
    fn many_round_trips_never_leave_both_or_neither_in_charge() {
        let mut pair = Pair::new();
        for trip in 0..10 {
            pair.push_across();
            assert!(pair.there.active(), "trip {trip}: cursor did not cross");
            assert!(pair.here.driving(), "trip {trip}: sender stopped streaming");
            assert!(
                !pair.there.driving(),
                "trip {trip}: holder is streaming too"
            );
            pair.push_back();
            assert!(pair.here.active(), "trip {trip}: cursor did not come home");
            assert!(
                pair.there.driving(),
                "trip {trip}: far mouse went dead after returning the cursor"
            );
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

    /// Being told to stand down hands the cursor away and leaves this mouse
    /// pointed at wherever it went.
    #[test]
    fn standing_down_streams_to_the_new_holder() {
        let mut role = CursorRole::new(true);
        role.adopt(false);
        assert!(!role.active());
        assert!(role.driving(), "this mouse must still reach the cursor");
    }

    /// The exact sequence from the 18:23 logs, in the order it happened.
    ///
    /// A pointer drifting into the edge of a screen that does *not* hold the
    /// cursor used to drag it over, and from there the cursor could not come
    /// back — the peer held it and this side dismissed every crossing. The edge
    /// must stay inert until the cursor is actually on this screen to be pushed
    /// off it.
    #[test]
    fn the_edge_is_inert_on_a_screen_the_cursor_has_left() {
        let mut mac = CursorRole::new(false);
        let mut windows = CursorRole::new(true);

        // The user at Windows shoves the cursor across.
        windows.hand_over();
        mac.take_over();
        assert!(mac.active());
        assert!(windows.driving(), "Windows' own mouse still reaches it");

        // And pushes it back.
        mac.hand_over();
        windows.take_over();
        assert!(!mac.active());
        assert!(windows.active());
        assert!(mac.driving(), "the Mac's mouse still reaches the cursor");

        // Now the Mac's pointer wanders into its edge, repeatedly. The cursor
        // is not there to be sent anywhere.
        let handed_back = mac;
        for _ in 0..5 {
            mac.hand_over();
        }
        assert_eq!(
            mac, handed_back,
            "the edge must be inert without the cursor"
        );
        assert!(
            windows.active(),
            "the cursor is still where the user left it"
        );

        // And the ordinary way across still works.
        windows.hand_over();
        mac.take_over();
        assert!(mac.active());
        assert!(windows.driving());
    }

    /// The panic hotkey exists for a wedged link, so it must reach a state this
    /// machine can be used from whatever it was doing.
    #[test]
    fn panic_returns_everything_to_this_machine() {
        let mut role = CursorRole::new(true);
        role.hand_over();
        assert!(role.driving());
        role.panic();
        assert!(role.active());
        assert!(!role.driving());
    }
}
