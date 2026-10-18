//! Each session module on a thread of its own.
//!
//! Modules used to share the session loop: every turn pumped each of them, and
//! an inbound module message was offered to each in turn until one claimed
//! it. The loop is also where the shared cursor lives, so a module that waited
//! on anything — a PipeWire thread join (380 ms), a server lock held across a
//! disk read (up to 1.9 s), `GetWindowText` on a hung window (220 ms) — froze
//! the mouse, the heartbeat and every other module with it. Each of those was
//! fixed where it happened; this is what makes the whole class impossible.
//!
//! A [`ModuleHost`] drives one [`SessionModule`] on a dedicated OS thread, in
//! the same four calls the loop used to make: `start`, a paced `pump`,
//! `handle_message` for what the reader routed to it, and `shutdown`. The trait
//! did not change shape, because none of those calls ever needed the session:
//! modules were already `Send`, held no `Rc`, and moved their own blocking work
//! to threads. What changed is who waits when a module is slow — now only the
//! module itself.
//!
//! ## Why a thread each, and not tasks on the runtime
//!
//! A task on the multi-threaded runtime is only isolated until it blocks, and
//! blocking is precisely the failure this exists for: a pump stuck in a
//! syscall holds a worker, and whatever was queued behind it on that worker
//! waits too. A thread per module bounds the damage to that module.
//!
//! The thread runs the module with [`tokio::runtime::Handle::block_on`] on the
//! runtime that owns the link, not a runtime of its own. Anything a module
//! spawns — a drag watcher, an AirPlay forwarder, a portal connection kept for
//! the next session — then lives on the process runtime and survives the
//! module's thread, instead of dying with a private runtime at the end of every
//! session. On a current-thread runtime (Android, tests) that runtime's own
//! thread drives timers and I/O, which it does for the whole life of a node.
//!
//! ## Sending
//!
//! Every host sends through its own [`LinkTx`]. Over the priority multiplexer
//! that is a clone of the one queue, so a module's message is scheduled
//! against input exactly as before and nothing is serialised behind a lock the
//! session also needs. A sender with no queue to share is wrapped once and
//! shared; that only happens on plain test transports.
//!
//! ## Receiving
//!
//! The reader routes each message by [`Message::module_id`] straight into one
//! module's inbox ([`Router`]), so module traffic never passes through the
//! session loop and never sits between two input events.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cursio_protocol::{Message, ModuleId, Priority};
use cursio_transport::{QueueDepth, TransportError, TransportTx};
use tokio::sync::{mpsc, oneshot};

use crate::module::SessionModule;

/// How often a module is pumped while it has traffic.
///
/// Sized just under the audio chunk cadence (~10 ms) so a steady stream is
/// pumped smoothly rather than in bursts. Without a pace a module was pumped
/// only when something else woke the loop, which turned continuous audio into
/// one clump every heartbeat.
pub(crate) const MODULE_PUMP_INTERVAL: Duration = Duration::from_millis(5);

/// The pace a module falls back to once it has been quiet for
/// [`IDLE_PUMP_AFTER`] consecutive pumps.
///
/// 200 wakeups a second buy nothing when nothing streams, but they keep the
/// core out of its deeper idle states — a steady draw on an always-on node, per
/// module now rather than per link. 50 ms still bounds how long a module that
/// starts producing between ticks waits for its first pump.
pub(crate) const IDLE_PUMP_INTERVAL: Duration = Duration::from_millis(50);

/// Consecutive silent pumps before dropping to [`IDLE_PUMP_INTERVAL`].
///
/// Several rather than one: audio chunks do not arrive on a perfect grid, so
/// one empty tick is normal mid-stream. At the fast pace this is 100 ms of
/// silence before the pace changes at all.
pub(crate) const IDLE_PUMP_AFTER: u32 = 20;

/// The pace of a module that has been quiet for long, after
/// [`DEEP_IDLE_AFTER`] pumps at [`IDLE_PUMP_INTERVAL`].
///
/// Even 50 ms is a wakeup per module per link twenty times a second: a phone
/// with eight modules on four links woke ~640 times a second with nothing
/// happening, and held the CPU awake to do it. What arrives from the peer
/// wakes a module through its inbox at once, whatever its pace; only a module
/// that has to notice something local — a capture that started producing, a
/// copy — waits, and a second is a fair price for that once it has been
/// quiet this long.
pub(crate) const DEEP_IDLE_INTERVAL: Duration = Duration::from_secs(1);

