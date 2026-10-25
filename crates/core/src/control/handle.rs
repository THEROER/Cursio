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

/// Answer a command that waits on a peer; anything else goes to `state`.
pub async fn handle_waiting(state: &NodeState, command: Command) -> Reply {
    use cursio_protocol::messages::MessagesResponse;
    if let Some(reply) = ask_gallery(&command).await {
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
                    restart_needed: self.apply(config.with_overrides(&self.overrides)),
                }
            }
            Command::SetModule { module, enabled } => {
                self.edit_file(&[module.field()], |c| *module.slot(c) = enabled)
            }
            Command::SetPosition { position } => {
                self.edit_file(&["position"], |c| c.position = position)
            }
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
