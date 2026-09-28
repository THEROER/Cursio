//! One link's half of the update exchange: offer what is held, fetch what is
//! wanted, answer what is asked.
//!
//! A [`Lane`] is a plain state machine — messages in, messages out — so the
//! same code runs inside a session (as [`UpdateModule`]) and on a link whose
//! peer speaks another protocol version ([`run_alone`]), where there is no
//! session at all.
//!
//! **Pulled, a window at a time.** The receiver asks for [`WINDOW`] bytes of a
//! file and the sender answers with that range in [`CHUNK`]-sized pieces; the
//! next request goes out when the window is in. So a sender never pushes more
//! than one window ahead of a receiver that is slow to write, and a receiver
//! that goes away leaves at most one window in flight.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cursio_protocol::update::{Artifact, Chunk, Done, Offer, Statement, UpdateMsg, Want};
use cursio_protocol::{Message, ModuleAdvert, ModuleId};
use cursio_transport::{TransportError, TransportTx};

use super::agent::{Agent, Decision};

/// One piece of a file on the wire.
pub const CHUNK: usize = 256 * 1024;
/// How much is asked for at once.
pub const WINDOW: u32 = 4 * CHUNK as u32;
/// The most a sender answers for one request, whatever it asked.
const MAX_WANT: u32 = 4 * WINDOW;
/// How often a lane looks for builds it has not offered yet.
const REOFFER: Duration = Duration::from_secs(30);

pub struct Lane {
    agent: Arc<Agent>,
    peer: String,
    fetch: Option<Fetch>,
    offered: Vec<(String, Vec<String>)>,
    last_look: Option<Instant>,
}

struct Fetch {
    statement: Statement,
    files: Vec<Artifact>,
    file: usize,
    /// Next byte expected in the current file.
    offset: u64,
    /// End of the window asked for in the current file.
    asked_to: u64,
    received: u64,
}

impl Lane {
    pub fn new(agent: Arc<Agent>, peer: impl Into<String>) -> Self {
        Self {
            agent,
            peer: peer.into(),
            fetch: None,
            offered: Vec::new(),
            last_look: None,
        }
    }

    /// Whether a fetch is under way on this link.
    pub fn busy(&self) -> bool {
        self.fetch.is_some()
    }

    /// Offers for held builds this peer has not been offered yet.
    pub fn offers(&mut self) -> Vec<UpdateMsg> {
        self.last_look = Some(Instant::now());
        let mut out = Vec::new();
        for held in self.agent.store().held() {
            let key = (held.statement.version.clone(), held.offer.have.clone());
            if !self.offered.contains(&key) {
                self.offered.push(key);
                out.push(UpdateMsg::Offer(held.offer));
            }
        }
        out
    }

    /// Offers again once in a while, for builds published since.
    pub fn poll(&mut self) -> Vec<UpdateMsg> {
        match self.last_look {
            Some(at) if at.elapsed() < REOFFER => Vec::new(),
            _ => self.offers(),
        }
    }

    pub fn handle(&mut self, msg: UpdateMsg) -> Vec<UpdateMsg> {
        match msg {
            UpdateMsg::Offer(offer) => self.on_offer(offer),
            UpdateMsg::Want(want) => self.on_want(want),
            UpdateMsg::Chunk(chunk) => self.on_chunk(chunk),
            UpdateMsg::Done(done) => {
                tracing::info!(
                    peer = %self.peer,
                    version = %done.version,
                    installed = done.installed,
                    reason = %done.reason,
                    "peer finished with a build this machine offered"
                );
                Vec::new()
            }
            // Only ever the first frame of a connection; the handshake reads it.
            UpdateMsg::Preamble(_) => Vec::new(),
        }
    }

    fn on_offer(&mut self, offer: Offer) -> Vec<UpdateMsg> {
        if self.fetch.is_some() {
            return Vec::new();
        }
        let Decision::Fetch(statement) = self.agent.consider(&offer, &self.peer) else {
            return Vec::new();
        };
        if let Err(e) = self.agent.store().remember(&offer) {
            return self.fail(&statement.version, &e.to_string());
        }
        let files: Vec<Artifact> = statement
            .for_platform(self.agent.platform())
            .cloned()
            .collect();
        tracing::info!(
            peer = %self.peer,
            version = %statement.version,
            files = files.len(),
            "fetching a build over the mesh"
        );
        self.fetch = Some(Fetch {
            statement,
            files,
            file: 0,
            offset: 0,
            asked_to: 0,
            received: 0,
        });
        self.next_step()
    }

