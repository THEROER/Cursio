//! A direct Wi-Fi link to a nearby peer, agreed over the session it then
//! carries.
//!
//! The session already runs through the router. When the two machines can —
//! one runs a group, the other has a second station interface to join it with,
//! and their own Wi-Fi is on one channel — this module puts the group up,
//! hands its passphrase across inside the session's Noise channel, and tells
//! the mesh the group address to dial. The connections that come of it join
//! the same session as direct paths (`cursio_transport::resume`), which carry
//! it while they are healthy; the router's paths stay beside them, so the
//! machines moving apart costs a path, not the session.
//!
//! The host paces retries: a group nobody joined, or one the joiner could not
//! reach or lost, is taken down and offered again after a pause that doubles.
//! The joiner only ever answers a group it was offered. One side keeps the
//! clock, so the two never wait on each other's timers.
//!
//! Mechanism is `cursio_wifidirect`; every call into it blocks (D-Bus, WinRT),
//! so the long ones run on the blocking pool and the pump only looks at them.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use cursio_protocol::direct::DirectMsg;
use cursio_protocol::{Message, ModuleAdvert, ModuleId, ModuleRole, PeerId};
use cursio_transport::{TransportError, TransportTx};
use cursio_wifidirect::{Ability, HostedGroup, Joined};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::module::SessionModule;

pub const VERSION: u16 = 1;

/// How often this machine's ability is read again: its Wi-Fi may change
/// channel, network or interface under it.
const READ_EVERY: Duration = Duration::from_secs(30);
/// How long a group waits for its joiner before it is taken down.
const JOIN_WAIT: Duration = Duration::from_secs(75);
/// How often a joined link is checked for still being up.
const CHECK_EVERY: Duration = Duration::from_secs(2);
/// The first pause after a failed attempt; it doubles up to [`RETRY_MAX`].
const RETRY: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(30 * 60);

/// Group addresses the mesh dials, by peer: set while this machine is on that
/// peer's group.
fn dial_table() -> std::sync::MutexGuard<'static, HashMap<PeerId, IpAddr>> {
    static TABLE: std::sync::OnceLock<Mutex<HashMap<PeerId, IpAddr>>> = std::sync::OnceLock::new();
    TABLE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Every peer this machine can reach over a direct link now, with the address
/// its mesh listener answers at on the group.
pub fn dial_ips() -> Vec<(PeerId, IpAddr)> {
    dial_table().iter().map(|(p, ip)| (*p, *ip)).collect()
}

/// Hold `addr` as one of this machine's direct-link addresses. Counted: a
/// host's one group address serves every machine that joins it.
fn hold_local(addr: Ipv4Addr, on: bool) {
    static HELD: Mutex<Option<HashMap<Ipv4Addr, usize>>> = Mutex::new(None);
    let mut guard = HELD.lock().unwrap_or_else(|p| p.into_inner());
    let held = guard.get_or_insert_with(HashMap::new);
    let count = held.entry(addr).or_insert(0);
    if on {
        *count += 1;
        if *count == 1 {
            cursio_transport::direct::set_local(IpAddr::V4(addr), true);
        }
    } else if *count > 0 {
        *count -= 1;
        if *count == 0 {
            held.remove(&addr);
            cursio_transport::direct::set_local(IpAddr::V4(addr), false);
        }
    }
}

/// What one end can do, as the other heard it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Can {
    host: bool,
    join: bool,
    freq_mhz: Option<u32>,
}

