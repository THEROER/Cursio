//! A user that is not Cursio: three classes of its own on two lanes — a
//! console and its metrics on an interactive lane, file chunks on a bulk
//! lane — over the in-memory carrier, through the public API alone.

use std::sync::Arc;
use std::time::Duration;

use cursio_channel::mem::MemCarrier;
use cursio_channel::{
    Carrier, ChannelError, Class, ClassId, Classes, Delivery, Envelope, Lane, LaneId, NoisePsk,
    Path, PathKind, PrioritizedTx, ResumableLink, ResumableRx, ResumeHandle, Role, Trust,
};

const CONSOLE: ClassId = 0;
const METRICS: ClassId = 1;
const FILES: ClassId = 2;
const BULK: LaneId = LaneId::new(1);

fn classes() -> Arc<Classes> {
    let lane = |name, paced| Lane {
        name,
        path_name: name,
        writer_depth: 8,
        paced,
        standby_skips_fresh: false,
    };
    let class = |name, lane, urgent| Class {
        name,
        lane,
        budget: 1024 * 1024,
        urgent,
    };
    Arc::new(
        Classes::new(
            vec![lane("console", false), lane("bulk", true)],
            vec![
                class("console", LaneId::INTERACTIVE, true),
                class("metrics", LaneId::INTERACTIVE, false),
                class("files", BULK, false),
            ],
        )
        .unwrap(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Panel {
    /// Keystrokes and output: reliable.
    Console(String),
    /// A sample of a server's load: worth nothing once replaced.
    Load(u32),
    /// One piece of a backup.
    Chunk(u32, Vec<u8>),
}

impl Envelope for Panel {
    fn class(&self) -> ClassId {
        match self {
            Panel::Console(_) => CONSOLE,
            Panel::Load(_) => METRICS,
            Panel::Chunk(..) => FILES,
        }
    }

    fn delivery(&self) -> Delivery {
        match self {
            Panel::Load(_) => Delivery::Fresh {
                within: Some(Duration::from_secs(5)),
            },
            _ => Delivery::Reliable,
        }
    }

    fn encode(&self) -> Result<Vec<u8>, ChannelError> {
        let mut out = Vec::new();
        match self {
            Panel::Console(text) => {
                out.push(b'c');
                out.extend_from_slice(text.as_bytes());
            }
            Panel::Load(n) => {
                out.push(b'l');
                out.extend_from_slice(&n.to_be_bytes());
            }
            Panel::Chunk(n, bytes) => {
                out.push(b'f');
                out.extend_from_slice(&n.to_be_bytes());
                out.extend_from_slice(bytes);
            }
        }
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self, ChannelError> {
        let bad = || ChannelError::Envelope("not a panel message".into());
        let (&tag, rest) = bytes.split_first().ok_or_else(bad)?;
        let number = || -> Result<u32, ChannelError> {
            Ok(u32::from_be_bytes(
                rest.get(..4).ok_or_else(bad)?.try_into().unwrap(),
            ))
        };
        match tag {
            b'c' => Ok(Panel::Console(
                String::from_utf8(rest.to_vec()).map_err(|_| bad())?,
            )),
            b'l' => Ok(Panel::Load(number()?)),
            b'f' => Ok(Panel::Chunk(number()?, rest[4..].to_vec())),
            _ => Err(bad()),
        }
    }
}

async fn path_pair(kind: PathKind) -> (Path, Path, [u8; 16]) {
    let (mut a, mut b) = MemCarrier::pair();
    let trust = NoisePsk::from_secret(b"panel", b"a secret the two ends share");
    let (oa, ob) = tokio::join!(
        trust.open(&mut a, Role::Initiator),
        trust.open(&mut b, Role::Responder)
    );
    let (oa, ob) = (oa.unwrap(), ob.unwrap());
    let tag = oa.session_tag;
    let (a, b): (Box<dyn Carrier>, Box<dyn Carrier>) = (Box::new(a), Box::new(b));
    (
        Path::new(a, oa.seal, kind, false),
        Path::new(b, ob.seal, kind, false),
        tag,
    )
}

struct End {
    tx: PrioritizedTx<Panel>,
    rx: ResumableRx<Panel>,
    handle: ResumeHandle,
}

async fn join_every_lane(a: &ResumeHandle, b: &ResumeHandle) {
    while !a.network_complete() {
        let (pa, pb, _) = path_pair(PathKind::INTERACTIVE).await;
        let (ra, rb) = tokio::join!(a.attach(pa), b.attach(pb));
        ra.unwrap();
        rb.unwrap();
    }
}

async fn session() -> (End, End) {
    let (a, b, tag) = path_pair(PathKind::INTERACTIVE).await;
    let (link_a, handle_a) = ResumableLink::new(a, tag, classes());
    let (link_b, handle_b) = ResumableLink::new(b, tag, classes());
    join_every_lane(&handle_a, &handle_b).await;
    let ((tx_a, rx_a), (tx_b, rx_b)) = (link_a.split(), link_b.split());
    (
        End {
            tx: tx_a,
            rx: rx_a,
            handle: handle_a,
        },
        End {
            tx: tx_b,
            rx: rx_b,
            handle: handle_b,
        },
    )
}

async fn next(end: &mut End) -> Panel {
    tokio::time::timeout(Duration::from_secs(5), end.rx.recv())
        .await
        .expect("a message within five seconds")
        .unwrap()
}

#[tokio::test]
async fn a_channel_of_its_own_classes_carries_each_on_its_lane() {
    let (mut agent, mut panel) = session().await;
    assert!(agent.handle.paths().network);
    let backup = Panel::Chunk(1, vec![7; 300_000]);
    agent.tx.send(&backup).await.unwrap();
    agent.tx.send(&Panel::Console("$ ls".into())).await.unwrap();
    agent.tx.send(&Panel::Load(42)).await.unwrap();
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(next(&mut panel).await);
    }
    assert!(got.contains(&backup));
    assert!(got.contains(&Panel::Console("$ ls".into())));
    assert!(got.contains(&Panel::Load(42)));

    panel.tx.send(&Panel::Console("ok".into())).await.unwrap();
    assert_eq!(next(&mut agent).await, Panel::Console("ok".into()));
}

#[tokio::test]
async fn after_a_dropped_connection_reliable_arrives_once_and_fresh_is_not_replayed() {
    let (mut agent, mut panel) = session().await;
    agent
        .tx
        .send(&Panel::Console("before".into()))
        .await
        .unwrap();
    assert_eq!(next(&mut panel).await, Panel::Console("before".into()));

    agent.handle.network_lost();
    panel.handle.network_lost();
    assert!(agent.tx.holding(), "with no path the session is held");
    agent.tx.send(&Panel::Load(1)).await.unwrap();
    agent
        .tx
        .send(&Panel::Console("while down".into()))
        .await
        .unwrap();
    let chunk = Panel::Chunk(2, vec![3; 100_000]);
    agent.tx.send(&chunk).await.unwrap();

    join_every_lane(&agent.handle, &panel.handle).await;
    agent
        .tx
        .send(&Panel::Console("after".into()))
        .await
        .unwrap();

    let mut console = Vec::new();
    let mut chunks = Vec::new();
    while console.len() < 2 || chunks.is_empty() {
        match next(&mut panel).await {
            Panel::Console(text) => console.push(text),
            Panel::Chunk(n, bytes) => chunks.push(Panel::Chunk(n, bytes)),
            Panel::Load(n) => panic!("a load sample from while the path was down came back: {n}"),
        }
    }
    assert_eq!(console, ["while down", "after"]);
    assert_eq!(chunks, [chunk]);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), panel.rx.recv())
            .await
            .is_err(),
        "nothing arrived twice"
    );
}
