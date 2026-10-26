//! `cursio` CLI — discovery, pairing, and sessions between machines on a LAN.
//!
//! **There is no server.** Every machine runs the same command, `mesh`, and
//! every node both accepts and dials: if one goes away the others keep talking
//! to each other directly and reconnect when it returns. Nothing is a hub.
//!
//! Subcommands:
//! - `mesh` — run a node: listen, dial every trusted peer, run the modules
//!   `config.toml` names (flags override it for one run).
//! - `config` — print the config `mesh` would run with.
//! - `peers` — list peers discovered on the LAN and exit.
//!
//! Typical use: `cursio mesh --pair` on both machines once, with the same
//! password, then `cursio mesh` on every machine, every time. The password is
//! read from the terminal without echoing it or exposing it in the process
//! list.
//!
//! (`serve`/`connect` are gone: they encoded a client/server split that the mesh
//! model doesn't have. `join` was the last of that split to go — it was the
//! *dialling* half of pairing while `mesh --pair` was the listening half, two
//! commands for one handshake that had to be run on the right two machines in
//! the right order. The mesh already listens and dials at once, so pairing does
//! too, and `should_dial` settles which side places the call.)

use std::sync::Arc;

use crate::airplay::claim_local_airplay;
use crate::airplay::local_airplay_fitness;
use crate::airplay::run_airplay;
use crate::airplay::AirPlayHost;
use crate::airplay::AirPlayHosting;
use crate::airplay::AirPlayMode;
use crate::cli::share_policy;
use crate::cli::Cli;
use crate::cli::Command;
use crate::cli::MeshArgs;
use crate::client::bluetooth;
use crate::client::call;
use crate::client::forget_machine;
use crate::client::phone_screen;
use crate::client::portal;
use crate::client::print_status;
use crate::client::ring_phone;
use crate::client::send_share;
use crate::client::sms;
use crate::input::apply_crossing;
use crate::input::apply_modifier_map;
use crate::input::watch_screens;
use crate::mesh::run_mesh;
use crate::mesh::start_control;
use crate::modules::module_registry;
use crate::modules::start_screen_portal;
use crate::modules::ServiceSetup;
use crate::modules::WindowOpts;
use crate::pairing::list_peers;
use crate::pairing::pair_with_code;
use crate::pairing::read_pairing_password;
use crate::process::restart_process;
use clap::Parser;
use cursio_core::live::Shared;
use cursio_core::pairing_window::PairingWindow;
use cursio_core::NodeIdentity;
use cursio_protocol::{ModuleId, ServiceId, ServiceOffer};

mod airplay;
mod cli;
mod client;
mod input;
mod mesh;
mod modules;
mod pairing;
mod peer_session;
mod process;

