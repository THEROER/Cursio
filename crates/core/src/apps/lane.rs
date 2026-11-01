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

use cursio_protocol::apps::AppsMsg;
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
}

impl AppsModule {
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

    fn on_message(&mut self, msg: AppsMsg) {
        let Some(update) = msg.update() else {
            tracing::debug!(app = %msg.app, "an application's build message of a kind this build does not know");
            return;
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
        for msg in std::mem::take(&mut self.outbox) {
            let message = Message::module(ModuleId::APPS, msg.priority(), &msg)?;
            tx.send(&message).await?;
        }
        Ok(())
    }

    fn has_pending_work(&self) -> bool {
        !self.outbox.is_empty()
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
        Ok(Box::new(AppsModule {
            builds,
            peer: ctx.peer_name().to_owned(),
            peer_id: ctx.peer.peer_id,
            lanes: BTreeMap::new(),
            outbox: Vec::new(),
        }))
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
        AppsModule {
            builds,
            peer: peer.into(),
            peer_id: PeerId::nil(),
            lanes: BTreeMap::new(),
            outbox: Vec::new(),
        }
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
}