impl From<&Ability> for Can {
    fn from(a: &Ability) -> Self {
        Self {
            host: a.host,
            join: a.join,
            freq_mhz: a.freq_mhz,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    Host,
    Join,
    Neither(String),
}

/// Who runs the group, derived alike on both ends from the same two answers.
fn plan(local: PeerId, mine: Can, peer: PeerId, theirs: Can) -> Plan {
    if !cursio_wifidirect::same_channel(mine.freq_mhz, theirs.freq_mhz) {
        return Plan::Neither(format!(
            "the two machines' Wi-Fi are on different channels ({} and {} MHz); a direct link would make each card split its time between two",
            mine.freq_mhz.unwrap_or(0),
            theirs.freq_mhz.unwrap_or(0)
        ));
    }
    let i_host = mine.host && theirs.join;
    let they_host = theirs.host && mine.join;
    match (i_host, they_host) {
        (true, true) if local < peer => Plan::Host,
        (true, true) => Plan::Join,
        (true, false) => Plan::Host,
        (false, true) => Plan::Join,
        (false, false) => {
            Plan::Neither("neither machine can run a group the other can join".to_string())
        }
    }
}

enum State {
    Idle,
    Starting(JoinHandle<cursio_wifidirect::Result<HostedGroup>>),
    Hosting {
        group: HostedGroup,
        since: Instant,
        /// The group's own address, once the joiner said it; held as direct.
        held: Option<Ipv4Addr>,
    },
    Joining(JoinHandle<cursio_wifidirect::Result<Joined>>),
    Joined {
        link: Joined,
        next_check: Instant,
    },
}

pub struct DirectEngine {
    peer: PeerId,
    local: PeerId,
    peer_name: String,
    mine: Option<Can>,
    told: Option<Can>,
    theirs: Option<Can>,
    reading: Option<JoinHandle<Ability>>,
    next_read: Instant,
    state: State,
    retry_at: Instant,
    failures: u32,
    outbox: Vec<DirectMsg>,
    /// Why there is no link, said once each time it changes.
    said: Option<String>,
}

impl DirectEngine {
    pub fn new(local: PeerId, peer: PeerId, peer_name: String) -> Self {
        let now = Instant::now();
        Self {
            peer,
            local,
            peer_name,
            mine: None,
            told: None,
            theirs: None,
            reading: None,
            next_read: now,
            state: State::Idle,
            retry_at: now,
            failures: 0,
            outbox: Vec::new(),
            said: None,
        }
    }

    fn plan(&self) -> Option<Plan> {
        Some(plan(self.local, self.mine?, self.peer, self.theirs?))
    }

    fn say(&mut self, why: Option<String>) {
        if why != self.said {
            if let Some(why) = &why {
                tracing::info!(peer = %self.peer_name, %why, "no direct Wi-Fi link");
            }
            self.said = why;
        }
    }

    /// The host gave up on an attempt: pause before the next, longer each
    /// time in a row.
    fn back_off(&mut self, now: Instant) {
        self.failures += 1;
        let wait = RETRY
            .saturating_mul(1 << (self.failures - 1).min(8))
            .min(RETRY_MAX);
        self.retry_at = now + wait;
    }

    /// Take down whatever link this end has, telling the peer why.
    fn tear_down(&mut self, why: &str, tell: bool) {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => return,
            State::Starting(task) => task.abort(),
            State::Joining(task) => task.abort(),
            State::Hosting { held, .. } => {
                if let Some(addr) = held {
                    hold_local(addr, false);
                }
            }
            State::Joined { link, .. } => {
                dial_table().remove(&self.peer);
                hold_local(link.local, false);
                drop(link);
            }
        }
        tracing::info!(peer = %self.peer_name, %why, "direct Wi-Fi link down");
        cursio_transport::resume::direct_lost(why);
        if tell {
            self.outbox.push(DirectMsg::Left {
                why: why.to_string(),
            });
        }
    }

    fn on_message(&mut self, msg: DirectMsg, now: Instant) {
        match msg {
            DirectMsg::Able {
                host,
                join,
                freq_mhz,
            } => {
                self.theirs = Some(Can {
                    host,
                    join,
                    freq_mhz,
                });
            }
            DirectMsg::Group { ssid, passphrase } => {
                if self.plan() != Some(Plan::Join) {
                    self.outbox.push(DirectMsg::Left {
                        why: "this machine cannot join a group now".into(),
                    });
                    return;
                }
                if !matches!(self.state, State::Idle) {
                    self.tear_down("the peer offered a new group", false);
                }
                tracing::info!(peer = %self.peer_name, %ssid, "joining the peer's direct Wi-Fi group");
                self.state = State::Joining(tokio::task::spawn_blocking(move || {
                    cursio_wifidirect::join(&ssid, &passphrase)
                }));
            }
            DirectMsg::Joined { host, .. } => {
                if let State::Hosting { held, .. } = &mut self.state {
                    if held.is_none() {
                        let addr = Ipv4Addr::from(host);
                        hold_local(addr, true);
                        *held = Some(addr);
                        self.failures = 0;
                        tracing::info!(peer = %self.peer_name, %addr, "the peer joined the direct Wi-Fi group");
                    }
                }
            }
            DirectMsg::Left { why } => {
                let hosting = matches!(self.state, State::Hosting { .. } | State::Starting(_));
                self.tear_down(&format!("the peer left: {why}"), false);
                if hosting {
                    self.back_off(now);
                }
            }
        }
    }

    async fn step(&mut self, now: Instant) {
        if self.reading.as_ref().is_some_and(|r| r.is_finished()) {
            if let Some(Ok(ability)) = self.reading.take().map(futures_ready) {
                let can = Can::from(&ability);
                if !can.host && !can.join {
                    self.say(ability.why_not.clone());
                }
                self.mine = Some(can);
            }
        }
        if self.reading.is_none() && now >= self.next_read {
            self.reading = Some(tokio::task::spawn_blocking(cursio_wifidirect::ability));
            self.next_read = now + READ_EVERY;
        }
        if let Some(mine) = self.mine.filter(|m| Some(*m) != self.told) {
            self.outbox.push(DirectMsg::Able {
                host: mine.host,
                join: mine.join,
                freq_mhz: mine.freq_mhz,
            });
            self.told = Some(mine);
        }

        let plan = self.plan();
        match &plan {
            Some(Plan::Neither(why)) => {
                let why = why.clone();
                self.say(Some(why.clone()));
                self.tear_down(&why, true);
            }
            Some(_) => self.say(None),
            None => {}
        }

        match &mut self.state {
            State::Idle => {
                if plan == Some(Plan::Host) && now >= self.retry_at {
                    tracing::info!(peer = %self.peer_name, "offering the peer a direct Wi-Fi group");
                    self.state =
                        State::Starting(tokio::task::spawn_blocking(cursio_wifidirect::host));
                }
            }
            State::Starting(task) if task.is_finished() => {
                let State::Starting(task) = std::mem::replace(&mut self.state, State::Idle) else {
                    unreachable!()
                };
                match futures_ready(task) {
                    Ok(Ok(group)) => {
                        self.outbox.push(DirectMsg::Group {
                            ssid: group.ssid(),
                            passphrase: group.passphrase(),
                        });
                        self.state = State::Hosting {
                            group,
                            since: now,
                            held: None,
                        };
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(peer = %self.peer_name, error = %e, "could not run a direct Wi-Fi group");
                        self.back_off(now);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "the direct Wi-Fi group task failed");
                        self.back_off(now);
                    }
                }
            }
            State::Hosting { group, since, held } => {
                let why = if plan != Some(Plan::Host) {
                    Some("this machine no longer hosts the link")
                } else if !group.is_up() {
                    Some("Windows stopped the group")
                } else if held.is_none() && now.duration_since(*since) > JOIN_WAIT {
                    Some("nobody joined the group")
                } else {
                    None
                };
                if let Some(why) = why {
                    self.tear_down(why, true);
                    self.back_off(now);
                }
            }
            State::Joining(task) if task.is_finished() => {
                let State::Joining(task) = std::mem::replace(&mut self.state, State::Idle) else {
                    unreachable!()
                };
                match futures_ready(task) {
                    Ok(Ok(link)) => {
                        tracing::info!(
                            peer = %self.peer_name,
                            here = %link.local,
                            host = %link.host,
                            "on the peer's direct Wi-Fi group"
                        );
                        hold_local(link.local, true);
                        dial_table().insert(self.peer, IpAddr::V4(link.host));
                        self.outbox.push(DirectMsg::Joined {
                            host: link.host.octets(),
                            joiner: link.local.octets(),
                        });
                        self.state = State::Joined {
                            link,
                            next_check: now + CHECK_EVERY,
                        };
                    }
                    Ok(Err(e)) => {
                        tracing::info!(peer = %self.peer_name, error = %e, "could not join the peer's direct Wi-Fi group");
                        self.outbox.push(DirectMsg::Left { why: e.to_string() });
                    }
                    Err(e) => {
                        self.outbox.push(DirectMsg::Left {
                            why: format!("the join failed: {e}"),
                        });
                    }
                }
            }
            State::Joined { link, next_check } => {
                if plan != Some(Plan::Join) {
                    self.tear_down("this machine no longer joins the link", true);
                } else if now >= *next_check {
                    *next_check = now + CHECK_EVERY;
                    if !link.is_up() {
                        self.tear_down("the group went away", true);
                    }
                }
            }
            State::Starting(_) | State::Joining(_) => {}
        }
    }
}

