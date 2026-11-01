//! Messages between copies of one application on different machines, and
//! which machines run it — the node's side of `Cursio::send`, `on_message`
//! and `peers` in `cursio-sdk` (`docs/app-sdk-design.md` §3).
//!
//! A copy hands a message to its own node over the apps socket; the node
//! carries it in the `apps` module's envelope to the named machine, whose
//! node hands it to every running copy listening to its topic and answers
//! what became of it. Nothing is stored: a message to a machine that is not
//! connected, or to one where no copy listens, fails at once and says which
//! — the sender decides whether to try again, because only the application
//! knows whether a late message still means anything.
//!
//! **Whose messages arrive.** A machine hands its copy a message only from a
//! machine its person lets drive that application — the same answer as for
//! calling its actions (`[apps.<id>] actions`, [`super::actions::allowed`]).
//! A message is input to a running program exactly as a call is; a second
//! consent for the same trust would be a second question with one answer.
//!
//! **Which machines run it** is pushed, not asked: every link's `apps`
//! module sends what is registered on its machine whenever that changes
//! ([`AppsWire::Present`]), so `peers()` answers from what is already here.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use cursio_app_protocol::{AppId, AppPeer, ProcessName, SendError, SendFailure, TopicName};
use cursio_protocol::apps::{AppPresence, AppsMsg, AppsWire, Delivery};
use tokio::sync::{mpsc, oneshot};

/// How long a sender waits for the receiving machine to say what became of
/// its message.
pub const SEND_WAIT: Duration = Duration::from_secs(30);

/// One link's way to send, and which link it is (a newer one for the same
/// machine replaces it).
struct LinkTx {
    token: u64,
    tx: mpsc::UnboundedSender<AppsMsg>,
}

#[derive(Default)]
struct Mesh {
    links: BTreeMap<String, LinkTx>,
    /// What each connected machine said it runs.
    presence: BTreeMap<String, Vec<AppPresence>>,
    /// Messages sent and not yet answered, by id.
    pending: HashMap<u64, oneshot::Sender<Delivery>>,
    /// Questions about blobs sent and not yet answered, by id.
    asked: HashMap<u64, oneshot::Sender<AppsWire>>,
}

fn mesh() -> MutexGuard<'static, Mesh> {
    static MESH: OnceLock<Mutex<Mesh>> = OnceLock::new();
    MESH.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// The `apps` module of a link to `peer` is up: what it is given here goes
/// out on that link. The token says which link, for [`unlink`].
pub fn link(peer: &str, tx: mpsc::UnboundedSender<AppsMsg>) -> u64 {
    let token: u64 = rand::random();
    mesh().links.insert(peer.to_owned(), LinkTx { token, tx });
    token
}

/// The link `token` to `peer` is gone; what it said runs there is forgotten.
pub fn unlink(peer: &str, token: u64) {
    let mut mesh = mesh();
    if mesh.links.get(peer).is_some_and(|l| l.token == token) {
        mesh.links.remove(peer);
        mesh.presence.remove(peer);
    }
}

/// What `peer` says is registered on it now.
pub fn set_presence(peer: &str, apps: Vec<AppPresence>) {
    mesh().presence.insert(peer.to_owned(), apps);
}

/// The connected machines where `app` is registered, by what each said.
pub fn peers(app: &AppId) -> Vec<AppPeer> {
    let mesh = mesh();
    mesh.presence
        .iter()
        .filter(|(peer, _)| mesh.links.contains_key(*peer))
        .filter_map(|(peer, apps)| {
            let found = apps.iter().find(|p| p.app == app.as_str())?;
            Some(AppPeer {
                machine: peer.clone(),
                running: found.running,
                version: found.version.clone(),
                topics: found
                    .topics
                    .iter()
                    .filter_map(|t| TopicName::parse(t).ok())
                    .collect(),
            })
        })
        .collect()
}

fn failure(kind: SendFailure, reason: String) -> SendError {
    SendError { kind, reason }
}

