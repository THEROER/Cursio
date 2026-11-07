//! Measuring one link: its speed each way and the pointer's round trips while
//! it is loaded. See [`cursio_protocol::probe`] for why the test is traffic of
//! its own rather than a reading of Cursio's transfers.
//!
//! Symmetric: every node answers pings and pulls, and either may run a test.
//! A test is ordered through [`run`] (the control socket, `cursio link test`)
//! or by the engine itself, rarely and only at a quiet moment
//! ([`AUTO_EVERY`]). Results are kept in [`crate::probe_history`].

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cursio_protocol::probe::{Direction, ProbeMsg, ProbeResult, RoundTrips, MAX_PIECE};
use cursio_protocol::{DeviceKind, Message, ModuleAdvert, ModuleId, ModuleRole, PeerId};
use cursio_transport::{TransportError, TransportTx};
use tokio::sync::{mpsc, oneshot};

use crate::module::SessionModule;

/// Version of [`ProbeMsg`]; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

/// How long each direction is loaded when nobody says.
pub const DEFAULT_SECONDS: u32 = 5;
/// Longest a direction may be loaded: a test spends the link it measures.
pub const MAX_SECONDS: u32 = 30;

/// Pings before any load, for the round trip the link has at rest.
const IDLE_SPAN: Duration = Duration::from_secs(1);
const PING_EVERY: Duration = Duration::from_millis(50);
/// The start of each direction, left out of its speed: the window filling
/// and the congestion control finding its pace are not the link's speed.
const WARM_UP: Duration = Duration::from_millis(500);
/// Pieces outstanding at once, 4 MiB: well past a gigabit LAN's or a Wi-Fi
/// link's bandwidth-delay product, so the window is never what limits.
const WINDOW: usize = 16;
/// Longest the pieces of one direction may take to stop arriving before the
/// next begins, so they are not counted against it.
const DRAIN_MOST: Duration = Duration::from_secs(3);

/// How often a test runs by itself, per pair of machines.
pub const AUTO_EVERY: Duration = Duration::from_secs(12 * 3600);
/// How long a link is up before its first test by itself.
const AUTO_FIRST_AFTER: Duration = Duration::from_secs(10 * 60);
/// How often the engine looks for a quiet moment once one is due.
const AUTO_LOOK_EVERY: Duration = Duration::from_secs(60);
/// Seconds per direction of a test that runs by itself: enough for a
/// figure, short enough not to be felt.
const AUTO_SECONDS: u32 = 3;

type Reply = oneshot::Sender<Result<ProbeResult, String>>;

struct Order {
    secs: u32,
    automatic: bool,
    reply: Option<Reply>,
}

struct Registered {
    name: String,
    token: u64,
    orders: mpsc::UnboundedSender<Order>,
}

fn registry() -> std::sync::MutexGuard<'static, HashMap<PeerId, Registered>> {
    static REG: OnceLock<Mutex<HashMap<PeerId, Registered>>> = OnceLock::new();
    REG.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn next_token() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Test the link to `machine` (a name or a peer id), loading each direction
/// for `secs` seconds.
pub async fn run(machine: &str, secs: u32) -> Result<(String, ProbeResult), String> {
    let secs = secs.clamp(1, MAX_SECONDS);
    let (name, orders) = {
        let reg = registry();
        let (_, r) = reg
            .iter()
            .find(|(id, r)| id.to_string() == machine || r.name.eq_ignore_ascii_case(machine))
            .ok_or_else(|| {
                format!("no connected machine called {machine} takes link tests; is it running this version of Cursio?")
            })?;
        (r.name.clone(), r.orders.clone())
    };
    let (tx, rx) = oneshot::channel();
    orders
        .send(Order {
            secs,
            automatic: false,
            reply: Some(tx),
        })
        .map_err(|_| "the session with that machine ended".to_string())?;
    let most =
        IDLE_SPAN + 2 * (Duration::from_secs(secs.into()) + DRAIN_MOST) + Duration::from_secs(15);
    match tokio::time::timeout(most, rx).await {
        Ok(Ok(result)) => result.map(|r| (name, r)),
        Ok(Err(_)) => Err("the session with that machine ended during the test".into()),
        Err(_) => Err(format!(
            "the test did not finish within {} s",
            most.as_secs()
        )),
    }
}

/// One stretch's pings.
#[derive(Default)]
struct Pings {
    next_at: Option<Instant>,
    outstanding: HashMap<u32, Instant>,
    trips: Vec<Duration>,
    sent: u32,
}

