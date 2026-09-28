//! `cursio update …`: the subcommands, kept here so the binary only names them.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;

use super::apply::{self, Plan};
use super::layout::InstallLayout;
use super::manifest::Manifest;

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

pub async fn run(action: UpdateAction) -> anyhow::Result<()> {
    let layout = InstallLayout::for_user()?;
    match action {
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
