//! A session bound to the identity that opened it: who may join it, and what
//! ends it — a proof expiring, a serial revoked while the session is only
//! held, a fresh proof of someone else — and what it may send.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use cursio_channel::{
    Authorization, PeerIdentity, PrioritizedTx, ResumableLink, ResumableRx, ResumeHandle,
    SessionEvent, SessionEvents, SessionOptions, Unauthorized, Verified,
};

struct End {
    tx: PrioritizedTx<Panel>,
    rx: ResumableRx<Panel>,
    handle: ResumeHandle,
    events: SessionEvents<Panel>,
}

fn options(issuer: &Arc<Issuer>, policy: Option<&Authorization>) -> SessionOptions {
    let options = SessionOptions::bound_to(issuer.clone()).check_every(Duration::from_millis(200));
    match policy {
        Some(policy) => options.authorize(policy.clone()),
        None => options,
    }
}

/// The agent proves `agent` to the panel; the panel proves `panel`.
async fn session(issuer: &Arc<Issuer>, agent_proof: Verified) -> (End, End) {
    session_under(issuer, agent_proof, None).await
}

/// As [`session`], each end holding the other to `policy`.
async fn session_under(
    issuer: &Arc<Issuer>,
    agent_proof: Verified,
    policy: Option<Authorization>,
) -> (End, End) {
    let panel_proof = Issuer::proof("panel", "p1", Duration::from_secs(3600));
    let (a, p, tag) = path_pair(panel_proof.clone(), agent_proof.clone()).await;
    let start = |path, tag| {
        let options = options(issuer, policy.as_ref());
        let (mut link, handle) =
            ResumableLink::with_options(path, tag, classes(), options).unwrap();
        let events = link.events().unwrap();
        let (tx, rx) = link.split();
        End {
            tx,
            rx,
            handle,
            events,
        }
    };
    let (agent, panel) = (start(a, tag), start(p, tag));
    while !agent.handle.network_complete() {
        let (a, p, _) = path_pair(panel_proof.clone(), agent_proof.clone()).await;
        let (ra, rp) = tokio::join!(agent.handle.attach(a), panel.handle.attach(p));
        ra.unwrap();
        rp.unwrap();
    }
    (agent, panel)
}

async fn next(end: &mut End) -> Panel {
    tokio::time::timeout(Duration::from_secs(5), end.rx.recv())
        .await
        .expect("a message within five seconds")
        .unwrap()
}

async fn ended(end: &mut End) -> String {
    let event = tokio::time::timeout(Duration::from_secs(5), end.events.recv())
        .await
        .expect("the session to end within five seconds");
    match event {
        Some(SessionEvent::Ended { reason, .. }) => reason,
        other => panic!("expected the session to end, got {other:?}"),
    }
}

fn agent_for(lasts: Duration) -> Verified {
    Issuer::proof("agent", "a1", lasts)
}

#[tokio::test]
async fn a_resume_attempted_by_another_peer_is_refused() {
    let issuer = Arc::new(Issuer::default());
    let (mut agent, mut panel) = session(&issuer, agent_for(Duration::from_secs(3600))).await;

    let intruder = Issuer::proof("intruder", "i1", Duration::from_secs(3600));
    let panel_proof = Issuer::proof("panel", "p1", Duration::from_secs(3600));
    let (a, p, _) = path_pair(panel_proof, intruder).await;
    let (ra, rp) = tokio::join!(agent.handle.attach(a), panel.handle.attach(p));
    let refused = rp.expect_err("the panel must refuse a connection proving another host");
    assert!(refused.to_string().contains("intruder"), "{refused}");
    assert!(ra.is_err(), "the far end hears no RESUME and gives up");

    assert!(!panel.handle.is_ended(), "a refused stranger ends nothing");
    agent.tx.send(&line("still here")).await.unwrap();
    assert_eq!(next(&mut panel).await, line("still here"));
    panel.tx.send(&line("and here")).await.unwrap();
    assert_eq!(next(&mut agent).await, line("and here"));
}