/// Pumps at [`IDLE_PUMP_INTERVAL`] before dropping to [`DEEP_IDLE_INTERVAL`]:
/// two seconds of nothing.
pub(crate) const DEEP_IDLE_AFTER: u32 = 40;

/// How many modules, across every link of this process, are at the fast pace
/// now — that is, have traffic. Zero means the node is idle, which a phone
/// uses to let its CPU and radio sleep; see [`modules_busy`].
static FAST_MODULES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Whether any module of any link is moving data or owes an answer now.
pub fn modules_busy() -> bool {
    FAST_MODULES.load(std::sync::atomic::Ordering::Relaxed) > 0
}

/// Warn when a single pump takes at least this long.
///
/// Only the module itself waited, so this is no longer "the mouse froze" — it
/// is this module's own traffic arriving late, which is still worth naming.
const SLOW_PUMP_WARN: Duration = Duration::from_millis(50);

/// Warn when handling one inbound message takes at least this long.
///
/// Lower than [`SLOW_PUMP_WARN`]: handling one message is a lookup and a state
/// change, so anything near a frame's worth means it waited on a device node,
/// a lock or a syscall — and every later message for this module waited too.
const SLOW_HANDLE_WARN: Duration = Duration::from_millis(20);

/// Most inbound messages handled before the module is pumped again.
///
/// A flood of requests must not starve the replies they produce: the pump is
/// what sends them.
const INBOX_BATCH: usize = 64;

/// Unhandled messages in one inbox that are worth a warning.
///
/// Inboxes are unbounded because the reader must never block — it carries the
/// heartbeat and the cursor too — and dropping a module's message silently
/// would be worse than holding it. A backlog this deep means the module is
/// wedged or far behind, and memory is growing on its behalf.
const INBOX_BACKLOG_WARN: usize = 4096;

/// How long teardown waits for every module to shut down before leaving the
/// stragglers behind.
///
/// A module wedged in a blocking call cannot be interrupted, and waiting for
/// it held the session — and with it the reconnect — hostage. Past this the
/// thread is detached with a warning naming it; it finishes, and drops the
/// module, whenever its call returns.
pub(crate) const MODULE_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// The message's variant name, for a log line that has to name what stalled.
///
/// `Debug` on the message would carry the payload — an audio chunk's samples,
/// a window's pixels — into a warning that fires exactly when the log is the
/// only evidence available.
pub(crate) fn message_kind(msg: &Message) -> &'static str {
    match msg {
        Message::Hello(_) => "Hello",
        Message::PairRequest { .. } => "PairRequest",
        Message::PairResponse { .. } => "PairResponse",
        Message::PairConfirm { .. } => "PairConfirm",
        Message::PairFailed { .. } => "PairFailed",
        Message::Heartbeat { .. } => "Heartbeat",
        Message::Bye => "Bye",
        Message::NodeStatus(_) => "NodeStatus",
        Message::Reconfigure(_) => "Reconfigure",
        Message::Opaque(_) => "Opaque",
        Message::Input(_) => "Input",
        Message::Grab { .. } => "Grab",
        Message::CrossEdge { .. } => "CrossEdge",
        Message::WindowFrame(_) => "WindowFrame",
        Message::WindowInput(_) => "WindowInput",
        Message::AudioChunk(_) => "AudioChunk",
        Message::Update(_) => "Update",
        // The module's own vocabulary is opaque here by design, so this names
        // the module rather than the message inside it.
        Message::Module { module, .. } => match *module {
            ModuleId::INPUT => "Module(input)",
            ModuleId::AUDIO => "Module(audio)",
            ModuleId::CLIPBOARD => "Module(clipboard)",
            ModuleId::WINDOW => "Module(window)",
            ModuleId::FILES => "Module(files)",
            ModuleId::SERVICE => "Module(service)",
            ModuleId::USB => "Module(usb)",
            _ => "Module(other)",
        },
    }
}

type Slot = Arc<tokio::sync::Mutex<Option<Box<dyn TransportTx>>>>;

