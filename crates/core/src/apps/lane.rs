//! The `apps` module: applications' builds crossing a link.
//!
//! One update [`Lane`] per application, each driving that application's own
//! [`Agent`](crate::update::agent::Agent) — the same offer, want, chunk and
//! done Cursio's builds use, the same resumable fetch, the same verification
//! — with every message wrapped in [`AppsMsg`] naming its application. The
//! update module keeps the frozen lane to itself; see `protocol/src/apps.rs`.
//!
//! Applications published while the link is up are offered at the next
//! [`REOFFER`](crate::update::lane::REOFFER), as Cursio's own are.

use std::collections::BTreeMap;

use cursio_protocol::apps::{AppsContent, AppsMsg, AppsWire};
use cursio_protocol::update::UpdateMsg;
use cursio_protocol::{Message, ModuleAdvert, ModuleId, PeerId};
use cursio_transport::{TransportError, TransportTx};

use super::builds::{builds, Builds};
use crate::update::lane::Lane;

/// Version of the apps module's messages.
pub const VERSION: u16 = 1;

pub struct AppsModule {
    builds: &'static Builds,
    peer: String,
    peer_id: PeerId,
    lanes: BTreeMap<String, Lane>,
    outbox: Vec<AppsMsg>,
    /// Messages between copies that this machine's applications send to the
    /// peer ([`super::mesh::send`]).
    inbox: tokio::sync::mpsc::UnboundedReceiver<AppsMsg>,
    /// This link in [`super::mesh`], to leave it when the link goes.
    token: u64,
    /// The registry's generation last told to the peer; `None` until the
    /// first presence goes out.
    told: Option<u64>,
}

impl Drop for AppsModule {
    fn drop(&mut self) {
        super::mesh::unlink(&self.peer, self.token);
    }
}

impl AppsModule {
    fn new(builds: &'static Builds, peer: &str, peer_id: PeerId) -> Self {
        let (tx, inbox) = tokio::sync::mpsc::unbounded_channel();
        let token = super::mesh::link(peer, tx);
        AppsModule {
            builds,
            peer: peer.to_owned(),
            peer_id,
            lanes: BTreeMap::new(),
            outbox: Vec::new(),
            inbox,
            token,
            told: None,
        }
    }

    fn lane(&mut self, app: &str) -> Option<&mut Lane> {
        if !self.lanes.contains_key(app) {
            let agent = self.builds.agent(app)?;
            agent.know_peer(&self.peer, self.peer_id);
            self.lanes
                .insert(app.to_owned(), Lane::new(agent, self.peer.clone()));
        }
        self.lanes.get_mut(app)
    }

    /// A lane for every application this machine knows, so each one's builds
    /// are offered.
    fn every_lane(&mut self) {
        for agent in self.builds.known() {
            let app = agent.subject().to_owned();
            let _ = self.lane(&app);
        }
    }

    fn queue(&mut self, app: &str, msgs: Vec<UpdateMsg>) {
        self.outbox
            .extend(msgs.iter().map(|msg| AppsMsg::new(app, msg)));
    }

    /// What the peer should hear: messages this machine's copies sent it, and
    /// what runs here when that changed.
    fn gather(&mut self) {
        while let Ok(msg) = self.inbox.try_recv() {
            self.outbox.push(msg);
        }
        let (generation, apps) = {
            let registry = super::registry().lock().unwrap_or_else(|p| p.into_inner());
            (registry.generation(), registry.presence())
        };
        if self.told != Some(generation) {
            self.told = Some(generation);
            self.outbox
                .push(AppsMsg::wire("", &AppsWire::Present { apps }));
        }
    }

    fn on_wire(&mut self, app: &str, wire: AppsWire) {
        match wire {
            AppsWire::Datagram {
                id,
                from_process,
                topic,
                payload,
            } => {
                let outcome = super::mesh::arrived(&self.peer, app, from_process, &topic, payload);
                self.outbox
                    .push(AppsMsg::wire(app, &AppsWire::Delivered { id, outcome }));
            }
            AppsWire::Delivered { id, outcome } => super::mesh::delivered(id, outcome),
            AppsWire::Present { apps } => super::mesh::set_presence(&self.peer, apps),
        }
    }