impl Pings {
    fn finish(&mut self) -> RoundTrips {
        let mut trips = std::mem::take(&mut self.trips);
        trips.sort_unstable();
        let at = |f: f64| {
            trips
                .get(((trips.len() as f64 * f).ceil() as usize).clamp(1, trips.len().max(1)) - 1)
                .map_or(0, |d| d.as_micros() as u64)
        };
        let result = RoundTrips {
            p50_us: at(0.50),
            p95_us: at(0.95),
            max_us: trips.last().map_or(0, |d| d.as_micros() as u64),
            sent: self.sent,
            lost: self.outstanding.len() as u32,
        };
        *self = Pings::default();
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Down,
    DrainDown,
    Up,
    DrainUp,
}

struct Test {
    secs: u32,
    automatic: bool,
    reply: Option<Reply>,
    started_unix: u64,
    path: Option<String>,
    phase: Phase,
    phase_at: Instant,
    pings: Pings,
    in_flight: HashSet<u32>,
    counted: u64,
    idle: RoundTrips,
    download: Direction,
    upload: Direction,
}

impl Test {
    fn enter(&mut self, phase: Phase, now: Instant) {
        self.phase = phase;
        self.phase_at = now;
        self.counted = 0;
    }

    fn load_span(&self) -> Duration {
        Duration::from_secs(self.secs.into())
    }

    fn rate(&self, now: Instant) -> u64 {
        let measured = now
            .saturating_duration_since(self.phase_at + WARM_UP)
            .as_secs_f64();
        if measured <= 0.0 {
            return 0;
        }
        (self.counted as f64 / measured) as u64
    }

    fn count(&mut self, bytes: usize, now: Instant) {
        if now >= self.phase_at + WARM_UP {
            self.counted += bytes as u64;
        }
    }
}

pub struct ProbeEngine {
    peer: PeerId,
    peer_name: String,
    token: u64,
    orders: mpsc::UnboundedReceiver<Order>,
    orders_tx: mpsc::UnboundedSender<Order>,
    /// Answers owed to the peer.
    owed: Vec<ProbeMsg>,
    test: Option<Test>,
    next_id: u32,
    /// Whether this end runs the tests that run by themselves: one end of
    /// each pair, so two machines never load the link at once.
    auto: bool,
    up_since: Instant,
    auto_looked: Option<Instant>,
    /// Whether results go to [`crate::probe_history`]; a test's do not.
    keeps: bool,
}

impl ProbeEngine {
    pub fn new(peer: PeerId, peer_name: String, auto: bool) -> Self {
        let (orders_tx, orders) = mpsc::unbounded_channel();
        let token = next_token();
        registry().insert(
            peer,
            Registered {
                name: peer_name.clone(),
                token,
                orders: orders_tx.clone(),
            },
        );
        ProbeEngine {
            peer,
            peer_name,
            token,
            orders,
            orders_tx,
            owed: Vec::new(),
            test: None,
            next_id: 1,
            auto,
            up_since: Instant::now(),
            auto_looked: None,
            keeps: true,
        }
    }

    fn on_message(&mut self, msg: ProbeMsg, now: Instant) {
        match msg {
            ProbeMsg::Ping { seq } => self.owed.push(ProbeMsg::Pong { seq }),
            ProbeMsg::Pull { id, bytes } => self.owed.push(ProbeMsg::Fill {
                id,
                payload: vec![0; bytes.min(MAX_PIECE) as usize],
            }),
            ProbeMsg::Push { id, payload } => self.owed.push(ProbeMsg::Took {
                id,
                bytes: payload.len() as u32,
            }),
            ProbeMsg::Pong { seq } => {
                if let Some(test) = &mut self.test {
                    if let Some(sent) = test.pings.outstanding.remove(&seq) {
                        test.pings.trips.push(now - sent);
                    }
                }
            }
            ProbeMsg::Fill { id, payload } => {
                if let Some(test) = &mut self.test {
                    if test.in_flight.remove(&id) && test.phase == Phase::Down {
                        test.count(payload.len(), now);
                    }
                }
            }
            ProbeMsg::Took { id, bytes } => {
                if let Some(test) = &mut self.test {
                    if test.in_flight.remove(&id) && test.phase == Phase::Up {
                        test.count(bytes as usize, now);
                    }
                }
            }
        }
    }

    fn take_order(&mut self) {
        while let Ok(order) = self.orders.try_recv() {
            if self.test.is_some() {
                if let Some(reply) = order.reply {
                    let _ = reply.send(Err("a test of this link is already running".into()));
                }
                continue;
            }
            let now = Instant::now();
            tracing::info!(peer = %self.peer_name, secs = order.secs, automatic = order.automatic, "testing the link");
            self.test = Some(Test {
                secs: order.secs,
                automatic: order.automatic,
                reply: order.reply,
                started_unix: crate::probe_history::unix_now(),
                path: crate::diag_session::paths(&self.peer, None)
                    .ok()
                    .and_then(|r| r.carrying),
                phase: Phase::Idle,
                phase_at: now,
                pings: Pings::default(),
                in_flight: HashSet::new(),
                counted: 0,
                idle: RoundTrips::default(),
                download: Direction::default(),
                upload: Direction::default(),
            });
        }
    }