/// One holder's way onto the link: the session's own, or one module's.
///
/// Closable from outside, which is what lets teardown finish while a module is
/// wedged: its handle onto the queue is taken away, so the link closes when
/// the session is done with it rather than when the module lets go, and
/// anything the module sends afterwards fails instead of reaching a peer that
/// has moved on.
pub(crate) struct LinkTx {
    slot: Slot,
    open: Arc<AtomicBool>,
    /// Whether `slot` belongs to this holder alone. A shared one must stay
    /// filled when one holder closes, because the others still send through
    /// it.
    own: bool,
    /// Messages this handle has sent — what a module's pace follows.
    sent: u64,
}

impl LinkTx {
    pub(crate) fn new(tx: Box<dyn TransportTx>) -> Self {
        Self {
            slot: Arc::new(tokio::sync::Mutex::new(Some(tx))),
            open: Arc::new(AtomicBool::new(true)),
            own: true,
            sent: 0,
        }
    }

    /// A handle for a module: a clone of the queue where the sender has one,
    /// or this very sender, shared, where it does not.
    pub(crate) fn fork(&self) -> Self {
        let cloned = self
            .slot
            .try_lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|tx| tx.clone_sender()));
        match cloned {
            Some(tx) => Self::new(tx),
            None => Self {
                slot: self.slot.clone(),
                open: Arc::new(AtomicBool::new(true)),
                own: false,
                sent: 0,
            },
        }
    }

    /// Another reference to the same handle, for closing it from outside.
    fn closer(&self) -> Self {
        Self {
            slot: self.slot.clone(),
            open: self.open.clone(),
            own: self.own,
            sent: 0,
        }
    }

    /// Refuse every later send, and let go of the queue if it is ours alone.
    ///
    /// Bounded: a module holds the slot only for the length of one `send`,
    /// which on the priority multiplexer never waits for the socket.
    pub(crate) async fn close(&self) {
        self.open.store(false, Ordering::Release);
        if !self.own {
            return;
        }
        match tokio::time::timeout(Duration::from_millis(250), self.slot.lock()).await {
            Ok(mut slot) => drop(slot.take()),
            Err(_) => tracing::warn!("a module held its sender through teardown; leaving it"),
        }
    }

    pub(crate) fn sent(&self) -> u64 {
        self.sent
    }
}

#[async_trait]
impl TransportTx for LinkTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        if !self.open.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        let mut slot = self.slot.lock().await;
        let tx = slot.as_mut().ok_or(TransportError::Closed)?;
        tx.send(msg).await?;
        self.sent = self.sent.wrapping_add(1);
        Ok(())
    }

    fn queue_depth(&self, priority: Priority) -> QueueDepth {
        // Only this holder and teardown ever take the lock, so a miss means
        // teardown has it and the figure no longer matters.
        self.slot
            .try_lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|tx| tx.queue_depth(priority)))
            .unwrap_or_default()
    }

    async fn flush(&mut self) {
        let mut slot = self.slot.lock().await;
        if let Some(tx) = slot.as_mut() {
            tx.flush().await;
        }
    }
}

/// Whether a module is pumped at the fast pace or the idle one.
///
/// Per module now, which is what the shared loop could never be: a file
/// transfer used to hold every module at 200 wakeups a second, and an idle
/// clipboard now sleeps beside a live audio stream.
struct Pace {
    interval: tokio::time::Interval,
    fast: bool,
    /// Past [`DEEP_IDLE_AFTER`] at the idle pace.
    deep: bool,
    silent_pumps: u32,
    last_sent: u64,
}

impl Pace {
    fn new() -> Self {
        FAST_MODULES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            interval: Self::interval(MODULE_PUMP_INTERVAL),
            fast: true,
            deep: false,
            silent_pumps: 0,
            last_sent: 0,
        }
    }

    fn interval(period: Duration) -> tokio::time::Interval {
        let mut interval = tokio::time::interval(period);
        // A missed tick fires once and carries on; replaying the backlog as a
        // burst is the very clumping the pace exists to prevent.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval
    }

    /// Follow the module's traffic after a pump.
    ///
    /// Sent anything, received anything, or owes an answer: stay fast. A mount
    /// serving a download sends little and receives a lot, and judging that
    /// idle put 50 ms between each request and the answer that had just
    /// arrived. Answers `true` when the pace changed.
    fn follow(&mut self, sent: u64, busy: bool) -> bool {
        let active = sent != self.last_sent || busy;
        self.last_sent = sent;
        if active {
            self.silent_pumps = 0;
            self.deep = false;
            if !self.fast {
                self.interval = Self::interval(MODULE_PUMP_INTERVAL);
                self.fast = true;
                FAST_MODULES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return true;
            }
        } else if self.fast {
            self.silent_pumps = self.silent_pumps.saturating_add(1);
            if self.silent_pumps >= IDLE_PUMP_AFTER {
                self.interval = Self::interval(IDLE_PUMP_INTERVAL);
                self.fast = false;
                self.silent_pumps = 0;
                FAST_MODULES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                return true;
            }
        } else if !self.deep {
            self.silent_pumps = self.silent_pumps.saturating_add(1);
            if self.silent_pumps >= DEEP_IDLE_AFTER {
                self.interval = Self::interval(DEEP_IDLE_INTERVAL);
                self.deep = true;
                return true;
            }
        }
        false
    }
}

