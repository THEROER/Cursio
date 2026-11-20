//! A server panel and its user over WSS, in one process: what Leave starts
//! from.
//!
//! The panel listens for `wss://` with a certificate from a CA made on the
//! spot; the user dials it, proves itself with a token in the opening
//! exchange, and the two hold one bound session with a console lane and a
//! bulk lane. The user uploads a file in chunks while typing into the
//! console, and halfway through its connections are killed. The session is
//! held, the user dials again asking to resume, and the upload carries on.
//!
//! What it prints shows the three promises: the console answers in
//! milliseconds while the bulk lane is full; every chunk arrives exactly once
//! across the break; and the load samples sent while the connections were
//! down are not replayed when they come back.
//!
//! ```sh
//! cargo run -p cursio-channel --example panel
//! ```

#[path = "../tests/common/certs.rs"]
mod certs;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use cursio_channel::authenticated::PeerContext;
use cursio_channel::{
    ws, CarrierAuthenticated, ChannelError, Class, ClassId, Classes, Delivery, Envelope, Lane,
    LaneId, Path, PathKind, PeerIdentity, ResumableLink, ResumeHandle, Revoked, Role, SessionEvent,
    SessionOptions, Trust, Verified, Verifier,
};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const NAME: &str = "panel.example";
const CONSOLE: ClassId = 0;
const METRICS: ClassId = 1;
const FILES: ClassId = 2;
const BULK: LaneId = LaneId::new(1);
const CHUNK: usize = 256 * 1024;
const CHUNKS: u32 = 128;

fn classes() -> Arc<Classes> {
    let lane = |name, paced| Lane {
        name,
        path_name: name,
        writer_depth: 8,
        paced,
        standby_skips_fresh: false,
    };
    let class = |name, lane, budget, urgent| Class {
        name,
        lane,
        budget,
        urgent,
    };
    Arc::new(
        Classes::new(
            vec![lane("console", false), lane("bulk", true)],
            vec![
                class("console", LaneId::INTERACTIVE, 64 * 1024, true),
                class("metrics", LaneId::INTERACTIVE, 64 * 1024, false),
                class("files", BULK, 4 * 1024 * 1024, false),
            ],
        )
        .expect("a valid declaration"),
    )
}

#[derive(Debug, Clone)]
enum Msg {
    /// A line typed into the console, or its echo.
    Console(String),
    /// A server's load: worth nothing once a newer one exists.
    Load(u32),
    /// A piece of the upload.
    Chunk(u32, Vec<u8>),
}

impl Envelope for Msg {
    fn class(&self) -> ClassId {
        match self {
            Msg::Console(_) => CONSOLE,
            Msg::Load(_) => METRICS,
            Msg::Chunk(..) => FILES,
        }
    }

    fn delivery(&self) -> Delivery {
        match self {
            Msg::Load(_) => Delivery::Fresh {
                within: Some(Duration::from_secs(5)),
            },
            _ => Delivery::Reliable,
        }
    }

    fn encode(&self) -> Result<Vec<u8>, ChannelError> {
        let mut out = Vec::new();
        match self {
            Msg::Console(text) => {
                out.push(b'c');
                out.extend_from_slice(text.as_bytes());
            }
            Msg::Load(n) => {
                out.push(b'l');
                out.extend_from_slice(&n.to_be_bytes());
            }
            Msg::Chunk(n, bytes) => {
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
        let number = || {
            rest.get(..4)
                .map(|n| u32::from_be_bytes(n.try_into().unwrap()))
        };
        match tag {
            b'c' => Ok(Msg::Console(
                String::from_utf8(rest.to_vec()).map_err(|_| bad())?,
            )),
            b'l' => Ok(Msg::Load(number().ok_or_else(bad)?)),
            b'f' => Ok(Msg::Chunk(number().ok_or_else(bad)?, rest[4..].to_vec())),
            _ => Err(bad()),
        }
    }
}

/// The panel's rules: a user is whoever a token it issued names, for an
/// hour; the panel is whoever TLS found behind its name. Leave's would check
/// a JWT's signature, audience and `exp`, and ask introspection in `check`.
struct Rules;

#[async_trait]
impl Verifier for Rules {
    async fn verify(&self, peer: PeerContext<'_>) -> Result<Verified, Revoked> {
        match (peer.token, peer.tls) {
            (Some(b"ann-token"), _) => Ok(Verified {
                identity: PeerIdentity::User {
                    sub: "ann".into(),
                    aud: "panel".into(),
                },
                serial: Some("token-1".into()),
                not_after: Some(SystemTime::now() + Duration::from_secs(3600)),
            }),
            (Some(_), _) => Err(Revoked("a token the panel did not issue".into())),
            (None, Some(tls)) if !tls.peer_certificates.is_empty() => {
                Ok(Verified::lasting(PeerIdentity::Service { id: NAME.into() }))
            }
            (None, _) => Err(Revoked("no token and no certificate".into())),
        }
    }

    fn check(&self, _: &Verified, _: SystemTime) -> Result<(), Revoked> {
        Ok(())
    }
}

fn trust() -> CarrierAuthenticated {
    CarrierAuthenticated::new(Arc::new(Rules))
}

fn user_trust() -> CarrierAuthenticated {
    trust().presenting(b"ann-token".to_vec())
}

fn options(trust: CarrierAuthenticated) -> SessionOptions {
    SessionOptions::bound_to(Arc::new(trust)).hand_back()
}

/// The panel: every connection either starts the session or, offering the
/// held one's tag bound to its own TLS, joins it.
async fn serve(listener: TcpListener, certs: Arc<certs::Certs>) {
    let held: Arc<Mutex<Option<ResumeHandle>>> = Arc::default();
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            return;
        };
        let (config, held) = (certs.server.clone(), held.clone());
        tokio::spawn(async move {
            if let Err(error) = admit(tcp, config, held).await {
                println!("panel: refused a connection: {error}");
            }
        });
    }
}