    /// Order a test by itself when one is due and the moment is quiet.
    fn maybe_auto(&mut self, now: Instant) {
        if !self.auto || self.test.is_some() || now - self.up_since < AUTO_FIRST_AFTER {
            return;
        }
        if self
            .auto_looked
            .is_some_and(|at| now - at < AUTO_LOOK_EVERY)
        {
            return;
        }
        self.auto_looked = Some(now);
        let due = crate::probe_history::last_at(&self.peer_name).is_none_or(|at| {
            crate::probe_history::unix_now().saturating_sub(at) >= AUTO_EVERY.as_secs()
        });
        if !due
            || crate::update::quiet::busy().is_some()
            || crate::diag_session::carried_by_bluetooth(&self.peer)
        {
            return;
        }
        let _ = self.orders_tx.send(Order {
            secs: AUTO_SECONDS,
            automatic: true,
            reply: None,
        });
    }

    async fn drive(
        &mut self,
        tx: &mut dyn TransportTx,
        now: Instant,
    ) -> Result<(), TransportError> {
        let Some(test) = &mut self.test else {
            return Ok(());
        };
        let elapsed = now - test.phase_at;
        let next = match test.phase {
            Phase::Idle if elapsed >= IDLE_SPAN => {
                test.idle = test.pings.finish();
                Some(Phase::Down)
            }
            Phase::Down if elapsed >= test.load_span() => {
                test.download = Direction {
                    bytes_per_sec: test.rate(now),
                    loaded: test.pings.finish(),
                };
                Some(Phase::DrainDown)
            }
            Phase::Up if elapsed >= test.load_span() => {
                test.upload = Direction {
                    bytes_per_sec: test.rate(now),
                    loaded: test.pings.finish(),
                };
                Some(Phase::DrainUp)
            }
            Phase::DrainDown if test.in_flight.is_empty() || elapsed >= DRAIN_MOST => {
                Some(Phase::Up)
            }
            Phase::DrainUp if test.in_flight.is_empty() || elapsed >= DRAIN_MOST => {
                let test = self.test.take().expect("a test is running");
                self.finish(test);
                return Ok(());
            }
            _ => None,
        };
        if let Some(phase) = next {
            test.in_flight.clear();
            test.enter(phase, now);
        }
        if matches!(test.phase, Phase::Idle | Phase::Down | Phase::Up)
            && test.pings.next_at.is_none_or(|at| now >= at)
        {
            let seq = test.pings.sent;
            test.pings.sent += 1;
            test.pings.outstanding.insert(seq, now);
            test.pings.next_at = Some(now + PING_EVERY);
            send(tx, &ProbeMsg::Ping { seq }).await?;
        }
        while matches!(test.phase, Phase::Down | Phase::Up) && test.in_flight.len() < WINDOW {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            test.in_flight.insert(id);
            let msg = match test.phase {
                Phase::Down => ProbeMsg::Pull {
                    id,
                    bytes: MAX_PIECE,
                },
                _ => ProbeMsg::Push {
                    id,
                    payload: vec![0; MAX_PIECE as usize],
                },
            };
            send(tx, &msg).await?;
        }
        Ok(())
    }

    fn finish(&mut self, test: Test) {
        let result = ProbeResult {
            at_unix: test.started_unix,
            path: test.path,
            idle: test.idle,
            download: test.download,
            upload: test.upload,
            automatic: test.automatic,
        };
        tracing::info!(
            peer = %self.peer_name,
            down_bytes_per_sec = result.download.bytes_per_sec,
            up_bytes_per_sec = result.upload.bytes_per_sec,
            idle_p50_us = result.idle.p50_us,
            down_p95_us = result.download.loaded.p95_us,
            up_p95_us = result.upload.loaded.p95_us,
            "link tested"
        );
        if self.keeps {
            if let Err(error) = crate::probe_history::record(&self.peer_name, &result) {
                tracing::warn!(%error, "the link test's result could not be kept");
            }
        }
        if let Some(reply) = test.reply {
            let _ = reply.send(Ok(result));
        }
    }
}

async fn send(tx: &mut dyn TransportTx, msg: &ProbeMsg) -> Result<(), TransportError> {
    tx.send(&Message::module(ModuleId::PROBE, msg.priority(), msg)?)
        .await
}

#[async_trait]
impl SessionModule for ProbeEngine {
    fn name(&self) -> &'static str {
        "probe"
    }