#[tokio::test]
async fn a_certificate_expiring_mid_stream_closes_the_session() {
    let issuer = Arc::new(Issuer::default());
    let (agent, mut panel) = session(&issuer, agent_for(Duration::from_millis(1500))).await;
    assert!(panel.handle.deadline().is_some());

    let mut tx = agent.tx.clone();
    let streaming = tokio::spawn(async move {
        for n in 0.. {
            if tx.send(&Panel::Chunk(n, vec![1; 4096])).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    assert!(matches!(next(&mut panel).await, Panel::Chunk(..)));

    let why = ended(&mut panel).await;
    assert!(why.contains("expired"), "{why}");
    assert!(panel.handle.is_ended());
    streaming.abort();
}

#[tokio::test]
async fn a_serial_added_to_the_deny_list_closes_a_held_session() {
    let issuer = Arc::new(Issuer::default());
    let (agent, mut panel) = session(&issuer, agent_for(Duration::from_secs(3600))).await;
    agent.handle.network_lost();
    panel.handle.network_lost();
    assert!(panel.tx.holding(), "with no path the session is held");

    issuer.deny("a1");
    let why = ended(&mut panel).await;
    assert!(why.contains("deny list"), "{why}");
    assert!(
        !agent.handle.is_ended(),
        "the agent's view of the panel was not revoked"
    );
}

#[tokio::test]
async fn reauth_with_the_same_identity_extends_the_session() {
    let issuer = Arc::new(Issuer::default());
    let (mut agent, mut panel) = session(&issuer, agent_for(Duration::from_millis(1500))).await;
    let renewed = Issuer::proof("agent", "a2", Duration::from_secs(3600));
    agent
        .handle
        .reauthenticate(Issuer::write(&renewed))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert!(!panel.handle.is_ended(), "the fresh proof carried it on");
    assert!(panel.handle.deadline().unwrap() > renewed.not_after.unwrap() - Duration::from_secs(1));
    agent.tx.send(&line("past the old deadline")).await.unwrap();
    assert_eq!(next(&mut panel).await, line("past the old deadline"));
    panel.tx.send(&line("heard")).await.unwrap();
    assert_eq!(next(&mut agent).await, line("heard"));
}

#[tokio::test]
async fn reauth_with_another_identity_ends_the_session() {
    let issuer = Arc::new(Issuer::default());
    let (agent, mut panel) = session(&issuer, agent_for(Duration::from_secs(3600))).await;
    let other = Issuer::proof("someone-else", "s1", Duration::from_secs(3600));
    agent
        .handle
        .reauthenticate(Issuer::write(&other))
        .await
        .unwrap();
    let why = ended(&mut panel).await;
    assert!(why.contains("someone-else"), "{why}");
}

/// Only the panel may send files; anyone may use the console.
fn files_only_from_the_panel(enforce: bool) -> Authorization {
    Authorization::new(enforce, |who, lane, class| {
        class == CONSOLE || (lane == BULK && *who == PeerIdentity::Host { id: "panel".into() })
    })
}

#[tokio::test]
async fn an_authorization_violation_is_only_reported_when_not_enforcing() {
    let issuer = Arc::new(Issuer::default());
    let policy = files_only_from_the_panel(false);
    let agent_proof = agent_for(Duration::from_secs(3600));
    let (mut agent, mut panel) = session_under(&issuer, agent_proof, Some(policy)).await;
    agent.tx.send(&line("allowed")).await.unwrap();
    assert_eq!(next(&mut panel).await, line("allowed"));
    let chunk = Panel::Chunk(1, vec![5; 100]);
    agent.tx.send(&chunk).await.unwrap();
    assert_eq!(
        next(&mut panel).await,
        chunk,
        "delivered, since not enforced"
    );

    let report = tokio::time::timeout(Duration::from_secs(5), panel.events.recv())
        .await
        .unwrap();
    assert_eq!(
        report,
        Some(SessionEvent::Unauthorized(Unauthorized {
            identity: PeerIdentity::Host { id: "agent".into() },
            lane: BULK,
            class: FILES,
            enforced: false,
        }))
    );
    panel.tx.send(&Panel::Chunk(2, vec![6; 100])).await.unwrap();
    assert_eq!(next(&mut agent).await, Panel::Chunk(2, vec![6; 100]));
    assert!(!panel.handle.is_ended());
}

#[tokio::test]
async fn an_authorization_violation_closes_the_session_when_enforcing() {
    let issuer = Arc::new(Issuer::default());
    let policy = files_only_from_the_panel(true);
    let agent_proof = agent_for(Duration::from_secs(3600));
    let (mut agent, mut panel) = session_under(&issuer, agent_proof, Some(policy)).await;
    agent.tx.send(&Panel::Chunk(1, vec![5; 100])).await.unwrap();
    let delivered = tokio::time::timeout(Duration::from_secs(5), panel.rx.recv())
        .await
        .unwrap();
    assert!(delivered.is_err(), "the refused message is not delivered");
    match panel.events.recv().await {
        Some(SessionEvent::Unauthorized(report)) => {
            assert!(report.enforced);
            assert_eq!(report.lane, BULK);
        }
        other => panic!("expected a report, got {other:?}"),
    }
    let why = ended(&mut panel).await;
    assert!(why.contains("may not send"), "{why}");
}

#[tokio::test]
async fn a_policy_without_an_identity_to_hold_to_is_refused() {
    let (a, _, tag) = path_pair(
        Issuer::proof("panel", "p1", Duration::from_secs(60)),
        Issuer::proof("agent", "a1", Duration::from_secs(60)),
    )
    .await;
    let options = SessionOptions::default().authorize(files_only_from_the_panel(true));
    assert!(ResumableLink::<Panel>::with_options(a, tag, classes(), options).is_err());
}