fn main() -> anyhow::Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    process::limit_malloc_arenas();
    #[cfg(unix)]
    process::raise_descriptor_limit();
    #[cfg(unix)]
    process::accept_interrupt();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("cannot start the async runtime: {e}"))?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    // The portal backend's screen chooser is this executable in a child
    // process (see `linux_chooser`); it takes no flags a user would type.
    #[cfg(target_os = "linux")]
    if std::env::args().nth(1).as_deref() == Some(cursio_window::linux_chooser::CHOOSER_ARG) {
        std::process::exit(cursio_window::linux_chooser::run_chooser_process());
    }
    // Started as the install's stable copy: run the current version instead.
    if let Some(target) = cursio_core::update::layout::handover("cursio") {
        cursio_core::update::layout::hand_over(&target)?;
        return Ok(());
    }
    let cli = Cli::parse();

    // Its own log file: the node is writing `cursio`'s at the same moment.
    if let Command::MountHelper { parent } = cli.command {
        let _log = cursio_core::logging::init_with_verbosity("cursio-mount-helper", cli.verbose);
        return cursio_core::mount_helper::run_helper(parent).await;
    }

    // Console (info) + rotating file (debug) logging. `_log` must live for the
    // whole run — dropping it flushes and stops the file writer. See
    // `cursio_core::logging` for the filters and log-file location.
    let _log = cursio_core::logging::init_with_verbosity("cursio", cli.verbose);

    // Ask for admin rights before doing anything else, but only for `mesh` —
    // it's the mode that injects input, and Windows' UIPI silently discards
    // injected events aimed at higher-integrity windows (an elevated console,
    // Task Manager) unless we match them. `peers` never injects, so
    // prompting there would be a UAC dialog for nothing.
    //
    // Declining is fine and expected: `ensure_elevated` logs what will and won't
    // work and we carry on. Only `Relaunched` ends this process — an elevated
    // copy is taking over, and two nodes must not share the port and portal.
    if matches!(cli.command, Command::Mesh(_))
        && cursio_core::elevate::ensure_elevated() == cursio_core::elevate::Elevation::Relaunched
    {
        return Ok(());
    }

    let identity = NodeIdentity::load_or_create()?;
    tracing::info!(
        peer_id = %identity.peer_id,
        name = %identity.display_name,
        "this node"
    );

    let config_path = match cli.config {
        Some(path) => path,
        None => cursio_core::config::default_path()?,
    };

    match cli.command {
        Command::Update { action } => cursio_core::update::cli::run(action, &config_path).await,
        Command::Config => {
            let config = cursio_core::config::Config::load(&config_path)?;
            let origin = match config_path.exists() {
                true => "read from",
                false => "defaults; no file at",
            };
            println!("# {origin} {}", config_path.display());
            print!("{}", config.to_toml()?);
            Ok(())
        }
        Command::Status { json } => print_status(json).await,
        Command::Pair { code } => pair_with_code(code).await,
        Command::Mesh(args) => {
            let overrides = args.overrides();
            let config =
                cursio_core::config::Config::load(&config_path)?.with_overrides(&overrides);
            tracing::info!(
                path = %config_path.display(),
                pinned_by_flags = ?overrides.pinned(),
                "config"
            );
            // Before the registry: the update module advertises only once
            // there is an agent to hand builds to.
            cursio_core::update::agent::start(config_path.clone());
            let MeshArgs {
                pair,
                pair_addr,
                files_share: files_share_flag,
                ..
            } = args;
            let pairing = PairingWindow::closed();
            // What a change in the app reaches without a restart: the
            // switches and settings every link follows, and the options the
            // factories read when they build a module.
            let live = cursio_core::live::LiveConfig::new(&config);
            cursio_core::calls_hub::hub().configure(config.calls.clone());
            let screen_host = Shared::new(config.modules.screen_host);
            let files_share =
                Shared::new(share_policy(&files_share_flag, config.files.share.clone())?);
            let render_device = Shared::new(config.audio.render_device.clone());
            let mic_gate = Shared::new(cursio_core::audio_gate::MicGate::from_config(
                config.audio.mic_gate.as_deref(),
            )?);
            let applier: cursio_core::control::Applier = {
                let live = live.clone();
                let screen_host = screen_host.clone();
                let files_share = files_share.clone();
                let render_device = render_device.clone();
                let mic_gate = mic_gate.clone();
                std::sync::Arc::new(move |running, next| {
                    let mut plan = cursio_core::live::plan(running, next);
                    if let Some(on) = plan.screen_host {
                        screen_host.set(on);
                    }
                    if plan.files_share_changed {
                        // A `--files-share` flag replaces the file's policy for
                        // this run, so the file changing changes nothing.
                        if files_share_flag.is_empty() {
                            match share_policy(&files_share_flag, next.files.share.clone()) {
                                Ok(policy) => files_share.set(policy),
                                Err(error) => {
                                    tracing::warn!(%error, "keeping the running share policy");
                                    plan.restart.retain(|id| *id != ModuleId::FILES);
                                }
                            }
                        } else {
                            plan.restart.retain(|id| *id != ModuleId::FILES);
                        }
                    }
                    if let Some(device) = &plan.render_device {
                        render_device.set(device.clone());
                    }
                    if let Some(gate) = &plan.mic_gate {
                        match cursio_core::audio_gate::MicGate::from_config(gate.as_deref()) {
                            Ok(gate) => mic_gate.set(gate),
                            Err(error) => tracing::warn!(%error, "keeping the running mic gate"),
                        }
                    }
                    if let Some(calls) = &plan.calls {
                        cursio_core::calls_hub::hub().configure(calls.clone());
                    }
                    if let Some(spec) = &plan.mac_modifiers {
                        if let Err(error) = apply_modifier_map(spec) {
                            tracing::warn!(%error, "keeping the running modifier map");
                        }
                    }
                    if let Some(crossing) = &plan.crossing {
                        apply_crossing(crossing);
                    }
                    tracing::info!(
                        switches = ?plan.switches,
                        position = ?plan.position,
                        input = ?plan.input,
                        restart = ?plan.restart,
                        "config changed; applying it to the running node"
                    );
                    live.apply(&plan);
                })
            };
            let control = cursio_core::control::NodeState::new(
                &identity,
                config_path.clone(),
                overrides.clone(),
                config.clone(),
                pairing.clone(),
                applier,
            );
            {
                let live = live.clone();
                control.set_on_forget(std::sync::Arc::new(move |peer| live.forget(peer)));
            }
            {
                let live = live.clone();
                control.set_on_arrange(std::sync::Arc::new(move |peer, request| {
                    live.request_arrange(peer, request)
                }));
            }
            watch_screens(live.clone(), control.clone());
            start_control(control.clone());
            let cursio_core::config::Config {
                port,
                modules,
                airplay: airplay_config,
                input: input_config,
                ..
            } = config;
            let send_input = modules.input;
            let remote_screen = modules.remote_screen;
            let share_airplay = airplay_config.share;
            let airplay = airplay_config.mode;
            let airplay_window = airplay_config.window;
            let airplay_port = airplay_config
                .port
                .unwrap_or(cursio_airplay::DEFAULT_CONTROL_PORT);
            let airplay_interface = airplay_config.interface;
            let mac_modifiers = input_config.mac_modifiers;
            apply_modifier_map(&mac_modifiers)?;
            apply_crossing(&input_config.crossing);
            let airplay =
                AirPlayMode::parse(&airplay).map_err(|e| anyhow::anyhow!("--airplay: {e}"))?;
            // Checked here rather than with clap's `requires`, which cannot see
            // inside a value: a machine that took itself out of AirPlay has
            // nothing to share, and starting anyway would leave peers waiting
            // on a stream that never comes.
            if (share_airplay || airplay_window) && !airplay.participates() {
                anyhow::bail!(
                    "--airplay off leaves nothing to show or share; drop it, or drop \
                     --airplay-window/--share-airplay"
                );
            }
            // What this machine brings to the mesh's choice of host, and what
            // it wants from it. Both travel in the greeting, so placement is
            // settled the moment two machines meet.
            let offer = ServiceOffer {
                fitness: local_airplay_fitness(airplay, airplay_port),
                wanted: airplay.wanted(),
                show_window: airplay_window,
            };
            tracing::info!(
                fitness = ?offer.fitness,
                wanted = ?offer.wanted.map(|q| q.to_string()),
                window = airplay_window,
                "AirPlay: what this machine offers the mesh"
            );
            // Read before anything binds a port, so a mistyped password costs
            // nothing and the prompt is not buried under startup logging.
            if pair {
                let password = read_pairing_password()?;
                tracing::info!(
                    minutes = cursio_core::pairing_window::PASSWORD_LIFETIME.as_secs() / 60,
                    "pairing window open; run `cursio mesh --pair` on the other machine too"
                );
                pairing.open_password(password, pair_addr);
            }
            // Everything the service module needs, built before the registry
            // so the same state is read by the AirPlay bring-up below.
            let service_state = cursio_core::service_session::ServiceState::new();
            let service_reasons: Arc<
                std::sync::Mutex<std::collections::HashMap<ServiceId, String>>,
            > = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            service_state.note_offer(identity.peer_id, offer);

            // Every participating receiver has a tap. It is also the sink that
            // lets a selected host accept mirroring without opening a local
            // window; the window module subscribes only for peers that asked
            // for one (or when --share-airplay explicitly asks for all peers).
            let airplay_tap = airplay.participates().then(cursio_airplay::MirrorTap::new);

            // The CLI's only job for modules is to build the registry; from here
            // on nothing names a specific module.
            let modules = module_registry(
                render_device,
                mic_gate,
                control.mixer(),
                files_share,
                live.switches(),
                WindowOpts {
                    airplay: airplay_tap.clone().map(|tap| {
                        cursio_core::window_session::AirPlayShare {
                            tap,
                            state: service_state.clone(),
                            local: identity.peer_id,
                            force: share_airplay,
                        }
                    }),
                    remote_screen,
                    screen_host,
                    screen_hub: start_screen_portal(remote_screen),
                },
                ServiceSetup {
                    state: service_state.clone(),
                    reasons: service_reasons.clone(),
                    make_host: {
                        let state = service_state.clone();
                        // `off` refuses rather than declining to answer. A peer
                        // that asked and heard nothing waits forever for a
                        // receiver; one that hears "no" asks the next machine.
                        match airplay.participates() {
                            false => Box::new(|| {
                                Box::new(cursio_core::service_session::RefuseAll)
                                    as Box<dyn cursio_core::service_session::ServiceHost>
                            }),
                            true => Box::new(move || {
                                Box::new(AirPlayHost {
                                    state: state.clone(),
                                })
                                    as Box<dyn cursio_core::service_session::ServiceHost>
                            }),
                        }
                    },
                },
            );
            // Every desktop has an input backend; the rest follow the build.
            control.set_available(cursio_core::control::ModuleSwitch::available(
                &modules.available(),
                true,
            ));
            let mesh = run_mesh(
                identity.clone(),
                port,
                send_input,
                live,
                modules,
                pairing,
                offer,
                control.clone(),
            );
            let node = async move {
                if !airplay.participates() {
                    // Nothing bound and nothing announced, and the service module
                    // is refusing on this machine's behalf, so a peer that asks
                    // learns to look elsewhere. With every machine here, AirPlay
                    // does not run at all — which is what asking for it means.
                    tracing::info!("AirPlay is off on this machine");
                    return mesh.await;
                }
                // Is another cursio on *this* machine already announcing? No
                // peer can answer that — both processes are the same peer — so
                // a lock file settles it locally, before anything binds.
                let airplay_lock = claim_local_airplay();
                run_airplay(
                    AirPlayHosting {
                        identity: identity.clone(),
                        port: airplay_port,
                        interface: airplay_interface,
                        show_window: airplay_window,
                        tap: airplay_tap,
                        may_announce: airplay_lock.is_some(),
                        state: service_state.clone(),
                        reasons: service_reasons.clone(),
                    },
                    mesh,
                )
                .await
            };
            // The file changing under the node — a text editor, a script — is
            // applied the way a change from the app is. The app's own changes
            // are applied before its reply and come back through here as no
            // change at all.
            let watcher = control.clone();
            tokio::spawn(async move {
                loop {
                    let running = watcher.running();
                    let next = cursio_core::config::wait_for_change(
                        watcher.config_path(),
                        watcher.overrides(),
                        &running,
                    )
                    .await;
                    let waiting = watcher.apply(next);
                    if !waiting.is_empty() {
                        tracing::warn!(
                            settings = ?waiting,
                            "these settings changed and apply only after a restart"
                        );
                    }
                }
            });
            tokio::select! {
                result = node => return result,
                _ = control.restart_requested() => {
                    tracing::info!("restart requested over the control socket");
                }
            };
            restart_process("asked to").await
        }
        Command::Forget { machine } => forget_machine(&machine, &config_path).await,
        Command::Wake { machine } => {
            let name = cursio_core::wake::wake(&machine)?;
            println!("sent a wake-up to {name}; it joins the mesh once it is up");
            Ok(())
        }
        Command::Send {
            target,
            to,
            copy,
            save,
        } => send_share(&target, &to, copy, save).await,
        Command::Ring { phone } => ring_phone(&phone).await,
        Command::Screen { phone, stop } => phone_screen(&phone, !stop).await,
        Command::Peers { timeout_secs } => list_peers(identity, timeout_secs).await,
        Command::Call { action } => call(action).await,
        Command::Bluetooth { action } => bluetooth(action),
        Command::Sms { action } => sms(action).await,
        Command::Portal { action } => portal(action).await,
        Command::MountHelper { .. } => unreachable!("handled before the node starts"),
    }
}