/// The result of a task already finished. Polled once, it is ready.
fn futures_ready<T>(task: JoinHandle<T>) -> Result<T, tokio::task::JoinError> {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut task = std::pin::pin!(task);
    match task.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(r) => r,
        std::task::Poll::Pending => unreachable!("only finished tasks are taken"),
    }
}

use std::future::Future;

#[async_trait]
impl SessionModule for DirectEngine {
    fn name(&self) -> &'static str {
        "direct"
    }

    fn id(&self) -> ModuleId {
        ModuleId::DIRECT
    }

    async fn start(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.step(Instant::now()).await;
        for msg in std::mem::take(&mut self.outbox) {
            tx.send(&Message::module(ModuleId::DIRECT, msg.priority(), &msg)?)
                .await?;
        }
        Ok(())
    }

    fn has_pending_work(&self) -> bool {
        !self.outbox.is_empty()
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<DirectMsg>(ModuleId::DIRECT) {
            Ok(Some(msg)) => {
                self.on_message(msg, Instant::now());
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable direct-link message from peer");
                true
            }
        }
    }

    async fn shutdown(&mut self, tx: &mut dyn TransportTx) {
        self.tear_down("the session is ending", true);
        for msg in std::mem::take(&mut self.outbox) {
            if let Ok(msg) = Message::module(ModuleId::DIRECT, msg.priority(), &msg) {
                let _ = tx.send(&msg).await;
            }
        }
    }
}

