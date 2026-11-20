//! What a session owes the application about its own sending: a message past
//! its deadline reported rather than delivered late, and everything never
//! acknowledged handed back whole when the session ends.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use cursio_channel::mem::MemCarrier;
use cursio_channel::{
    Carrier, CarrierRx, CarrierTx, ChannelError, NoisePsk, Path, PathKind, PrioritizedTx,
    ResumableLink, ResumableRx, ResumeHandle, Role, SessionEvent, SessionEvents, SessionOptions,
    Trust,
};

/// Where the agent's frames go once its network fails without either end
/// hearing of it: it lets `big` more large frames through, then loses every
/// frame on every connection.
#[derive(Default)]
struct Hole {
    big: AtomicUsize,
    closed: AtomicBool,
}

impl Hole {
    fn open() -> Arc<Self> {
        Arc::new(Self {
            big: AtomicUsize::new(usize::MAX),
            closed: AtomicBool::new(false),
        })
    }

    fn passes(&self, frame: &[u8]) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        if frame.len() < 1024 {
            return true;
        }
        let mut left = self.big.load(Ordering::SeqCst);
        loop {
            if left == 0 {
                self.closed.store(true, Ordering::SeqCst);
                return false;
            }
            match self
                .big
                .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(now) => left = now,
            }
        }
    }
}

struct Choked {
    inner: MemCarrier,
    hole: Arc<Hole>,
}

struct ChokedTx {
    inner: Box<dyn CarrierTx>,
    hole: Arc<Hole>,
}

#[async_trait]
impl CarrierTx for ChokedTx {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        if self.hole.passes(&frame) {
            self.inner.send(frame).await
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl Carrier for Choked {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        self.inner.send(frame).await
    }

    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError> {
        self.inner.recv().await
    }

    fn kind(&self) -> &'static str {
        "choked"
    }

    fn split(self: Box<Self>) -> (Box<dyn CarrierTx>, Box<dyn CarrierRx>) {
        let (tx, rx) = Box::new(self.inner).split();
        (
            Box::new(ChokedTx {
                inner: tx,
                hole: self.hole,
            }),
            rx,
        )
    }
}

/// One connection whose agent-to-panel direction goes through `hole`.
async fn path_pair(hole: &Arc<Hole>) -> (Path, Path, [u8; 16]) {
    let (mut a, mut b) = MemCarrier::pair();
    let trust = NoisePsk::from_secret(b"panel", b"a secret the two ends share");
    let (oa, ob) = tokio::join!(
        trust.open(&mut a, Role::Initiator),
        trust.open(&mut b, Role::Responder)
    );
    let (oa, ob) = (oa.unwrap(), ob.unwrap());
    let a = Choked {
        inner: a,
        hole: hole.clone(),
    };
    let (a, b): (Box<dyn Carrier>, Box<dyn Carrier>) = (Box::new(a), Box::new(b));
    (
        Path::new(a, oa.seal, PathKind::INTERACTIVE, false),
        Path::new(b, ob.seal, PathKind::INTERACTIVE, false),
        oa.session_tag,
    )
}

struct End {
    tx: PrioritizedTx<Panel>,
    rx: ResumableRx<Panel>,
    handle: ResumeHandle,
    events: SessionEvents<Panel>,
    hole: Arc<Hole>,
}

async fn session() -> (End, End) {
    let hole = Hole::open();
    let (a, p, tag) = path_pair(&hole).await;
    let start = |path, tag| {
        let options = SessionOptions::default().hand_back();
        let (mut link, handle) =
            ResumableLink::with_options(path, tag, classes(), options).unwrap();
        let events = link.events().unwrap();
        let (tx, rx) = link.split();
        End {
            tx,
            rx,
            handle,
            events,
            hole: hole.clone(),
        }
    };
    let (agent, panel) = (start(a, tag), start(p, tag));
    join_every_lane(&agent, &panel).await;
    (agent, panel)
}

async fn join_every_lane(agent: &End, panel: &End) {
    while !agent.handle.network_complete() {
        let (a, p, _) = path_pair(&agent.hole).await;
        let (ra, rp) = tokio::join!(agent.handle.attach(a), panel.handle.attach(p));
        ra.unwrap();
        rp.unwrap();
    }
}

async fn next(end: &mut End) -> Panel {
    tokio::time::timeout(Duration::from_secs(5), end.rx.recv())
        .await
        .expect("a message within five seconds")
        .unwrap()
}

async fn event(end: &mut End) -> SessionEvent<Panel> {
    tokio::time::timeout(Duration::from_secs(5), end.events.recv())
        .await
        .expect("a report within five seconds")
        .expect("the session still reports")
}

#[tokio::test]
async fn a_reliable_message_past_its_expires_at_is_reported_expired_and_not_delivered() {
    let (mut agent, mut panel) = session().await;
    agent.handle.network_lost();
    panel.handle.network_lost();

    let late = Panel::Console(
        "restart the server".into(),
        Some(deadline_in(Duration::from_millis(300))),
    );
    agent.tx.send(&late).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    join_every_lane(&agent, &panel).await;
    agent.tx.send(&line("after")).await.unwrap();

    assert_eq!(event(&mut agent).await, SessionEvent::Expired(late));
    assert_eq!(next(&mut panel).await, line("after"));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), panel.rx.recv())
            .await
            .is_err(),
        "the expired command never arrives"
    );
}

#[tokio::test]
async fn a_message_still_in_time_crosses_a_resume_as_any_other() {
    let (mut agent, mut panel) = session().await;
    agent.handle.network_lost();
    panel.handle.network_lost();
    let timely = Panel::Console("status".into(), Some(deadline_in(Duration::from_secs(60))));
    agent.tx.send(&timely).await.unwrap();
    join_every_lane(&agent, &panel).await;
    assert_eq!(next(&mut panel).await, timely);
}

#[tokio::test]
async fn a_session_that_ends_hands_back_what_was_never_acknowledged() {
    let (mut agent, mut panel) = session().await;
    agent.tx.send(&line("delivered")).await.unwrap();
    assert_eq!(next(&mut panel).await, line("delivered"));

    // Two pieces of the backup get through, and the panel acknowledges
    // them; then the agent's network goes, with nobody told, and nothing
    // more arrives — the backup's end, nor the lines after it.
    agent.hole.big.store(2, Ordering::SeqCst);
    let backup = Panel::Chunk(7, (0..200_000u32).map(|n| n as u8).collect());
    agent.tx.send(&backup).await.unwrap();
    while !agent.hole.closed.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    agent.tx.send(&line("one")).await.unwrap();
    agent.tx.send(&line("two")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    agent.handle.end("the operator closed the panel");
    match event(&mut agent).await {
        SessionEvent::Ended {
            reason,
            unacknowledged,
        } => {
            assert_eq!(reason, "the operator closed the panel");
            assert_eq!(unacknowledged.len(), 3, "{unacknowledged:?}");
            assert!(
                unacknowledged.contains(&backup),
                "the backup comes back whole"
            );
            let lines: Vec<_> = unacknowledged.iter().filter(|m| **m != backup).collect();
            assert_eq!(lines, [&line("one"), &line("two")]);
        }
        other => panic!("expected the end, got {other:?}"),
    }
}