async fn admit(
    tcp: tokio::net::TcpStream,
    config: Arc<cursio_channel::tls::ServerConfig>,
    held: Arc<Mutex<Option<ResumeHandle>>>,
) -> Result<(), ChannelError> {
    let mut carrier = ws::accept_wss(tcp, config).await?;
    let opened = trust().open(&mut carrier, Role::Responder).await?;
    let path = Path::new(Box::new(carrier), opened.seal, PathKind::INTERACTIVE, false)
        .proving(opened.peer.clone());
    let Some(offer) = opened.resume else {
        println!("panel: {} opened a session", opened.peer.identity);
        let (link, handle) =
            ResumableLink::with_options(path, opened.session_tag, classes(), options(trust()))?;
        *held.lock().await = Some(handle);
        tokio::spawn(run_panel(link));
        return Ok(());
    };
    let handle = held.lock().await.clone();
    match handle.filter(|handle| handle.named_by(&offer, &path)) {
        Some(handle) => handle.attach(path).await,
        None => Err(ChannelError::Other("it names no session held here".into())),
    }
}

/// The panel's side of the session: echo the console, take the upload, and
/// say at the end what arrived.
async fn run_panel(link: ResumableLink<Msg>) {
    let (mut tx, mut rx) = link.split();
    let mut chunks = vec![0u32; CHUNKS as usize];
    let mut loads = Vec::new();
    while let Ok(msg) = rx.recv().await {
        let reply = match msg {
            Msg::Console(line) => Some(Msg::Console(format!("echo {line}"))),
            Msg::Load(n) => {
                loads.push(n);
                None
            }
            Msg::Chunk(n, bytes) => {
                assert!(
                    bytes.iter().all(|b| *b == n as u8),
                    "chunk {n} arrived intact"
                );
                chunks[n as usize] += 1;
                let done = chunks.iter().all(|seen| *seen > 0);
                done.then(|| {
                    let twice = chunks.iter().filter(|seen| **seen > 1).count();
                    Msg::Console(format!(
                        "done: all {CHUNKS} chunks, {twice} twice; loads seen {loads:?}"
                    ))
                })
            }
        };
        if let Some(reply) = reply {
            if tx.send(&reply).await.is_err() {
                return;
            }
        }
    }
}

/// One WSS connection to the panel, opened by `trust`.
async fn dial(
    addr: SocketAddr,
    certs: &certs::Certs,
    trust: CarrierAuthenticated,
) -> Result<(Path, [u8; 16]), ChannelError> {
    let url = format!("wss://{NAME}:{}/panel", addr.port());
    let mut carrier = ws::connect_wss(addr, &url, NAME, certs.client.clone()).await?;
    let opened = trust.open(&mut carrier, Role::Initiator).await?;
    let path = Path::new(Box::new(carrier), opened.seal, PathKind::INTERACTIVE, false)
        .proving(opened.peer);
    Ok((path, opened.session_tag))
}

/// Dial until every lane has a connection of its own, each asking to join
/// the session known as `tag`.
async fn join_lanes(
    handle: &ResumeHandle,
    addr: SocketAddr,
    certs: &certs::Certs,
    tag: [u8; 16],
) -> Result<(), ChannelError> {
    while !handle.network_complete() {
        let (path, _) = dial(addr, certs, user_trust().resuming(tag)).await?;
        handle.attach(path).await?;
    }
    Ok(())
}

