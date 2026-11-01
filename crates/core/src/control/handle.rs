//! Answering a command: the ones the node settles at once, the ones that
//! wait on a peer, and editing the config file a setting lives in.

use crate::config::Config;
use crate::control::wire::GalleryAsk;
use crate::control::Command;
use crate::control::NodeState;
use crate::control::Reply;
use crate::control::CONTROL_VERSION;
use crate::pairing_window::PairingCode;
use std::path::Path;

/// Answer a gallery command by asking the machine it names.
pub async fn ask_gallery(command: &Command) -> Option<Reply> {
    use base64::Engine as _;
    use cursio_protocol::gallery::GalleryResponse;
    let hub = crate::gallery_hub::hub();
    let reply = match command.gallery_ask()? {
        GalleryAsk::Request(peer, request) => match hub.ask(peer, request).await {
            Ok(GalleryResponse::Items(items)) => Reply::Media { items },
            Ok(GalleryResponse::Thumbnail(jpeg)) => Reply::Thumbnail {
                jpeg: jpeg.map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
            },
            Ok(GalleryResponse::Slice(bytes)) => Reply::Slice {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
            Err(message) => Reply::Error { message },
        },
        GalleryAsk::Save {
            peer,
            item,
            name,
            dir,
        } => {
            let peer_name = hub
                .peers()
                .into_iter()
                .find(|p| p.peer.to_string() == peer || p.name.eq_ignore_ascii_case(peer))
                .map(|p| p.name)
                .unwrap_or_else(|| peer.to_string());
            // The socket is this user's alone, so a folder it names is one the
            // user chose; the item's own name is still made safe by `save`.
            let dir = dir
                .map(Path::to_path_buf)
                .or_else(|| crate::gallery_hub::save_dir(&peer_name));
            match dir {
                None => Reply::Error {
                    message: "this machine names no Pictures or Downloads folder to save into"
                        .into(),
                },
                Some(dir) => match hub.save(peer, item, name, &dir).await {
                    Ok(path) => Reply::SavedFile { path },
                    Err(message) => Reply::Error { message },
                },
            }
        }
    };
    Some(reply)
}

/// Answer a diagnostics command: a log here or on a peer, or a session's
/// paths seen, or chosen, from both ends.
/// A peer's window still, kept beside this machine's phone screenshots.
fn save_window_shot(machine: &str, app: &str, png: &[u8]) -> Result<std::path::PathBuf, String> {
    let dir = crate::data_dir::data_dir()
        .map(|d| d.join("screenshots"))
        .ok_or("no data directory to write the still in")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe: String = format!("{machine}-{app}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = dir.join(format!("{safe}-{stamp}.png"));
    std::fs::write(&path, png).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

async fn ask_diag(command: &Command) -> Option<Reply> {
    use cursio_protocol::diag::{DiagRequest, DiagResponse};
    let hub = crate::diag_session::hub();
    let logs = |result: Result<DiagResponse, String>| match result {
        Ok(DiagResponse::Logs { lines, truncated }) => Reply::LogLines { lines, truncated },
        Ok(other) => error(format!("a log request was answered with {other:?}")),
        Err(message) => Reply::Error { message },
    };
    Some(match command {
        Command::Logs {
            machine,
            after,
            within_secs,
            contains,
            max_lines,
            log,
        } => match machine {
            None => logs(crate::diag_session::own_log(crate::log_tail::Tail {
                after: after.clone(),
                within: within_secs.map(std::time::Duration::from_secs),
                contains: contains.clone(),
                max_lines: *max_lines,
                log: log.clone(),
            })),
            Some(machine) => {
                let request = DiagRequest::Logs {
                    after: after.clone(),
                    within_secs: *within_secs,
                    contains: contains.clone(),
                    max_lines: *max_lines,
                    log: log.clone(),
                };
                logs(hub.ask(machine, request).await)
            }
        },
        Command::Report {
            machine,
            since_secs,
        } => {
            let report = match machine {
                None => {
                    crate::diag_session::own_report(std::time::Duration::from_secs(*since_secs))
                }
                Some(machine) => match hub
                    .ask(
                        machine,
                        DiagRequest::Report {
                            since_secs: *since_secs,
                        },
                    )
                    .await
                {
                    Ok(DiagResponse::Report(report)) => Ok(*report),
                    Ok(other) => Err(format!("a report request was answered with {other:?}")),
                    Err(e) => Err(e),
                },
            };
            match report {
                Ok(report) => Reply::Report(Box::new(report)),
                Err(message) => Reply::Error { message },
            }
        }
        Command::Maintain { machine, what } => {
            match hub.ask(machine, DiagRequest::Maintain(*what)).await {
                Ok(DiagResponse::Maintaining(_)) => Reply::Done,
                Ok(other) => error(format!("a maintenance request was answered with {other:?}")),
                Err(message) => Reply::Error { message },
            }
        }
        Command::Launch {
            machine,
            app,
            version,
            component,
            restart,
            start,
            args,
        } => {
            let request = DiagRequest::Launch {
                app: app.clone(),
                version: version.clone(),
                component: component.clone(),
                restart: *restart,
                start: *start,
                args: args.clone(),
            };
            match hub.ask(machine, request).await {
                Ok(DiagResponse::Launched { what }) => Reply::Launched { what },
                Ok(other) => error(format!("a launch was answered with {other:?}")),
                Err(message) => Reply::Error { message },
            }
        }
        Command::AppActions { machine, app } => {
            let request = DiagRequest::AppActions { app: app.clone() };
            match hub.ask(machine, request).await {
                Ok(DiagResponse::Actions(actions)) => Reply::Actions { actions },
                Ok(other) => error(format!("a list of actions was answered with {other:?}")),
                Err(message) => Reply::Error { message },
            }
        }
        Command::AppSend {
            machine,
            app,
            topic,
            text,
        } => {
            let (id, topic) = match (
                cursio_app_protocol::AppId::parse(app),
                cursio_app_protocol::TopicName::parse(topic),
            ) {
                (Ok(id), Ok(topic)) => (id, topic),
                (Err(e), _) | (_, Err(e)) => return Some(error(e.to_string())),
            };
            match crate::apps::mesh::send(&id, None, machine, &topic, text.clone().into_bytes())
                .await
            {
                Ok(copies) => Reply::AppSent { copies },
                Err(failure) => Reply::Error {
                    message: failure.reason,
                },
            }
        }
        Command::AppPeers { app } => match cursio_app_protocol::AppId::parse(app) {
            Ok(id) => Reply::AppPeers {
                peers: crate::apps::mesh::peers(&id),
            },
            Err(e) => error(e.to_string()),
        },
        Command::CallAction {
            machine,
            app,
            process,
            action,
            args_json,
        } => {
            let request = DiagRequest::CallAction {
                app: app.clone(),
                process: process.clone(),
                action: action.clone(),
                args_json: args_json.clone(),
            };
            match hub.ask(machine, request).await {
                Ok(DiagResponse::Called { result_json }) => Reply::Called { result_json },
                Ok(other) => error(format!("a call was answered with {other:?}")),
                Err(message) => Reply::Error { message },
            }
        }
        Command::WindowShot {
            machine,
            target,
            max_side,
        } => {
            let request = DiagRequest::WindowShot {
                target: target.clone(),
                max_side: *max_side,
            };
            match hub.ask(machine, request).await {
                Ok(DiagResponse::WindowShot {
                    png, width, height, ..
                }) => match save_window_shot(machine, &target.app, &png) {
                    Ok(path) => Reply::Screenshot {
                        path,
                        width,
                        height,
                    },
                    Err(message) => Reply::Error { message },
                },
                Ok(other) => error(format!("a window still was answered with {other:?}")),
                Err(message) => Reply::Error { message },
            }
        }
        Command::WindowAct {
            machine,
            target,
            input,
        } => {
            let request = DiagRequest::WindowInput {
                target: target.clone(),
                input: input.clone(),
            };
            match hub.ask(machine, request).await {
                Ok(DiagResponse::WindowDone { what }) => Reply::Acted { what },
                Ok(other) => error(format!("window input was answered with {other:?}")),
                Err(message) => Reply::Error { message },
            }
        }
        Command::Paths { peer, choice } => {
            let Some(id) = hub
                .peers()
                .into_iter()
                .find(|p| p.peer.to_string() == *peer || p.name.eq_ignore_ascii_case(peer))
                .map(|p| p.peer)
            else {
                return Some(error(format!(
                    "no connected machine called {peer} answers diagnostics"
                )));
            };
            let here = match crate::diag_session::paths(&id, *choice) {
                Ok(here) => here,
                Err(message) => return Some(Reply::Error { message }),
            };
            let (there, there_error) =
                match hub.ask(peer, DiagRequest::Paths { choice: *choice }).await {
                    Ok(DiagResponse::Paths(report)) => (Some(report), None),
                    Ok(other) => (None, Some(format!("answered with {other:?}"))),
                    Err(e) => (None, Some(e)),
                };
            Reply::Paths {
                here,
                there,
                there_error,
            }
        }
        _ => return None,
    })
}

/// Answer a command that waits on a peer; anything else goes to `state`.
/// How long a screenshot with `ask` waits for the phone's person to allow
/// sharing and the first picture to arrive.
const SCREEN_ALLOW_WAIT: std::time::Duration = std::time::Duration::from_secs(45);

/// A phone's screen read or driven by a program here; `None` for any other
/// command.
async fn phone_remote(command: &Command) -> Option<Reply> {
    let answer = |result: Result<Reply, String>| match result {
        Ok(reply) => reply,
        Err(message) => Reply::Error { message },
    };
    Some(match command {
        Command::PhoneAct { phone, action } => answer(phone_act(phone, action).await),
        Command::PhoneScreenshot {
            phone,
            max_side,
            ask,
        } => answer(phone_screenshot(phone, *max_side, *ask).await),
        _ => return None,
    })
}

#[cfg(feature = "window")]
async fn phone_act(
    phone: &str,
    action: &crate::phone_action::PhoneAction,
) -> Result<Reply, String> {
    crate::phone_screen::remote::act(phone, action)
        .await
        .map(|what| Reply::Acted { what })
}

#[cfg(not(feature = "window"))]
async fn phone_act(
    phone: &str,
    _action: &crate::phone_action::PhoneAction,
) -> Result<Reply, String> {
    Err(format!(
        "this build cannot reach {phone}'s screen: it has no window module"
    ))
}

#[cfg(all(feature = "phone-control", any(target_os = "linux", windows)))]
async fn phone_screenshot(phone: &str, max_side: Option<u32>, ask: bool) -> Result<Reply, String> {
    use crate::phone_screen::{remote, shot};
    let mut stream = remote::stream(phone);
    if ask && stream.is_err() {
        crate::phone_screen::ask(phone, true)?;
        let deadline = tokio::time::Instant::now() + SCREEN_ALLOW_WAIT;
        while stream.is_err() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            stream = remote::stream(phone);
        }
    }
    let stream = stream?;
    let dir = shot::dir().ok_or("no data directory to write the screenshot in")?;
    // Decoding a phone's stream from its keyframe is tens of milliseconds of
    // CPU: off the runtime's workers.
    let shot = tokio::task::spawn_blocking(move || shot::take(&stream, max_side, &dir))
        .await
        .map_err(|e| format!("the screenshot did not finish: {e}"))??;
    Ok(Reply::Screenshot {
        path: shot.path,
        width: shot.width,
        height: shot.height,
    })
}

#[cfg(not(all(feature = "phone-control", any(target_os = "linux", windows))))]
async fn phone_screenshot(
    phone: &str,
    _max_side: Option<u32>,
    _ask: bool,
) -> Result<Reply, String> {
    Err(format!(
        "this build cannot take a still of {phone}'s screen: it has no video decoder"
    ))
}

pub async fn handle_waiting(state: &NodeState, command: Command) -> Reply {
    use cursio_protocol::messages::MessagesResponse;
    if let Some(reply) = ask_gallery(&command).await {
        return reply;
    }
    if let Some(reply) = ask_diag(&command).await {
        return reply;
    }
    if let Some(reply) = phone_remote(&command).await {
        return reply;
    }
    if let Command::Ring { peer } = &command {
        return match crate::notify_hub::hub().ring_confirmed(peer, true).await {
            Ok(name) => {
                tracing::info!(phone = %name, "the phone is ringing");
                Reply::Done
            }
            Err(message) => {
                tracing::warn!(%peer, %message, "the phone did not ring");
                Reply::Error { message }
            }
        };
    }
    let Some((peer, request)) = command.messages_request() else {
        return state.handle(command);
    };
    match crate::messages_hub::hub().ask(peer, request).await {
        Ok(MessagesResponse::Conversations(conversations)) => {
            Reply::Conversations { conversations }
        }
        Ok(MessagesResponse::Messages(messages)) => Reply::Messages { messages },
        Ok(MessagesResponse::Sent | MessagesResponse::Done) => Reply::Done,
        Err(message) => Reply::Error { message },
    }
}

pub(super) fn error(e: impl std::fmt::Display) -> Reply {
    Reply::Error {
        message: e.to_string(),
    }
}

/// Which installed version this process is, read off its own path: a stable
/// copy has already handed over to `versions/<v>/`, so the path names it.
pub(super) fn running_install_version() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    crate::update::layout::InstallLayout::for_user()
        .ok()?
        .version_of(&exe)
}

impl NodeState {
    pub(super) fn pinned(&self) -> Vec<String> {
        self.overrides
            .pinned()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Change the file with `edit`, reporting which touched fields a flag
    /// overrides anyway.
    pub(super) fn edit_file(&self, touched: &[&str], edit: impl FnOnce(&mut Config)) -> Reply {
        let mut file = match Config::load(&self.config_path) {
            Ok(file) => file,
            Err(e) => return error(e),
        };
        edit(&mut file);
        if let Err(e) = file.save(&self.config_path) {
            return error(e);
        }
        let pinned = self.overrides.pinned();
        Reply::Saved {
            pinned: touched
                .iter()
                .filter(|f| pinned.contains(f))
                .map(|f| (*f).to_owned())
                .collect(),
            restart_needed: self.apply(file.with_overrides(&self.overrides)),
        }
    }

    pub fn handle(&self, command: Command) -> Reply {
        match command {
            Command::Ping => Reply::Pong {
                version: CONTROL_VERSION,
            },
            Command::Status => Reply::Status(Box::new(self.status())),
            Command::Config => match Config::load(&self.config_path) {
                Ok(file) => Reply::Config {
                    path: self.config_path.clone(),
                    effective: Box::new(file.clone().with_overrides(&self.overrides)),
                    file: Box::new(file),
                    pinned: self.pinned(),
                },
                Err(e) => error(e),
            },
            Command::SetConfig { config } => {
                if let Err(e) = config.save(&self.config_path) {
                    return error(e);
                }
                Reply::Saved {
                    pinned: self.pinned(),
                    restart_needed: self.apply((*config).with_overrides(&self.overrides)),
                }
            }
            Command::SetModule { module, enabled } => {
                self.edit_file(&[module.field()], |c| *module.slot(c) = enabled)
            }
            Command::SetPosition { position } => {
                self.edit_file(&["position"], |c| c.position = position)
            }
            Command::Arrange { machine, place } => self.arrange(machine, place),
            Command::ArrangeScreen {
                machine,
                screen,
                dx_mm,
                dy_mm,
            } => self.arrange_screen(
                machine,
                cursio_protocol::ScreenOffset {
                    screen,
                    dx_mm,
                    dy_mm,
                },
            ),
            Command::Barrier {
                screen,
                peer,
                peer_screen,
                closed,
            } => self.set_barrier(screen, peer, peer_screen, closed),
            Command::PairShow => {
                self.pairing.open_shown_code();
                self.changed();
                self.pairing_reply()
            }
            Command::PairEnter { code } => match PairingCode::parse(&code) {
                Ok(code) => {
                    self.pairing.open_entered_code(&code, None);
                    self.changed();
                    self.pairing_reply()
                }
                Err(e) => error(e),
            },
            Command::PairCancel => {
                self.pairing.cancel();
                self.changed();
                self.pairing_reply()
            }
            Command::Pairing => self.pairing_reply(),
            Command::Restart => {
                self.restart.notify_one();
                Reply::Done
            }
            // The connection, not the node, keeps subscriptions; see
            // `transport::serve_connection`.
            Command::Subscribe => Reply::Subscribed,
            // The guard lives on the connection; see `serve_connection`.
            Command::TrayAttached => Reply::Done,
            Command::ShowScreenSource { peer, source } => {
                match crate::screen_views::pick(&peer, source) {
                    Ok(()) => Reply::Done,
                    Err(e) => error(e),
                }
            }
            Command::UpdateApply => match crate::update::agent::global() {
                Some(agent) => match agent.apply_ready() {
                    Ok(_) => {
                        self.changed();
                        Reply::Done
                    }
                    Err(e) => error(e),
                },
                None => Reply::Error {
                    message: "updates are not running on this node".into(),
                },
            },
            Command::Forget { peer } => self.forget(peer),
            Command::SetName { name } => self.rename(&name),
            Command::Share {
                peer,
                target,
                action,
            } => match crate::share_sink::share(&peer, &target, action) {
                Ok(()) => Reply::Done,
                Err(e) => error(e),
            },
            Command::Call { phone, action } => match crate::calls_hub::hub().act(&phone, action) {
                Ok(()) => Reply::Done,
                Err(e) => error(e),
            },
            Command::SetStreamVolume {
                stream,
                volume,
                muted,
            } => {
                if self.mixer.set(stream, volume, muted) {
                    self.changed();
                    Reply::Done
                } else {
                    error(format!("no stream {stream} is playing here"))
                }
            }
            #[cfg(feature = "window")]
            Command::PhoneScreen { peer, show } => match crate::phone_screen::ask(&peer, show) {
                Ok(name) => {
                    tracing::info!(phone = %name, show, "asking a phone about its screen");
                    Reply::Done
                }
                Err(e) => error(e),
            },
            #[cfg(not(feature = "window"))]
            Command::PhoneScreen { .. } => {
                error("this build has no window module to show a phone's screen in")
            }
            Command::Ring { peer } => match crate::notify_hub::hub().ring(&peer, true) {
                Ok(name) => {
                    tracing::info!(phone = %name, "asking a phone to ring");
                    Reply::Done
                }
                Err(e) => error(e),
            },
            // They wait on a phone, which this synchronous path must not;
            // the socket answers them through `handle_waiting`.
            Command::ListConversations { .. }
            | Command::ListMessages { .. }
            | Command::SendSms { .. }
            | Command::MarkSmsRead { .. } => {
                error("a phone's messages are read from a desktop's control socket")
            }
            Command::GalleryList { .. }
            | Command::GalleryThumbnail { .. }
            | Command::GalleryRead { .. }
            | Command::GallerySave { .. } => {
                error("a machine's gallery is browsed from a desktop's control socket")
            }
            // They wait on a peer; the socket answers them through
            // `handle_waiting`.
            Command::Logs { .. }
            | Command::Paths { .. }
            | Command::Report { .. }
            | Command::Maintain { .. }
            | Command::Launch { .. }
            | Command::PhoneScreenshot { .. }
            | Command::PhoneAct { .. }
            | Command::AppActions { .. }
            | Command::CallAction { .. }
            | Command::AppSend { .. }
            | Command::AppPeers { .. }
            | Command::WindowShot { .. }
            | Command::WindowAct { .. } => {
                error("logs and paths are asked through a desktop's control socket")
            }
        }
    }

    pub(super) fn pairing_reply(&self) -> Reply {
        Reply::Pairing {
            state: self.pairing.state(),
            last: self.pairing.last_outcome(),
            nearby: self.inner().nearby(),
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::config::Config;
    use crate::config::Overrides;
    use crate::control::test_support::state;
    use crate::control::test_support::state_applying;
    use crate::control::test_support::temp_dir;
    use crate::control::Command;
    use crate::control::KnownPeerView;
    use crate::control::ModuleSwitch;
    use crate::control::Presence;
    use crate::control::Reply;
    use cursio_protocol::PeerId;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[test]
    fn showing_a_phone_that_is_not_linked_answers_with_an_error() {
        let dir = temp_dir("phone-screen");
        let node = state(&dir, Overrides::default());
        let reply = node.handle(Command::PhoneScreen {
            peer: "nobody-here".into(),
            show: true,
        });
        assert!(
            matches!(&reply, Reply::Error { message } if message.contains("cannot show its screen")),
            "{reply:?}"
        );
    }

    #[test]
    fn ringing_a_phone_that_is_not_linked_answers_with_an_error() {
        let dir = temp_dir("ring");
        let node = state(&dir, Overrides::default());
        let reply = node.handle(Command::Ring {
            peer: "nobody-here".into(),
        });
        assert!(
            matches!(&reply, Reply::Error { message } if message.contains("cannot be rung")),
            "{reply:?}"
        );
    }

    #[test]
    fn a_stream_volume_reaches_the_engines_mixer_and_the_status() {
        let dir = temp_dir("volume");
        let node = state(&dir, Overrides::default());
        let refused = node.handle(Command::SetStreamVolume {
            stream: 7,
            volume: 0.5,
            muted: false,
        });
        assert!(matches!(refused, Reply::Error { .. }), "got {refused:?}");

        let engines = node.mixer();
        engines.add(7, "Nocturne", "Firefox", crate::mixer::StreamKind::App);
        let reply = node.handle(Command::SetStreamVolume {
            stream: 7,
            volume: 0.5,
            muted: false,
        });
        assert_eq!(reply, Reply::Done);
        assert_eq!(engines.level(7), 0.5);
        assert_eq!(node.status().streams[0].volume, 0.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_a_machine_that_was_never_paired_is_refused_and_leaves_the_mesh_alone() {
        let dir = temp_dir("forget");
        let node = state(&dir, Overrides::default());
        let told = Arc::new(Mutex::new(Vec::new()));
        let seen = told.clone();
        node.set_on_forget(Arc::new(move |peer| seen.lock().unwrap().push(peer)));

        let reply = node.handle(Command::Forget {
            peer: PeerId::new_v4(),
        });

        assert!(matches!(reply, Reply::Error { .. }), "got {reply:?}");
        assert!(told.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_module_toggle_is_written_to_the_file_and_a_pinned_one_says_so() {
        let dir = temp_dir("toggle");
        let node = state(
            &dir,
            Overrides {
                audio: Some(true),
                ..Overrides::default()
            },
        );
        let Reply::Saved { pinned, .. } = node.handle(Command::SetModule {
            module: ModuleSwitch::Usb,
            enabled: true,
        }) else {
            panic!("expected saved");
        };
        assert!(pinned.is_empty());
        assert!(Config::load(&dir.join("config.toml")).unwrap().modules.usb);

        let Reply::Saved { pinned, .. } = node.handle(Command::SetModule {
            module: ModuleSwitch::Audio,
            enabled: false,
        }) else {
            panic!("expected saved");
        };
        assert_eq!(pinned, vec!["modules.audio".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_toggle_is_applied_before_the_reply_and_only_a_restart_only_setting_is_named() {
        let dir = temp_dir("apply");
        let applied = Arc::new(Mutex::new(Vec::new()));
        let seen = applied.clone();
        let node = state_applying(
            &dir,
            Overrides::default(),
            Arc::new(move |_: &Config, next: &Config| seen.lock().unwrap().push(next.clone())),
        );
        let Reply::Saved { restart_needed, .. } = node.handle(Command::SetModule {
            module: ModuleSwitch::Files,
            enabled: false,
        }) else {
            panic!("expected saved");
        };
        assert!(restart_needed.is_empty());
        assert!(!applied.lock().unwrap()[0].modules.files);
        assert!(!node.status().modules.files);

        let Reply::Saved { restart_needed, .. } = node.handle(Command::SetModule {
            module: ModuleSwitch::RemoteScreen,
            enabled: true,
        }) else {
            panic!("expected saved");
        };
        assert_eq!(restart_needed, vec!["modules.remote_screen".to_owned()]);
        assert_eq!(node.status().restart_needed, restart_needed);

        // The watcher reading the same file again applies nothing twice.
        node.apply_file().unwrap();
        assert_eq!(applied.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shown_code_is_reported_and_a_bad_entered_one_is_refused() {
        let dir = temp_dir("pair");
        let node = state(&dir, Overrides::default());
        let Reply::Pairing { state: Some(s), .. } = node.handle(Command::PairShow) else {
            panic!("expected an open window");
        };
        assert!(s.code.is_some());
        assert_eq!(node.status().pairing, Some(s));
        assert!(matches!(
            node.handle(Command::PairEnter {
                code: "nope".into()
            }),
            Reply::Error { .. }
        ));
        node.handle(Command::PairCancel);
        assert_eq!(node.status().pairing, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pairing_window_lists_strangers_before_machines_already_paired() {
        let dir = temp_dir("nearby");
        let node = state(&dir, Overrides::default());
        let paired = PeerId::from_u128(1);
        let stranger = PeerId::from_u128(2);
        node.set_known(vec![KnownPeerView {
            id: paired,
            name: "Alpha".into(),
            presence: Presence::Offline,
        }]);
        node.saw(paired, "Alpha", "192.168.1.2:46073".parse().unwrap());
        node.saw(stranger, "Zulu", "192.168.1.3:46073".parse().unwrap());

        let Reply::Pairing { nearby, .. } = node.handle(Command::PairShow) else {
            panic!("expected a pairing reply");
        };
        let listed: Vec<(&str, bool)> =
            nearby.iter().map(|m| (m.name.as_str(), m.paired)).collect();
        assert_eq!(listed, vec![("Zulu", false), ("Alpha", true)]);
        assert_eq!(node.status().nearby, nearby);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pairing_window_with_nobody_heard_lists_nobody() {
        let dir = temp_dir("nobody");
        let node = state(&dir, Overrides::default());
        let Reply::Pairing { nearby, .. } = node.handle(Command::PairShow) else {
            panic!("expected a pairing reply");
        };
        assert!(nearby.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hearing_the_same_machine_again_wakes_no_subscriber_but_a_move_does() {
        let dir = temp_dir("resighting");
        let node = state(&dir, Overrides::default());
        let peer = PeerId::from_u128(7);
        let addr: std::net::SocketAddr = "10.0.0.7:46073".parse().unwrap();
        node.saw(peer, "Phone", addr);
        let mut revisions = node.revisions();
        revisions.mark_unchanged();

        node.saw(peer, "Phone", addr);
        assert!(!revisions.has_changed().unwrap());

        node.saw(peer, "Phone", "10.0.0.8:46073".parse().unwrap());
        assert!(revisions.has_changed().unwrap());
        assert_eq!(node.status().nearby[0].addr.to_string(), "10.0.0.8:46073");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gallery_save_names_its_folder_or_leaves_it_to_the_node() {
        let older: Command = serde_json::from_str(
            r#"{"cmd":"gallery_save","peer":"POCO","item":"7","name":"a.jpg"}"#,
        )
        .unwrap();
        let Command::GallerySave { dir, .. } = older else {
            panic!("not a gallery save: {older:?}");
        };
        assert_eq!(dir, None);

        let chosen: Command = serde_json::from_str(
            r#"{"cmd":"gallery_save","peer":"POCO","item":"7","name":"a.jpg","dir":"/tmp/x"}"#,
        )
        .unwrap();
        let Command::GallerySave { dir, .. } = chosen else {
            panic!("not a gallery save: {chosen:?}");
        };
        assert_eq!(dir.as_deref(), Some(Path::new("/tmp/x")));
    }
}