impl Drop for Pace {
    fn drop(&mut self) {
        if self.fast {
            FAST_MODULES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// A module that stopped being able to talk to the peer.
///
/// Reported to the session rather than handled here: a failed send means the
/// link is dead, which is the session's to act on, and a failed `start` has
/// always ended the session.
#[derive(Debug)]
pub(crate) struct ModuleFailure {
    pub module: &'static str,
    pub error: TransportError,
}

/// What the reader holds for one module: where its messages go.
#[derive(Clone)]
struct Inbox {
    name: &'static str,
    tx: mpsc::UnboundedSender<Message>,
    backlog: Arc<AtomicUsize>,
}

/// Most messages held for one module that is not running yet.
///
/// Enough for a module's opening exchange; a peer streaming into a module this
/// side never starts cannot grow it past this.
const PARKED_PER_MODULE: usize = 256;

#[derive(Default)]
struct Routes {
    inboxes: HashMap<ModuleId, Inbox>,
    /// Messages for a module not running here yet, in arrival order.
    parked: HashMap<ModuleId, Vec<Message>>,
}

/// The reader's routing table: each module's messages to that module alone.
///
/// Shared between the reader task, which routes, and the session loop, which
/// adds and removes modules when the link is reconfigured. The lock is held
/// for one lookup and one channel send; the session takes it only when a
/// module starts or stops.
#[derive(Clone, Default)]
pub(crate) struct Router {
    routes: Arc<std::sync::Mutex<Routes>>,
}

impl Router {
    fn routes(&self) -> std::sync::MutexGuard<'_, Routes> {
        self.routes.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Deliver `msg` to its module, or hand it back when it is the session's.
    ///
    /// A message for a module not running here is held, up to a bound. The
    /// peer starts a module right after announcing it in a reconfiguration,
    /// and the reader routes its first message before the session loop has
    /// read the announcement and started the module on this side; dropping it
    /// lost that module's opening exchange.
    pub(crate) fn route(&self, msg: Message) -> Option<Message> {
        let Some(id) = msg.module_id() else {
            return Some(msg);
        };
        let mut routes = self.routes();
        match routes.inboxes.get(&id) {
            Some(inbox) => deliver(inbox, msg),
            None => {
                let parked = routes.parked.entry(id).or_default();
                if parked.len() < PARKED_PER_MODULE {
                    parked.push(msg);
                } else {
                    tracing::debug!(module = %id, kind = message_kind(&msg), "message for a module not running here");
                }
            }
        }
        None
    }

    /// Start delivering `id`'s messages to `inbox`.
    ///
    /// `keep_parked` is whether what arrived before now belongs to it: yes
    /// when the peer announced the module first, since it sends nothing for a
    /// module before announcing it; no when this side started it, since then
    /// anything held is from an instance the peer has since stopped.
    fn insert(&self, id: ModuleId, inbox: Inbox, keep_parked: bool) {
        let mut routes = self.routes();
        let parked = routes.parked.remove(&id).unwrap_or_default();
        if keep_parked {
            for msg in parked {
                deliver(&inbox, msg);
            }
        }
        if routes.inboxes.insert(id, inbox).is_some() {
            tracing::warn!(module = %id, "two modules claim one id; the later one receives");
        }
    }

    fn remove(&self, id: ModuleId) {
        let mut routes = self.routes();
        routes.inboxes.remove(&id);
        routes.parked.remove(&id);
    }
}

fn deliver(inbox: &Inbox, msg: Message) {
    let depth = inbox.backlog.fetch_add(1, Ordering::Relaxed) + 1;
    if depth == INBOX_BACKLOG_WARN {
        tracing::warn!(
            module = inbox.name,
            depth,
            "module is not keeping up with the peer's messages; they are queueing"
        );
    }
    if inbox.tx.send(msg).is_err() {
        inbox.backlog.fetch_sub(1, Ordering::Relaxed);
        tracing::debug!(module = inbox.name, "message for a module that has ended");
    }
}

/// One module running on its own thread.
pub(crate) struct ModuleHost {
    name: &'static str,
    id: ModuleId,
    inbox: Inbox,
    stop: Option<oneshot::Sender<()>>,
    done: oneshot::Receiver<()>,
    tx: LinkTx,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ModuleHost {
    /// Start `module` on a thread of its own, sending through `tx`.
    ///
    /// Must be called from inside the runtime the link runs on, which the
    /// thread then borrows (see the module docs for why not a runtime of its
    /// own).
    pub(crate) fn spawn(
        module: Box<dyn SessionModule>,
        tx: LinkTx,
        failures: mpsc::UnboundedSender<ModuleFailure>,
    ) -> std::io::Result<Self> {
        let name = module.name();
        let id = module.id();
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        let backlog = Arc::new(AtomicUsize::new(0));
        let (stop_tx, stop_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let closer = tx.closer();
        let runtime = tokio::runtime::Handle::current();
        let host_backlog = backlog.clone();
        let thread = std::thread::Builder::new()
            .name(format!("cursio-{name}"))
            .spawn(move || {
                runtime.block_on(drive(module, tx, inbox_rx, host_backlog, stop_rx, failures));
                // After `drive` has dropped the module: a module's drop joins
                // its own threads (PipeWire streams, a capture), and the next
                // session with this peer should not start beside them.
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            name,
            id,
            inbox: Inbox {
                name,
                tx: inbox_tx,
                backlog,
            },
            stop: Some(stop_tx),
            done: done_rx,
            tx: closer,
            thread: Some(thread),
        })
    }

    /// Ask the module to shut down. Returns at once; see [`Self::finished`].
    fn request_stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Build the reader's routing table for these hosts.
pub(crate) fn router_for(hosts: &[ModuleHost]) -> Router {
    let router = Router::default();
    for host in hosts {
        router.insert(host.id, host.inbox.clone(), false);
    }
    router
}

impl ModuleHost {
    pub(crate) fn id(&self) -> ModuleId {
        self.id
    }

    /// Route `id`'s messages to this host. See [`Router::insert`] for
    /// `keep_parked`.
    pub(crate) fn attach(&self, router: &Router, keep_parked: bool) {
        router.insert(self.id, self.inbox.clone(), keep_parked);
    }
}

/// Stop routing to `id`, and drop whatever was held for it.
pub(crate) fn detach(router: &Router, id: ModuleId) {
    router.remove(id);
}

/// Shut every host down, waiting at most `grace` for all of them together.
///
/// Stop is asked of all at once, so the slowest module sets the wait rather
/// than the sum. A module still running at the deadline is detached with a
/// warning naming it — its thread keeps the module until its blocking call
/// returns, and then drops it — and its sender is closed either way, so the
/// link can close behind the session.
pub(crate) async fn shut_down(hosts: Vec<ModuleHost>, grace: Duration) {
    let mut hosts = hosts;
    for host in &mut hosts {
        host.request_stop();
    }
    let deadline = tokio::time::Instant::now() + grace;
    for mut host in hosts {
        let finished = tokio::time::timeout_at(deadline, &mut host.done).await;
        match finished {
            Ok(_) => {
                if let Some(thread) = host.thread.take() {
                    // Already past the module's drop; this only reaps the
                    // thread.
                    let _ = thread.join();
                }
            }
            Err(_) => {
                tracing::warn!(
                    module = host.name,
                    grace_ms = grace.as_millis() as u64,
                    "module did not shut down in time; leaving its thread behind"
                );
            }
        }
        host.tx.close().await;
    }
}

/// The module's side: start, then pump at its own pace and handle what the
/// reader routed to it, until told to stop.
async fn drive(
    mut module: Box<dyn SessionModule>,
    mut tx: LinkTx,
    mut inbox: mpsc::UnboundedReceiver<Message>,
    backlog: Arc<AtomicUsize>,
    mut stop: oneshot::Receiver<()>,
    failures: mpsc::UnboundedSender<ModuleFailure>,
) {
    let name = module.name();
    tracing::info!(module = name, "starting session module");
    let mut failed = match module.start(&mut tx).await {
        Ok(()) => false,
        Err(error) => {
            let _ = failures.send(ModuleFailure {
                module: name,
                error,
            });
            true
        }
    };

    let mut pace = Pace::new();
    let mut pump_due = true;
    let mut saw_inbound = false;
    while !failed {
        if pump_due {
            pump_due = false;
            let started = tokio::time::Instant::now();
            let result = module.pump(&mut tx).await;
            let took = started.elapsed();
            if took >= SLOW_PUMP_WARN {
                tracing::warn!(
                    module = name,
                    took_ms = took.as_millis() as u64,
                    "module pump was slow; only this module's traffic waited"
                );
            }
            if let Err(error) = result {
                tracing::warn!(module = name, error = %error, "module pump failed");
                let _ = failures.send(ModuleFailure {
                    module: name,
                    error,
                });
                failed = true;
                continue;
            }
            let busy = std::mem::take(&mut saw_inbound) || module.has_pending_work();
            if pace.follow(tx.sent(), busy) {
                tracing::trace!(module = name, fast = pace.fast, "module pace changed");
            }
        }

        tokio::select! {
            // Stop first: teardown must not wait out a flood of inbound.
            biased;
            _ = &mut stop => break,
            msg = inbox.recv() => {
                let Some(first) = msg else { break };
                let mut next = Some(first);
                let mut handled = 0;
                while let Some(msg) = next.take() {
                    backlog.fetch_sub(1, Ordering::Relaxed);
                    let started = tokio::time::Instant::now();
                    let claimed = module.handle_message(&msg);
                    let took = started.elapsed();
                    if took >= SLOW_HANDLE_WARN {
                        tracing::warn!(
                            module = name,
                            took_ms = took.as_millis() as u64,
                            message = message_kind(&msg),
                            "module was slow handling a message; only its own messages waited"
                        );
                    }
                    if !claimed {
                        tracing::debug!(module = name, message = message_kind(&msg), "module did not recognise its message");
                    }
                    handled += 1;
                    if handled < INBOX_BATCH {
                        next = inbox.try_recv().ok();
                    }
                }
                // A module mid-exchange is busy even while it only listens;
                // the answer goes out on the pump this arranges.
                pump_due = true;
                saw_inbound = true;
            }
            _ = pace.interval.tick() => pump_due = true,
        }
    }

    if failed {
        // The link is gone or never came up for this module; wait for the
        // session to notice rather than shutting down underneath it.
        let _ = stop.await;
    }
    tracing::info!(module = name, "module shutdown begin");
    module.shutdown(&mut tx).await;
    tracing::info!(module = name, "module shutdown done");
    drop(module);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbox(name: &'static str) -> (Inbox, mpsc::UnboundedReceiver<Message>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Inbox {
                name,
                tx,
                backlog: Arc::default(),
            },
            rx,
        )
    }

    fn envelope(module: ModuleId) -> Message {
        Message::Module {
            module,
            priority: Priority::Bulk,
            payload: Vec::new(),
        }
    }

    /// Each module's messages reach that module and no other, and the session's
    /// own vocabulary is handed back rather than delivered anywhere.
    #[test]
    fn inbound_routing_delivers_each_modules_messages_to_it_alone() {
        let (files, mut files_rx) = inbox("files");
        let (audio, mut audio_rx) = inbox("audio");
        let router = Router::default();
        router.insert(ModuleId::FILES, files, false);
        router.insert(ModuleId::AUDIO, audio, false);

        assert!(router.route(envelope(ModuleId::FILES)).is_none());
        assert!(router
            .route(Message::AudioChunk(cursio_protocol::audio::AudioChunk {
                id: 1,
                seq: 0,
                timestamp: 0,
                data: Vec::new(),
            }))
            .is_none());
        assert_eq!(
            router.route(Message::Heartbeat { nonce: 1 }),
            Some(Message::Heartbeat { nonce: 1 })
        );
        // Negotiated away: nobody to deliver it to, and not the session's.
        assert!(router.route(envelope(ModuleId::CLIPBOARD)).is_none());

        assert_eq!(
            files_rx.try_recv().unwrap().module_id(),
            Some(ModuleId::FILES)
        );
        assert!(
            files_rx.try_recv().is_err(),
            "files got someone else's message"
        );
        assert_eq!(
            audio_rx.try_recv().unwrap().module_id(),
            Some(ModuleId::AUDIO)
        );
        assert!(
            audio_rx.try_recv().is_err(),
            "audio got someone else's message"
        );
    }

    /// The reader counts what it delivered and the host what it took, so a
    /// module falling behind shows up as depth rather than as silence.
    #[test]
    fn a_routed_message_counts_against_its_modules_backlog() {
        let (files, _rx) = inbox("files");
        let backlog = files.backlog.clone();
        let router = Router::default();
        router.insert(ModuleId::FILES, files, false);
        router.route(envelope(ModuleId::FILES));
        router.route(envelope(ModuleId::FILES));
        assert_eq!(backlog.load(Ordering::Relaxed), 2);
    }

    /// A module the peer announced gets what it sent ahead of the announcement
    /// being read; one this side starts does not inherit a stopped instance's
    /// leftovers.
    #[test]
    fn messages_for_a_module_not_yet_started_wait_for_it_only_when_the_peer_started_it() {
        let router = Router::default();
        router.route(envelope(ModuleId::FILES));
        let (files, mut files_rx) = inbox("files");
        router.insert(ModuleId::FILES, files, true);
        assert!(files_rx.try_recv().is_ok(), "the early message was lost");

        router.route(envelope(ModuleId::AUDIO));
        let (audio, mut audio_rx) = inbox("audio");
        router.insert(ModuleId::AUDIO, audio, false);
        assert!(
            audio_rx.try_recv().is_err(),
            "a stale message reached a fresh module"
        );
    }

    /// A stall warning has to say which subsystem to look at, and module
    /// traffic is opaque here, so the id is what names it.
    #[test]
    fn a_module_message_is_named_by_its_module() {
        assert_eq!(message_kind(&envelope(ModuleId::AUDIO)), "Module(audio)");
        assert_eq!(message_kind(&envelope(ModuleId::FILES)), "Module(files)");
        assert_eq!(message_kind(&Message::Bye), "Bye");
    }

    /// The idle pace is slower, and a stream with gaps cannot trip it.
    #[test]
    fn the_idle_pace_is_slower_and_slow_to_engage() {
        assert!(IDLE_PUMP_INTERVAL > MODULE_PUMP_INTERVAL);
        assert!(MODULE_PUMP_INTERVAL * IDLE_PUMP_AFTER >= Duration::from_millis(100));
    }

    /// A module that sends, receives or owes an answer stays fast; one that
    /// does none of those for long enough drops to the idle pace, and comes
    /// back the moment it sends again.
    #[tokio::test]
    async fn a_modules_pace_follows_its_own_traffic() {
        let mut pace = Pace::new();
        for _ in 0..IDLE_PUMP_AFTER - 1 {
            assert!(!pace.follow(0, false));
        }
        assert!(pace.fast, "one pump short of the threshold is still fast");
        assert!(pace.follow(0, false));
        assert!(!pace.fast, "quiet long enough drops to the idle pace");

        assert!(pace.follow(1, false), "sending restores the fast pace");
        assert!(pace.fast);

        for _ in 0..IDLE_PUMP_AFTER * 2 {
            pace.follow(1, true);
        }
        assert!(
            pace.fast,
            "a module owed an answer is busy however little it sends"
        );
    }

    /// Quiet at the idle pace for long, a module sleeps a second between
    /// pumps; anything it sends brings it straight back to the fast pace.
    #[tokio::test]
    async fn a_long_quiet_module_drops_to_the_deep_idle_pace() {
        let mut pace = Pace::new();
        for _ in 0..IDLE_PUMP_AFTER {
            pace.follow(0, false);
        }
        assert!(!pace.fast && !pace.deep);
        for _ in 0..DEEP_IDLE_AFTER - 1 {
            assert!(!pace.follow(0, false));
        }
        assert!(!pace.deep, "one pump short of the threshold is not deep yet");
        assert!(pace.follow(0, false));
        assert!(pace.deep);
        assert_eq!(pace.interval.period(), DEEP_IDLE_INTERVAL);

        assert!(pace.follow(1, false), "sending wakes it");
        assert!(pace.fast && !pace.deep);
        assert_eq!(pace.interval.period(), MODULE_PUMP_INTERVAL);
    }
}