fn percentile(sorted: &[Duration], at: f64) -> Duration {
    sorted
        .get(((sorted.len() as f64 - 1.0) * at) as usize)
        .copied()
        .unwrap_or_default()
}

/// The numbers between `loads seen [` and `]` in the panel's last line.
fn loads_in(line: &str) -> Vec<u32> {
    let list = line
        .split_once("loads seen [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .map_or("", |(list, _)| list);
    list.split(", ").filter_map(|n| n.parse().ok()).collect()
}

#[tokio::main]
async fn main() -> Result<(), ChannelError> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let certs = Arc::new(certs::certs(NAME));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(serve(listener, certs.clone()));

    let (first, tag) = dial(addr, &certs, user_trust()).await?;
    let (mut link, handle) =
        ResumableLink::with_options(first, tag, classes(), options(user_trust()))?;
    let mut events = link.events().expect("taken once, before the split");
    join_lanes(&handle, addr, &certs, tag).await?;
    let (tx, mut rx) = link.split();
    println!(
        "user: session open over wss, uploading {} MiB",
        (CHUNKS as usize * CHUNK) >> 20
    );

    let (halfway, at_half) = tokio::sync::oneshot::channel();
    let mut upload = tx.clone();
    tokio::spawn(async move {
        let mut halfway = Some(halfway);
        for n in 0..CHUNKS {
            upload.send(&Msg::Chunk(n, vec![n as u8; CHUNK])).await?;
            if n == CHUNKS / 2 {
                halfway.take().map(|signal| signal.send(()));
            }
        }
        Ok::<_, ChannelError>(())
    });

    let start = Instant::now();
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let while_down = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut typing, typing_down, typing_while_down) =
        (tx.clone(), down.clone(), while_down.clone());
    tokio::spawn(async move {
        for n in 0u32.. {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let at = start.elapsed().as_micros();
            typing.send(&Msg::Console(format!("key {n} {at}"))).await?;
            typing.send(&Msg::Load(n)).await?;
            if typing_down.load(std::sync::atomic::Ordering::SeqCst) {
                typing_while_down.lock().unwrap().push(n);
            }
        }
        Ok::<_, ChannelError>(())
    });

    let killer = handle.clone();
    let rejoin_certs = certs.clone();
    tokio::spawn(async move {
        if at_half.await.is_err() {
            return;
        }
        println!("user: killing every connection halfway through the upload");
        killer.network_lost();
        down.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(1)).await;
        down.store(false, std::sync::atomic::Ordering::SeqCst);
        match join_lanes(&killer, addr, &rejoin_certs, tag).await {
            Ok(()) => println!("user: dialled again; the session resumed"),
            Err(error) => println!("user: could not resume: {error}"),
        }
    });

    let mut echoes = Vec::new();
    let done = loop {
        let msg = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .map_err(|_| ChannelError::Other("the panel went quiet".into()))??;
        let Msg::Console(line) = msg else { continue };
        if let Some(sent) = line.strip_prefix("echo key ") {
            let mut fields = sent.split(' ').map(|f| f.parse::<u64>().unwrap_or(0));
            let (key, micros) = (fields.next().unwrap_or(0), fields.next().unwrap_or(0));
            let waited = start
                .elapsed()
                .saturating_sub(Duration::from_micros(micros));
            echoes.push((key as u32, waited));
        } else if line.starts_with("done") {
            break line;
        }
    };
    println!("panel: {}", done.split(';').next().unwrap_or_default());

    let while_down = while_down.lock().unwrap().clone();
    let (mut across, mut beside): (Vec<_>, Vec<_>) =
        echoes.iter().partition(|(key, _)| while_down.contains(key));
    let waits = |echoes: &mut Vec<&(u32, Duration)>| {
        echoes.sort_by_key(|(_, waited)| *waited);
        echoes.iter().map(|(_, waited)| *waited).collect::<Vec<_>>()
    };
    let (across, beside) = (waits(&mut across), waits(&mut beside));
    println!(
        "console: {} keys echoed beside the upload, median {:?}, slowest {:?}; {} typed during the break, slowest {:?}",
        beside.len(),
        percentile(&beside, 0.5),
        beside.last().copied().unwrap_or_default(),
        across.len(),
        across.last().copied().unwrap_or_default(),
    );
    let replayed: Vec<u32> = loads_in(&done)
        .into_iter()
        .filter(|n| while_down.contains(n))
        .collect();
    println!(
        "metrics: {} load samples sent while down, {} of them replayed",
        while_down.len(),
        replayed.len()
    );

    handle.end("the user closed the panel");
    if let Some(SessionEvent::Ended { unacknowledged, .. }) = events.recv().await {
        println!(
            "user: session ended; {} messages the panel never acknowledged were handed back",
            unacknowledged.len()
        );
    }
    Ok(())
}