impl Drop for DirectEngine {
    fn drop(&mut self) {
        self.tear_down("the session ended", false);
    }
}

/// Builds [`DirectEngine`] between two computers. A phone has no second
/// station interface an app may drive, and runs no group.
pub struct DirectFactory;

impl crate::module::ModuleFactory for DirectFactory {
    fn id(&self) -> ModuleId {
        ModuleId::DIRECT
    }

    fn advert(&self) -> Option<ModuleAdvert> {
        cfg!(any(target_os = "linux", windows))
            .then(|| ModuleAdvert::new(ModuleId::DIRECT, VERSION, ModuleRole::Both))
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(DirectEngine::new(
            ctx.local.peer_id,
            ctx.peer.peer_id,
            ctx.peer_name().to_string(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> PeerId {
        PeerId::from_bytes([n; 16])
    }

    const WINDOWS: Can = Can {
        host: true,
        join: false,
        freq_mhz: Some(5745),
    };
    const LINUX: Can = Can {
        host: false,
        join: true,
        freq_mhz: Some(5745),
    };

    #[test]
    fn the_machine_that_can_host_hosts_and_the_other_joins() {
        assert_eq!(plan(id(9), WINDOWS, id(1), LINUX), Plan::Host);
        assert_eq!(plan(id(1), LINUX, id(9), WINDOWS), Plan::Join);
    }

    #[test]
    fn two_machines_that_can_both_do_both_agree_on_the_lower_id_hosting() {
        let both = Can {
            host: true,
            join: true,
            freq_mhz: None,
        };
        assert_eq!(plan(id(1), both, id(2), both), Plan::Host);
        assert_eq!(plan(id(2), both, id(1), both), Plan::Join);
    }

    #[test]
    fn different_channels_rule_the_link_out() {
        let elsewhere = Can {
            freq_mhz: Some(5180),
            ..LINUX
        };
        assert!(matches!(
            plan(id(9), WINDOWS, id(1), elsewhere),
            Plan::Neither(_)
        ));
    }

    #[test]
    fn two_hosts_have_no_link() {
        assert!(matches!(
            plan(id(1), WINDOWS, id(2), WINDOWS),
            Plan::Neither(_)
        ));
    }

    #[test]
    fn a_shared_address_stays_direct_until_its_last_holder_lets_go() {
        let addr = Ipv4Addr::new(192, 0, 2, 9);
        hold_local(addr, true);
        hold_local(addr, true);
        hold_local(addr, false);
        assert!(cursio_transport::direct::is_local(IpAddr::V4(addr)));
        hold_local(addr, false);
        assert!(!cursio_transport::direct::is_local(IpAddr::V4(addr)));
    }

    #[test]
    fn a_failed_attempt_waits_longer_each_time_in_a_row() {
        let mut engine = DirectEngine::new(id(1), id(2), "peer".into());
        let now = Instant::now();
        engine.back_off(now);
        assert_eq!(engine.retry_at - now, RETRY);
        engine.back_off(now);
        assert_eq!(engine.retry_at - now, RETRY * 2);
        for _ in 0..20 {
            engine.back_off(now);
        }
        assert_eq!(engine.retry_at - now, RETRY_MAX);
    }
}