    fn id(&self) -> ModuleId {
        ModuleId::PROBE
    }

    async fn start(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        for msg in std::mem::take(&mut self.owed) {
            send(tx, &msg).await?;
        }
        let now = Instant::now();
        self.maybe_auto(now);
        self.take_order();
        self.drive(tx, now).await
    }

    fn has_pending_work(&self) -> bool {
        self.test.is_some() || !self.owed.is_empty()
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<ProbeMsg>(ModuleId::PROBE) {
            Ok(Some(msg)) => {
                self.on_message(msg, Instant::now());
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable probe message from peer");
                true
            }
        }
    }
}

impl Drop for ProbeEngine {
    fn drop(&mut self) {
        let mut reg = registry();
        if reg.get(&self.peer).is_some_and(|r| r.token == self.token) {
            reg.remove(&self.peer);
        }
    }
}

/// Builds [`ProbeEngine`] on every link.
pub struct ProbeFactory;

impl crate::module::ModuleFactory for ProbeFactory {
    fn id(&self) -> ModuleId {
        ModuleId::PROBE
    }

    fn advert(&self) -> Option<ModuleAdvert> {
        Some(ModuleAdvert::new(
            ModuleId::PROBE,
            VERSION,
            ModuleRole::Both,
        ))
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        let peer = ctx.peer.peer_id;
        // A phone's link is tested when someone asks: by itself, a test would
        // spend its battery and its data for a figure nobody looked at.
        let auto = ctx.local.peer_id < peer
            && ctx.peer.device_kind != DeviceKind::Phone
            && crate::session::local_device_kind() != DeviceKind::Phone;
        Ok(Box::new(ProbeEngine::new(
            peer,
            ctx.peer_name().to_string(),
            auto,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::RecordingTx;

    #[tokio::test]
    async fn a_test_measures_both_directions_and_the_round_trips_between_two_ends() {
        let mut here = ProbeEngine::new(PeerId::from_bytes([0x81; 16]), "Probed".into(), false);
        let mut there = ProbeEngine::new(PeerId::from_bytes([0x82; 16]), "Prober".into(), false);
        here.keeps = false;
        there.keeps = false;
        let test = tokio::spawn(run("Probed", 1));
        let (mut to_there, mut to_here) = (RecordingTx::default(), RecordingTx::default());
        let mut fills = 0;
        let mut pushes = 0;
        while !test.is_finished() {
            here.pump(&mut to_there).await.unwrap();
            for msg in to_there.sent.drain(..) {
                if let Ok(Some(ProbeMsg::Push { .. })) = msg.decode_module(ModuleId::PROBE) {
                    pushes += 1;
                }
                assert!(there.handle_message(&msg));
            }
            there.pump(&mut to_here).await.unwrap();
            for msg in to_here.sent.drain(..) {
                if let Ok(Some(ProbeMsg::Fill { .. })) = msg.decode_module(ModuleId::PROBE) {
                    fills += 1;
                }
                assert!(here.handle_message(&msg));
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let (peer, result) = test.await.unwrap().unwrap();
        assert_eq!(peer, "Probed");
        assert!(
            fills > WINDOW && pushes > WINDOW,
            "{fills} fills, {pushes} pushes, {result:?}"
        );
        assert!(result.download.bytes_per_sec > 0 && result.upload.bytes_per_sec > 0);
        assert!(
            result.idle.sent >= 10 && result.idle.lost <= 1,
            "{:?}",
            result.idle
        );
        assert!(result.download.loaded.sent > 0 && result.upload.loaded.sent > 0);
        assert!(!result.automatic);
    }

    #[tokio::test]
    async fn a_second_test_while_one_runs_is_refused() {
        let _engine = ProbeEngine::new(PeerId::from_bytes([0x83; 16]), "Busy".into(), false);
        let mut engine = ProbeEngine::new(PeerId::from_bytes([0x84; 16]), "Busy2".into(), false);
        engine.keeps = false;
        let first = tokio::spawn(run("Busy2", 1));
        let second = tokio::spawn(run("Busy2", 1));
        let mut tx = RecordingTx::default();
        while !second.is_finished() && !first.is_finished() {
            engine.pump(&mut tx).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let refused = if second.is_finished() { second } else { first };
        assert_eq!(
            refused.await.unwrap().unwrap_err(),
            "a test of this link is already running"
        );
    }

    #[test]
    fn an_unknown_machine_is_named_in_the_refusal() {
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run("Nobody", 1))
            .unwrap_err();
        assert!(error.contains("Nobody"), "{error}");
    }
}
