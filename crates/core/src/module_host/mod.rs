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

use std::time::Duration;

use cursio_protocol::{Message, ModuleId};
use cursio_transport::TransportError;
use tokio::sync::{mpsc, oneshot};

use crate::module::SessionModule;
pub(super) use crate::module_host::link_tx::LinkTx;
use crate::module_host::pace::Pace;
use crate::module_host::router::module_inbox;
use crate::module_host::router::Inbox;
pub(super) use crate::module_host::router::Router;

mod link_tx;
mod pace;
mod router;

pub use pace::modules_busy;
pub(crate) use pace::MODULE_PUMP_INTERVAL;

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
        let (inbox_tx, inbox_rx) = module_inbox(name);
        let (stop_tx, stop_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let closer = tx.closer();
        let runtime = tokio::runtime::Handle::current();
        let thread = std::thread::Builder::new()
            .name(format!("cursio-{name}"))
            .spawn(move || {
                runtime.block_on(drive(module, tx, inbox_rx, stop_rx, failures));
                // After `drive` has dropped the module: a module's drop joins
                // its own threads (PipeWire streams, a capture), and the next
                // session with this peer should not start beside them.
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            name,
            id,
            inbox: Inbox { name, tx: inbox_tx },
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
    mut inbox: crate::queue::Receiver<Message>,
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
    use crate::module_host::router::module_inbox;
    use crate::module_host::router::Inbox;
    use crate::module_host::router::PARKED_PER_MODULE;

    use cursio_protocol::Priority;

    fn inbox(name: &'static str) -> (Inbox, crate::queue::Receiver<Message>) {
        let (tx, rx) = module_inbox(name);
        (Inbox { name, tx }, rx)
    }

    #[test]
    fn a_module_overload_is_visible_to_the_session_instead_of_growing_memory() {
        let (tx, mut rx) = crate::queue::channel(
            "small module",
            2,
            1024,
            crate::queue::message_bytes,
            |_, _| false,
        );
        let router = Router::default();
        router.insert(ModuleId::FILES, Inbox { name: "files", tx }, false);
        for _ in 0..3 {
            router.route(envelope(ModuleId::FILES));
        }
        assert!(router.error().is_some());
        assert!(rx.stats().failed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_message_for_a_module_that_has_ended_does_not_end_the_session() {
        let (files, files_rx) = inbox("files");
        let router = Router::default();
        router.insert(ModuleId::FILES, files, false);
        drop(files_rx);
        assert!(router.route(envelope(ModuleId::FILES)).is_none());
        assert!(router.error().is_none());
    }

    #[test]
    fn parked_modules_have_an_aggregate_limit() {
        let router = Router::default();
        for _ in 0..=PARKED_PER_MODULE {
            router.route(envelope(ModuleId::FILES));
        }
        assert!(router.error().is_some());
        assert_eq!(
            router.routes().parked.values().map(Vec::len).sum::<usize>(),
            PARKED_PER_MODULE
        );
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
        let backlog = files.tx.clone();
        let router = Router::default();
        router.insert(ModuleId::FILES, files, false);
        router.route(envelope(ModuleId::FILES));
        router.route(envelope(ModuleId::FILES));
        assert_eq!(backlog.stats().items, 2);
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
}