    fn on_message(&mut self, msg: AppsMsg) {
        let update = match msg.content() {
            Some(AppsContent::Update(update)) => update,
            Some(AppsContent::Wire(wire)) => return self.on_wire(&msg.app.clone(), wire),
            None => {
                tracing::debug!(app = %msg.app, "an application message of a kind this build does not know");
                return;
            }
        };
        let Some(lane) = self.lane(&msg.app) else {
            tracing::debug!(app = %msg.app, peer = %self.peer, "a build message for something that is not an application id");
            return;
        };
        let replies = lane.handle(update);
        self.queue(&msg.app.clone(), replies);
    }
}

#[async_trait::async_trait]
impl crate::module::SessionModule for AppsModule {
    fn name(&self) -> &'static str {
        "apps"
    }

    fn id(&self) -> ModuleId {
        ModuleId::APPS
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.every_lane();
        let offers: Vec<(String, Vec<UpdateMsg>)> = self
            .lanes
            .iter_mut()
            .map(|(app, lane)| (app.clone(), lane.offers()))
            .collect();
        for (app, msgs) in offers {
            self.queue(&app, msgs);
        }
        self.pump(tx).await
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.every_lane();
        let polled: Vec<(String, Vec<UpdateMsg>)> = self
            .lanes
            .iter_mut()
            .map(|(app, lane)| (app.clone(), lane.poll()))
            .collect();
        for (app, msgs) in polled {
            self.queue(&app, msgs);
        }
        self.gather();
        for msg in std::mem::take(&mut self.outbox) {
            let message = Message::module(ModuleId::APPS, msg.priority(), &msg)?;
            tx.send(&message).await?;
        }
        Ok(())
    }

    fn has_pending_work(&self) -> bool {
        !self.outbox.is_empty() || !self.inbox.is_empty()
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<AppsMsg>(ModuleId::APPS) {
            Ok(Some(msg)) => {
                self.on_message(msg);
                true
            }
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%error, peer = %self.peer, "undecodable apps message from the peer");
                true
            }
        }
    }
}

/// Builds the apps module on every link, once this node takes applications'
/// builds.
pub struct AppsFactory;

impl crate::module::ModuleFactory for AppsFactory {
    fn id(&self) -> ModuleId {
        ModuleId::APPS
    }

    fn advert(&self) -> Option<ModuleAdvert> {
        builds().map(|_| ModuleAdvert::symmetric(ModuleId::APPS, VERSION))
    }