    fn on_want(&mut self, want: Want) -> Vec<UpdateMsg> {
        let mut out = Vec::new();
        let end = want.offset + u64::from(want.length.min(MAX_WANT));
        let mut offset = want.offset;
        while offset < end {
            let length = (end - offset).min(CHUNK as u64) as usize;
            let data = match self.agent.store().read(
                &want.version,
                &want.platform,
                &want.name,
                offset,
                length,
            ) {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(peer = %self.peer, error = %e, "cannot serve a build file");
                    break;
                }
            };
            if data.is_empty() {
                break;
            }
            let read = data.len() as u64;
            out.push(UpdateMsg::Chunk(Chunk {
                version: want.version.clone(),
                platform: want.platform.clone(),
                name: want.name.clone(),
                offset,
                data,
            }));
            offset += read;
            if read < length as u64 {
                break;
            }
        }
        out
    }

    fn on_chunk(&mut self, chunk: Chunk) -> Vec<UpdateMsg> {
        let Some(fetch) = self.fetch.as_mut() else {
            return Vec::new();
        };
        let Some(file) = fetch.files.get(fetch.file) else {
            return Vec::new();
        };
        if chunk.version != fetch.statement.version
            || chunk.platform != file.platform
            || chunk.name != file.name
            || chunk.offset != fetch.offset
        {
            return Vec::new();
        }
        let version = fetch.statement.version.clone();
        let len = chunk.data.len() as u64;
        if fetch.offset + len > file.size {
            return self.fail(&version, "a peer sent more bytes than the file has");
        }
        if let Err(e) = self.agent.store().write(
            &chunk.version,
            &chunk.platform,
            &chunk.name,
            chunk.offset,
            &chunk.data,
        ) {
            return self.fail(&version, &e.to_string());
        }
        fetch.offset += len;
        fetch.received += len;
        self.agent.progressed(fetch.received);
        self.next_step()
    }

    /// Ask for the next window, finish a file, or finish the build.
    fn next_step(&mut self) -> Vec<UpdateMsg> {
        loop {
            let Some(fetch) = self.fetch.as_mut() else {
                return Vec::new();
            };
            let version = fetch.statement.version.clone();
            let Some(file) = fetch.files.get(fetch.file).cloned() else {
                return self.complete();
            };
            if fetch.offset >= file.size {
                if let Err(e) = self.agent.store().finish_file(&version, &file) {
                    return self.fail(&version, &e.to_string());
                }
                fetch.file += 1;
                fetch.offset = 0;
                fetch.asked_to = 0;
                continue;
            }
            if fetch.offset < fetch.asked_to {
                return Vec::new();
            }
            let length = (file.size - fetch.offset).min(u64::from(WINDOW)) as u32;
            fetch.asked_to = fetch.offset + u64::from(length);
            return vec![UpdateMsg::Want(Want {
                version,
                platform: file.platform,
                name: file.name,
                offset: fetch.offset,
                length,
            })];
        }
    }

    fn complete(&mut self) -> Vec<UpdateMsg> {
        let Some(fetch) = self.fetch.take() else {
            return Vec::new();
        };
        let statement = fetch.statement;
        let platform = self.agent.platform().to_owned();
        let installed = self
            .agent
            .store()
            .finish_platform(&statement.version, &platform)
            .and_then(|dir| self.agent.layout().install(&dir));
        match installed {
            Ok(_) => {
                self.agent.installed(&statement, &self.peer);
                vec![UpdateMsg::Done(Done {
                    version: statement.version,
                    platform,
                    installed: true,
                    reason: String::new(),
                })]
            }
            Err(e) => self.fail(&statement.version, &e.to_string()),
        }
    }

    fn fail(&mut self, version: &str, reason: &str) -> Vec<UpdateMsg> {
        self.fetch = None;
        self.agent.failed(version, &self.peer, reason);
        vec![UpdateMsg::Done(Done {
            version: version.to_owned(),
            platform: self.agent.platform().to_owned(),
            installed: false,
            reason: reason.to_owned(),
        })]
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        if let Some(fetch) = &self.fetch {
            self.agent.abandoned(&fetch.statement.version);
        }
    }
}

/// Wrap lane messages for the channel.
pub fn to_messages(msgs: Vec<UpdateMsg>) -> impl Iterator<Item = Message> {
    msgs.into_iter().map(|m| Message::Update(m.encode()))
}

/// Unwrap a lane message; `None` for anything else, including a lane kind
/// this build does not know.
pub fn from_message(msg: &Message) -> Option<UpdateMsg> {
    match msg {
        Message::Update(bytes) => UpdateMsg::decode(bytes).ok().flatten(),
        _ => None,
    }
}

/// The lane inside a session, on its module thread like any other module.
pub struct UpdateModule {
    lane: Lane,
    outbox: Vec<UpdateMsg>,
}

