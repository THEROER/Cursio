//! `cursio update …`: the subcommands, kept here so the binary only names them.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;

use cursio_protocol::update::Channel;

use super::apply::{self, Plan};
use super::layout::InstallLayout;
use super::manifest::Manifest;
use super::sign::{self, BuilderKey};
use super::store::Store;
use crate::config::{Config, TrustedBuilder, UpdateChannel};

#[derive(Debug, Subcommand)]
pub enum UpdateAction {
    /// Installed versions, and which are current and previous.
    Status,
    /// Copy a staged build (`cargo xtask stage`) into the install.
    Install {
        /// The build directory, with its `manifest.json`.
        dir: PathBuf,
        /// Build it by patching this installed version with the zstd deltas
        /// in `dir` instead of copying whole files.
        #[arg(long, value_name = "VERSION")]
        delta_from: Option<String>,
        /// Also make it current (no node is stopped or started).
        #[arg(long)]
        activate: bool,
    },
    /// Switch to an installed version: wait for the named processes, flip
    /// `current`, start the node, and roll back if it does not answer.
    Apply {
        #[arg(long)]
        version: String,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Go back to the previous version, the same way `apply` goes forward.
    Rollback {
        #[command(flatten)]
        run: RunArgs,
    },
    /// Delete old versions, keeping the newest N plus current and previous.
    Prune {
        #[arg(long, default_value_t = 3)]
        keep: usize,
    },
    /// Sign staged builds with this machine's key and hand them to the mesh
    /// on the `dev` channel. `cargo xtask publish` runs this.
    Publish {
        /// `PLATFORM=DIR`, e.g. `windows-x86_64=target/stage/…`. Repeatable;
        /// every directory must be the same build.
        #[arg(long = "build", value_name = "PLATFORM=DIR", required = true)]
        builds: Vec<String>,
    },
    /// Print this machine's build-signing key, making it on first use.
    Key,
    /// Accept `dev` builds signed by this paired machine. Pins the key it
    /// signs with now; switches this machine to the `dev` channel.
    TrustBuilder {
        /// The machine's name or id, as `cursio peers` shows it.
        machine: String,
        /// Its key, from `cursio update key` there. Without it, the key of
        /// the last build that machine offered here is used.
        #[arg(long)]
        key: Option<String>,
    },
    /// Stop accepting builds from this machine.
    UntrustBuilder { machine: String },
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// A process to wait for before switching (the node, the app). Repeatable.
    #[arg(long = "wait-pid", value_name = "PID")]
    wait_pids: Vec<u32>,
    /// Ask those processes to stop instead of only waiting. Unix sends
    /// SIGINT; Windows terminates them.
    #[arg(long)]
    stop: bool,
    /// Switch without starting the node.
    #[arg(long)]
    no_relaunch: bool,
    /// Seconds the new node has to answer on its control socket.
    #[arg(long, default_value_t = 90)]
    health_timeout: u64,
    #[arg(long, default_value_t = 3)]
    keep: usize,
    /// What the node is started with; `mesh` when empty. After `--`.
    #[arg(last = true)]
    node_args: Vec<OsString>,
}

impl RunArgs {
    fn plan(&self, version: String) -> Plan {
        Plan {
            version,
            wait_pids: self.wait_pids.clone(),
            stop: self.stop,
            node_args: self.node_args.clone(),
            relaunch: !self.no_relaunch,
            health_timeout: Duration::from_secs(self.health_timeout),
            keep: self.keep,
        }
    }
}

pub async fn run(action: UpdateAction, config_path: &std::path::Path) -> anyhow::Result<()> {
    let layout = InstallLayout::for_user()?;
    match action {
        UpdateAction::Publish { builds } => {
            let builds = builds
                .iter()
                .map(|b| {
                    let (platform, dir) = b.split_once('=').ok_or_else(|| {
                        anyhow::anyhow!(
                            "`{b}` is not PLATFORM=DIR, e.g. linux-x86_64=target/stage/v"
                        )
                    })?;
                    Ok((platform.to_owned(), PathBuf::from(dir)))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let key = BuilderKey::load_or_create()?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let store = Store::of(&layout);
            let statement = store.publish(&key, Channel::Dev, &builds, now)?;
            let platforms: Vec<&str> = builds.iter().map(|(p, _)| p.as_str()).collect();
            println!(
                "published {} for {} on the dev channel, signed by {}",
                statement.version,
                platforms.join(", "),
                sign::short_key(&key.public())
            );
            println!(
                "a running node offers it to its peers within {} s",
                super::lane::REOFFER.as_secs()
            );
            for gone in store.prune(3) {
                println!("stopped holding {gone}");
            }
            Ok(())
        }
        UpdateAction::Key => {
            let key = BuilderKey::load_or_create()?;
            println!("{}", sign::hex(&key.public()));
            Ok(())
        }
        UpdateAction::TrustBuilder { machine, key } => {
            let (peer, name) = paired(&machine)?;
            let key = match key {
                Some(key) => key,
                None => Store::of(&layout).seen_signer(&name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "no build from {name} has reached this machine yet; run \
                         `cursio update key` on {name} and pass it with --key"
                    )
                })?,
            };
            sign::parse_hex::<32>(&key)
                .ok_or_else(|| anyhow::anyhow!("`{key}` is not a 64-character hex key"))?;
            let mut config = Config::load(config_path)?;
            config.update.trusted_builders.retain(|b| b.peer != peer);
            config.update.trusted_builders.push(TrustedBuilder {
                peer,
                name: name.clone(),
                key: key.clone(),
            });
            let switched = config.update.channel != UpdateChannel::Dev;
            config.update.channel = UpdateChannel::Dev;
            config.save(config_path)?;
            println!(
                "builds from {name} signed by {} are now accepted here",
                &key[..12]
            );
            if switched {
                println!("this machine now follows the dev channel");
            }
            Ok(())
        }
        UpdateAction::UntrustBuilder { machine } => {
            let mut config = Config::load(config_path)?;
            let before = config.update.trusted_builders.len();
            config
                .update
                .trusted_builders
                .retain(|b| b.name != machine && b.peer.to_string() != machine);
            if config.update.trusted_builders.len() == before {
                anyhow::bail!("{machine} is not a trusted builder here");
            }
            config.save(config_path)?;
            println!("builds from {machine} are no longer accepted here");
            Ok(())
        }
        UpdateAction::Status => {
            println!("install root: {}", layout.root().display());
            let current = layout.current();
            let previous = layout.previous();
            let installed = layout.installed();
            if installed.is_empty() {
                println!("nothing installed");
            }
            for version in installed {
                let mark = match (
                    Some(&version) == current.as_ref(),
                    Some(&version) == previous.as_ref(),
                ) {
                    (true, _) => "  current",
                    (_, true) => "  previous",
                    _ => "",
                };
                println!("{version}{mark}");
            }
            Ok(())
        }
        UpdateAction::Install {
            dir,
            delta_from,
            activate,
        } => {
            std::fs::create_dir_all(layout.root())?;
            let version = match delta_from {
                Some(from) => layout.install_delta(&dir, &Manifest::read(&dir)?, &from)?,
                None => layout.install(&dir)?,
            };
            println!("installed {version}");
            if activate {
                layout.activate(&version)?;
                for (path, e) in layout.refresh_stable_copies() {
                    eprintln!("could not refresh {}: {e}", path.display());
                }
                println!("current is now {version}");
            }
            Ok(())
        }
        UpdateAction::Apply { version, run } => switch(&layout, run.plan(version)).await,
        UpdateAction::Rollback { run } => {
            let previous = layout
                .previous()
                .ok_or_else(|| anyhow::anyhow!("there is no previous version to go back to"))?;
            switch(&layout, run.plan(previous)).await
        }
        UpdateAction::Prune { keep } => {
            for gone in layout.prune(keep, &[]) {
                println!("removed {gone}");
            }
            Ok(())
        }
    }
}

/// A paired machine by name or id: trusting a builder presumes pairing, since
/// its builds reach this machine only over a link to it or one of its peers.
fn paired(machine: &str) -> anyhow::Result<(cursio_protocol::PeerId, String)> {
    let store = cursio_transport::pairing::TrustStore::load()?;
    let found = store
        .peers()
        .find(|p| p.display_name == machine || p.peer_id.to_string() == machine)
        .map(|p| (p.peer_id, p.display_name.clone()));
    found.ok_or_else(|| {
        anyhow::anyhow!("{machine} is not paired with this machine; `cursio peers` lists who is")
    })
}

async fn switch(layout: &InstallLayout, plan: Plan) -> anyhow::Result<()> {
    if apply::relocate_if_installed(layout)? {
        return Ok(());
    }
    let apply::Outcome::Switched { from, to } = apply::run(layout, &plan).await?;
    match from {
        Some(from) if from != to => println!("switched {from} -> {to}"),
        _ => println!("current is {to}"),
    }
    Ok(())
}