/// Carry `payload` about `topic` from this machine's copy of `app` (its
/// `from_process`) to the copies on `to`, and wait for what became of it.
pub async fn send(
    app: &AppId,
    from_process: Option<&ProcessName>,
    to: &str,
    topic: &TopicName,
    payload: Vec<u8>,
) -> Result<u32, SendError> {
    let id: u64 = rand::random();
    let (answer_tx, answer) = oneshot::channel();
    {
        let mut mesh = mesh();
        let Some(link) = mesh.links.get(to) else {
            return Err(failure(
                SendFailure::Offline,
                format!("{to} is not connected to this machine"),
            ));
        };
        let msg = AppsMsg::wire(
            app.as_str(),
            &AppsWire::Datagram {
                id,
                from_process: from_process.map(|p| p.to_string()),
                topic: topic.to_string(),
                payload,
            },
        );
        if link.tx.send(msg).is_err() {
            return Err(failure(
                SendFailure::Offline,
                format!("the link to {to} closed as the message was sent"),
            ));
        }
        mesh.pending.insert(id, answer_tx);
    }
    let outcome = tokio::time::timeout(SEND_WAIT, answer).await;
    mesh().pending.remove(&id);
    match outcome {
        Ok(Ok(Delivery::Delivered { copies })) => Ok(copies),
        Ok(Ok(Delivery::NotRunning)) => Err(failure(
            SendFailure::NotRunning,
            format!("no copy of {app} on {to} listens to {topic} now"),
        )),
        Ok(Ok(Delivery::Refused(why))) => Err(failure(SendFailure::Refused, why)),
        Ok(Err(_)) => Err(failure(
            SendFailure::Offline,
            format!("the link to {to} closed before it answered"),
        )),
        Err(_) => Err(failure(
            SendFailure::Failed,
            format!("{to} did not answer within {} s", SEND_WAIT.as_secs()),
        )),
    }
}

/// The id a question about blobs carries, which its answer carries back.
fn question_id(wire: &AppsWire) -> Option<u64> {
    match wire {
        AppsWire::BlobHave { id, .. } | AppsWire::BlobWant { id, .. } => Some(*id),
        _ => None,
    }
}

/// Send `wire` about `app` to `to` as is: an answer to its question.
pub fn reply(to: &str, app: &str, wire: &AppsWire) {
    if let Some(link) = mesh().links.get(to) {
        let _ = link.tx.send(AppsMsg::wire(app, wire));
    }
}

/// Ask `to` a question about `app`'s blobs and wait up to `wait` for its
/// answer.
pub async fn ask(
    to: &str,
    app: &AppId,
    wire: AppsWire,
    wait: Duration,
) -> Result<AppsWire, String> {
    let id = question_id(&wire).ok_or("not a question about blobs")?;
    let (answer_tx, answer) = oneshot::channel();
    {
        let mut mesh = mesh();
        let Some(link) = mesh.links.get(to) else {
            return Err(format!("{to} is not connected to this machine"));
        };
        if link.tx.send(AppsMsg::wire(app.as_str(), &wire)).is_err() {
            return Err(format!("the link to {to} closed as it was asked"));
        }
        mesh.asked.insert(id, answer_tx);
    }
    let outcome = tokio::time::timeout(wait, answer).await;
    mesh().asked.remove(&id);
    match outcome {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(_)) => Err(format!("the link to {to} closed before it answered")),
        Err(_) => Err(format!("{to} did not answer within {} s", wait.as_secs())),
    }
}

/// An answer `id` to a question about blobs arrived.
pub fn answered(id: u64, wire: AppsWire) {
    if let Some(answer) = mesh().asked.remove(&id) {
        let _ = answer.send(wire);
    }
}

/// The mesh, as the fetching side of [`super::blobs`] reaches it.
pub struct MeshPeers;

#[async_trait::async_trait]
impl super::blobs::Peers for MeshPeers {
    async fn ask(&self, peer: &str, app: &AppId, wire: AppsWire) -> Result<AppsWire, String> {
        ask(peer, app, wire, super::blobs::ASK_WAIT).await
    }
}

/// `peer` answered what became of message `id`.
pub fn delivered(id: u64, outcome: Delivery) {
    if let Some(answer) = mesh().pending.remove(&id) {
        let _ = answer.send(outcome);
    }
}

/// A message from `peer`'s copy of `app` reached this machine: hand it to
/// the copies here listening to its topic, if this machine's person lets
/// `peer` reach them.
pub fn arrived(
    peer: &str,
    app: &str,
    from_process: Option<String>,
    topic: &str,
    payload: Vec<u8>,
) -> Delivery {
    let (Ok(id), Ok(topic)) = (AppId::parse(app), TopicName::parse(topic)) else {
        return Delivery::Refused(format!("{app}/{topic} is not an application and topic"));
    };
    let from_process = from_process.and_then(|p| ProcessName::parse(&p).ok());
    let listening = super::registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .listening(&id, &topic);
    // Checked before consent: asking this machine's person about an
    // application nothing here listens for would be a question about nothing.
    if listening.is_empty() {
        return Delivery::NotRunning;
    }
    if let Err(why) = super::actions::allowed(peer, &id) {
        return Delivery::Refused(why);
    }
    let copies = super::serve::deliver(&listening, peer, from_process, &topic, &payload);
    match copies {
        0 => Delivery::NotRunning,
        n => Delivery::Delivered {
            copies: u32::try_from(n).unwrap_or(u32::MAX),
        },
    }
}