#[async_trait::async_trait]
impl crate::module::SessionModule for UpdateModule {
    fn name(&self) -> &'static str {
        "update"
    }

    fn id(&self) -> ModuleId {
        ModuleId::UPDATE
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let offers = self.lane.offers();
        send(tx, offers).await
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let mut out = std::mem::take(&mut self.outbox);
        out.extend(self.lane.poll());
        send(tx, out).await
    }

    fn has_pending_work(&self) -> bool {
        !self.outbox.is_empty()
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match from_message(msg) {
            Some(update) => {
                let out = self.lane.handle(update);
                self.outbox.extend(out);
                true
            }
            None => matches!(msg, Message::Update(_)),
        }
    }
}

async fn send(tx: &mut dyn TransportTx, msgs: Vec<UpdateMsg>) -> Result<(), TransportError> {
    for msg in to_messages(msgs) {
        tx.send(&msg).await?;
    }
    Ok(())
}

/// Builds the update module on every link, when this node runs updates.
pub struct UpdateFactory;

/// Version of the update module's advert. The lane's bytes are frozen, so
/// this stays 1; it exists because every module advertises one.
pub const UPDATE_MODULE_VERSION: u16 = 1;

impl crate::module::ModuleFactory for UpdateFactory {
    fn id(&self) -> ModuleId {
        ModuleId::UPDATE
    }

    fn advert(&self) -> Option<ModuleAdvert> {
        super::agent::global()
            .map(|_| ModuleAdvert::symmetric(ModuleId::UPDATE, UPDATE_MODULE_VERSION))
    }

    fn build(
        &self,
        ctx: &crate::module::SessionCtx,
    ) -> anyhow::Result<Box<dyn crate::module::SessionModule>> {
        let agent = super::agent::global()
            .ok_or_else(|| anyhow::anyhow!("updates are not running on this node"))?;
        Ok(Box::new(UpdateModule {
            lane: Lane::new(agent, ctx.peer_name()),
            outbox: Vec::new(),
        }))
    }
}

/// How long an update-only link stays up with nothing to do.
const ALONE_IDLE: Duration = Duration::from_secs(20);
/// The longest an update-only link is kept, whatever it is doing.
const ALONE_LIMIT: Duration = Duration::from_secs(30 * 60);

/// Carry only the update lane on a link whose peer speaks another protocol
/// version, until neither side has anything left to say.
///
/// Runs on a thread of its own: writing and hashing a build is blocking disk
/// work, and the caller's thread is the one every other link's cursor rides.
pub async fn run_alone<T>(transport: T, agent: Arc<Agent>, peer: String) -> anyhow::Result<()>
where
    T: cursio_transport::Transport + 'static,
{
    run_alone_for(transport, agent, peer, ALONE_IDLE).await
}

/// [`run_alone`], going quiet after `idle` with nothing to do.
#[doc(hidden)]
pub async fn run_alone_for<T>(
    transport: T,
    agent: Arc<Agent>,
    peer: String,
    idle: Duration,
) -> anyhow::Result<()>
where
    T: cursio_transport::Transport + 'static,
{
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("update-lane".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|rt| rt.block_on(alone(transport, agent, peer, idle)));
            let _ = done_tx.send(result);
        })?;
    done_rx
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the update lane's thread ended early")))
}