    fn build(
        &self,
        ctx: &crate::module::SessionCtx,
    ) -> anyhow::Result<Box<dyn crate::module::SessionModule>> {
        let builds =
            builds().ok_or_else(|| anyhow::anyhow!("this node takes no applications' builds"))?;
        Ok(Box::new(AppsModule::new(
            builds,
            ctx.peer_name(),
            ctx.peer.peer_id,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, TrustedBuilder};
    use crate::update::manifest::Manifest;
    use crate::update::sign::{hex, BuilderKey};
    use crate::update::store::Store;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cursio-apps-lane-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn module(builds: &'static Builds, peer: &str) -> AppsModule {
        AppsModule::new(builds, peer, PeerId::nil())
    }

    /// Carry every queued message across until neither side has more.
    fn settle(a: &mut AppsModule, b: &mut AppsModule) {
        for _ in 0..10_000 {
            let (to_b, to_a) = (std::mem::take(&mut a.outbox), std::mem::take(&mut b.outbox));
            if to_b.is_empty() && to_a.is_empty() {
                return;
            }
            to_b.into_iter().for_each(|m| b.on_message(m));
            to_a.into_iter().for_each(|m| a.on_message(m));
        }
        panic!("the two sides never went quiet");
    }

    fn offer_everything(module: &mut AppsModule) {
        module.every_lane();
        let offers: Vec<(String, Vec<UpdateMsg>)> = module
            .lanes
            .iter_mut()
            .map(|(app, lane)| (app.clone(), lane.offers()))
            .collect();
        for (app, msgs) in offers {
            module.queue(&app, msgs);
        }
    }

    #[test]
    fn a_trusting_machine_takes_an_application_build_and_makes_it_current() {
        let root = scratch("carry");
        let key = BuilderKey::from_seed([9; 32]);
        let platform = cursio_protocol::update::this_platform();

        let a_root = root.join("a");
        let app = cursio_app_protocol::AppId::parse("demo").unwrap();
        let staged = root.join("staged");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("demo.exe"), vec![7u8; 300_000]).unwrap();
        Manifest::describe(&staged, "1.0.0", &["demo.exe"])
            .unwrap()
            .write(&staged)
            .unwrap();
        Store::of(&super::super::builds::layout_of(&a_root, &app))
            .publish(
                &key,
                cursio_protocol::update::Channel::Dev,
                &[(platform, staged)],
                5,
            )
            .unwrap();

        let b_root = root.join("b");
        let b_config = root.join("b.toml");
        let mut config = Config::default();
        config
            .apps
            .entry("demo".into())
            .or_default()
            .trusted_builders
            .push(TrustedBuilder {
                peer: PeerId::nil(),
                name: "A".into(),
                key: hex(&key.public()),
            });
        config.save(&b_config).unwrap();

        let a: &'static Builds = Box::leak(Box::new(Builds::at(a_root, root.join("a.toml"))));
        let b: &'static Builds = Box::leak(Box::new(Builds::at(b_root.clone(), b_config)));
        let (mut at_a, mut at_b) = (module(a, "B"), module(b, "A"));
        offer_everything(&mut at_a);
        settle(&mut at_a, &mut at_b);

        let installed = super::super::builds::layout_of(&b_root, &app);
        assert_eq!(installed.installed(), ["1.0.0"]);
        assert_eq!(installed.current().as_deref(), Some("1.0.0"));
        assert_eq!(
            std::fs::read(installed.version_dir("1.0.0").join("demo.exe")).unwrap(),
            vec![7u8; 300_000]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_phone_hands_an_application_build_to_its_installer() {
        let root = scratch("phone");
        let key = BuilderKey::from_seed([11; 32]);
        let app = cursio_app_protocol::AppId::parse("demo3").unwrap();
        let staged = root.join("staged");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("demo3.apk"), b"apk").unwrap();
        Manifest::describe(&staged, "2.0.0", &["demo3.apk"])
            .unwrap()
            .write(&staged)
            .unwrap();
        Store::of(&super::super::builds::layout_of(&root.join("a"), &app))
            .publish(
                &key,
                cursio_protocol::update::Channel::Dev,
                &[(cursio_protocol::update::this_platform(), staged)],
                5,
            )
            .unwrap();
        let mut config = Config::default();
        config
            .apps
            .entry("demo3".into())
            .or_default()
            .trusted_builders
            .push(TrustedBuilder {
                peer: PeerId::nil(),
                name: "A".into(),
                key: hex(&key.public()),
            });
        config.save(&root.join("phone.toml")).unwrap();

        let offered = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = offered.clone();
        let phone = Builds::at(root.join("phone"), root.join("phone.toml")).with_switcher(
            std::sync::Arc::new(move |app, _layout, version| {
                seen.lock()
                    .unwrap()
                    .push((app.to_string(), version.to_owned()));
                Ok(())
            }),
        );
        assert!(phone.hands_to_installer());
        let a: &'static Builds =
            Box::leak(Box::new(Builds::at(root.join("a"), root.join("a.toml"))));
        let phone: &'static Builds = Box::leak(Box::new(phone));
        let (mut at_a, mut at_phone) = (module(a, "phone"), module(phone, "A"));
        offer_everything(&mut at_a);
        settle(&mut at_a, &mut at_phone);

        assert_eq!(
            offered.lock().unwrap().as_slice(),
            [("demo3".to_owned(), "2.0.0".to_owned())]
        );
        let layout = super::super::builds::layout_of(&root.join("phone"), &app);
        assert_eq!(layout.current().as_deref(), Some("2.0.0"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_machine_that_trusts_nobody_for_an_application_takes_none_of_it() {
        let root = scratch("untrusted");
        let key = BuilderKey::from_seed([10; 32]);
        let app = cursio_app_protocol::AppId::parse("demo2").unwrap();
        let staged = root.join("staged");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("demo2"), b"bin").unwrap();
        Manifest::describe(&staged, "1.0.0", &["demo2"])
            .unwrap()
            .write(&staged)
            .unwrap();
        Store::of(&super::super::builds::layout_of(&root.join("a"), &app))
            .publish(
                &key,
                cursio_protocol::update::Channel::Dev,
                &[(cursio_protocol::update::this_platform(), staged)],
                5,
            )
            .unwrap();
        let a: &'static Builds =
            Box::leak(Box::new(Builds::at(root.join("a"), root.join("a.toml"))));
        let b: &'static Builds =
            Box::leak(Box::new(Builds::at(root.join("b"), root.join("b.toml"))));
        let (mut at_a, mut at_b) = (module(a, "B"), module(b, "A"));
        offer_everything(&mut at_a);
        settle(&mut at_a, &mut at_b);
        let layout = super::super::builds::layout_of(&root.join("b"), &app);
        assert!(layout.installed().is_empty());

        // The person presses the refusal's button: the same link, offered
        // nothing new, takes the build.
        let mut config = Config::default();
        config
            .apps
            .entry("demo2".into())
            .or_default()
            .trusted_builders
            .push(TrustedBuilder {
                peer: PeerId::nil(),
                name: "A".into(),
                key: hex(&key.public()),
            });
        config.save(&root.join("b.toml")).unwrap();
        let retried: Vec<(String, Vec<UpdateMsg>)> = at_b
            .lanes
            .iter_mut()
            .map(|(app, lane)| (app.clone(), lane.poll()))
            .collect();
        for (app, msgs) in retried {
            at_b.queue(&app, msgs);
        }
        settle(&mut at_a, &mut at_b);
        assert_eq!(layout.installed(), ["1.0.0"]);
        crate::diagnostics::clear("app.demo2.refused");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Run `send` from A to B while carrying both modules' traffic across.
    async fn send_across(
        at_a: &mut AppsModule,
        at_b: &mut AppsModule,
        app: &str,
        topic: &str,
    ) -> Result<u32, cursio_app_protocol::SendError> {
        let (app, topic) = (
            cursio_app_protocol::AppId::parse(app).unwrap(),
            cursio_app_protocol::TopicName::parse(topic).unwrap(),
        );
        let to = at_a.peer.clone();
        let sending = tokio::spawn(async move {
            super::super::mesh::send(&app, None, &to, &topic, b"hello".to_vec()).await
        });
        for _ in 0..200 {
            if sending.is_finished() {
                break;
            }
            at_a.gather();
            at_b.gather();
            settle(at_a, at_b);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        sending.await.unwrap()
    }

    #[tokio::test]
    async fn a_message_to_a_copy_that_is_not_there_or_not_allowed_says_which() {
        let root = scratch("messages");
        let a: &'static Builds =
            Box::leak(Box::new(Builds::at(root.join("a"), root.join("a.toml"))));
        let b: &'static Builds =
            Box::leak(Box::new(Builds::at(root.join("b"), root.join("b.toml"))));
        // Each module is named after the machine at its far end.
        let (mut at_a, mut at_b) = (module(a, "msg-b"), module(b, "msg-a"));

        let unheard = send_across(&mut at_a, &mut at_b, "msg-nobody", "sync").await;
        assert_eq!(
            unheard.unwrap_err().kind,
            cursio_app_protocol::SendFailure::NotRunning
        );

        // A copy listens on B — the registry is the one process both modules
        // share here — but B's person has not allowed A yet.
        let key = super::super::Key::main(cursio_app_protocol::AppId::parse("msg-asks").unwrap());
        {
            let mut registry = super::super::registry().lock().unwrap();
            registry.connected(&key, "1".into(), 1, None);
            registry.set_topics(
                &key,
                vec![cursio_app_protocol::TopicName::parse("sync").unwrap()],
            );
        }
        let refused = send_across(&mut at_a, &mut at_b, "msg-asks", "sync").await;
        let refused = refused.unwrap_err();
        assert_eq!(refused.kind, cursio_app_protocol::SendFailure::Refused);
        assert!(refused.reason.contains("allow"), "{}", refused.reason);

        // What runs on each side reached the other as presence.
        let seen = super::super::mesh::peers(&key.app);
        assert!(
            seen.iter().any(|p| p.machine == "msg-a" && p.running),
            "{seen:?}"
        );
        super::super::registry().lock().unwrap().disconnected(&key);
        crate::diagnostics::clear("app.msg-asks.actions");

        let offline = super::super::mesh::send(
            &cursio_app_protocol::AppId::parse("msg-asks").unwrap(),
            None,
            "msg-nowhere",
            &cursio_app_protocol::TopicName::parse("sync").unwrap(),
            Vec::new(),
        )
        .await;
        assert_eq!(
            offline.unwrap_err().kind,
            cursio_app_protocol::SendFailure::Offline
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