async fn alone<T>(
    mut transport: T,
    agent: Arc<Agent>,
    peer: String,
    idle: Duration,
) -> anyhow::Result<()>
where
    T: cursio_transport::Transport,
{
    let mut lane = Lane::new(agent, peer);
    for msg in to_messages(lane.offers()) {
        transport.send(&msg).await?;
    }
    let started = Instant::now();
    while started.elapsed() < ALONE_LIMIT {
        let msg = match tokio::time::timeout(idle, transport.recv()).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(TransportError::Closed)) => break,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) if lane.busy() => anyhow::bail!("the peer stopped sending a build midway"),
            Err(_) => break,
        };
        if let Some(update) = from_message(&msg) {
            for out in to_messages(lane.handle(update)) {
                transport.send(&out).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TrustedBuilder, UpdateChannel, UpdateConfig, UpdatePolicy};
    use crate::update::layout::InstallLayout;
    use crate::update::manifest::Manifest;
    use crate::update::sign::{hex, BuilderKey};
    use cursio_protocol::update::Channel;
    use std::path::{Path, PathBuf};

    const PLATFORM: &str = "linux-x86_64";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cursio-lane-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn agent(root: &Path, trusts: Option<&BuilderKey>) -> Arc<Agent> {
        let trusted: Vec<TrustedBuilder> = trusts
            .map(|k| TrustedBuilder {
                peer: cursio_protocol::PeerId::nil(),
                name: "builder".into(),
                key: hex(&k.public()),
            })
            .into_iter()
            .collect();
        Arc::new(Agent::new(
            InstallLayout::at(root),
            PLATFORM.into(),
            move || UpdateConfig {
                policy: UpdatePolicy::Ask,
                channel: UpdateChannel::Dev,
                trusted_builders: trusted.clone(),
            },
        ))
    }

    /// A publisher holding one build whose executable is `size` bytes.
    fn publisher(root: &Path, key: &BuilderKey, size: usize) -> Arc<Agent> {
        let stage = root.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let exe: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(stage.join("cursio"), exe).unwrap();
        Manifest::describe(&stage, "0.9.0+new", &["cursio"])
            .unwrap()
            .write(&stage)
            .unwrap();
        let agent = agent(&root.join("install"), None);
        agent
            .store()
            .publish(key, Channel::Dev, &[(PLATFORM.into(), stage)], 100)
            .unwrap();
        agent
    }

    /// Carry messages both ways until neither side has anything to say;
    /// answers what `a` heard.
    fn converse(a: &mut Lane, b: &mut Lane, first: Vec<UpdateMsg>) -> Vec<UpdateMsg> {
        let mut to_b = first;
        let mut to_a: Vec<UpdateMsg> = Vec::new();
        let mut heard_by_a = Vec::new();
        for _ in 0..10_000 {
            if to_a.is_empty() && to_b.is_empty() {
                return heard_by_a;
            }
            let mut next_a = Vec::new();
            for m in to_b.drain(..) {
                next_a.extend(b.handle(m));
            }
            let mut next_b = Vec::new();
            for m in to_a.drain(..) {
                heard_by_a.push(m.clone());
                next_b.extend(a.handle(m));
            }
            to_a = next_a;
            to_b = next_b;
        }
        panic!("the lanes never went quiet");
    }

    #[test]
    fn a_build_crosses_in_windows_and_is_installed_on_the_receiver() {
        let root = scratch("cross");
        let key = BuilderKey::from_seed([7; 32]);
        let sender = publisher(&root.join("a"), &key, 3 * CHUNK + 17);
        let receiver = agent(&root.join("b"), Some(&key));
        let mut a = Lane::new(sender, "a");
        let mut b = Lane::new(receiver.clone(), "b");

        let offers = a.offers();
        let heard = converse(&mut a, &mut b, offers);

        assert!(heard
            .iter()
            .any(|m| matches!(m, UpdateMsg::Done(d) if d.installed)));
        assert!(receiver
            .layout()
            .installed()
            .contains(&"0.9.0+new".to_owned()));
        let status = receiver.status();
        assert_eq!(status.ready.unwrap().version, "0.9.0+new");
        assert!(status.progress.is_none());
        // The receiver can now serve it on to the next machine.
        assert!(receiver.store().is_complete("0.9.0+new", PLATFORM));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_build_from_an_untrusted_signer_is_never_fetched() {
        let root = scratch("untrusted");
        let key = BuilderKey::from_seed([7; 32]);
        let sender = publisher(&root.join("a"), &key, 10);
        let receiver = agent(&root.join("b"), None);
        let mut a = Lane::new(sender, "a");
        let mut b = Lane::new(receiver.clone(), "b");

        let offers = a.offers();
        let heard = converse(&mut a, &mut b, offers);

        assert!(heard.is_empty(), "nothing was asked for");
        assert!(receiver.layout().installed().is_empty());
        assert!(receiver.status().last.unwrap().contains("refused"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_altered_on_the_way_is_rejected_and_not_installed() {
        let root = scratch("tampered");
        let key = BuilderKey::from_seed([7; 32]);
        let sender = publisher(&root.join("a"), &key, CHUNK + 5);
        let receiver = agent(&root.join("b"), Some(&key));
        let mut a = Lane::new(sender, "a");
        let mut b = Lane::new(receiver.clone(), "b");

        let mut to_b = a.offers();
        let mut failed = false;
        for _ in 0..100 {
            let mut to_a = Vec::new();
            for m in to_b.drain(..) {
                to_a.extend(b.handle(m));
            }
            if to_a
                .iter()
                .any(|m| matches!(m, UpdateMsg::Done(d) if !d.installed))
            {
                failed = true;
                break;
            }
            for m in to_a {
                for mut reply in a.handle(m) {
                    if let UpdateMsg::Chunk(c) = &mut reply {
                        c.data[0] ^= 0xff;
                    }
                    to_b.push(reply);
                }
            }
            if to_b.is_empty() {
                break;
            }
        }

        assert!(failed, "the receiver said the build failed");
        assert!(receiver.layout().installed().is_empty());
        assert!(receiver.status().last.unwrap().contains("failed"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
