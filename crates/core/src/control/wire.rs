//! What crosses the control socket: a request, its command, and the reply,
//! as JSON lines both the app and the shell extension write.

use crate::config::Config;
use crate::control::NearbyMachine;
use crate::control::Status;
use crate::pairing_window::PairingOutcome;
use crate::pairing_window::PairingState;
use cursio_protocol::PeerId;
use cursio_protocol::Position;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

/// Bumped when a request or reply changes shape, so a stale app says "update
/// me" rather than misreading a field. v2: `restart_needed` in the status and
/// in every save reply, now that changes apply without a restart. v3:
/// `subscribe`, and status pushed with id 0. v4: the node's `pid`, `build`
/// and `installed_version`, so the app can show and roll back what runs. v5:
/// `update` in the status and `update_apply`, for builds from the mesh. v6:
/// `forget`, removing a machine from the mesh. v7: `streams` in the status and
/// `set_stream_volume`, the listener's volume on each peer stream. v8:
/// `set_name`, and `available` in the status. v9: `share`, sending a link,
/// text or file to a peer. v10: a phone's messages — `list_conversations`,
/// `list_messages`, `send_sms`, `mark_sms_read`, and `messages` in the status.
/// v11: `nearby` in the status and in every pairing reply. v12: `path` on a
/// link, the network or Bluetooth. v13: `inapplicable` on a link, apart from
/// `skipped`. v14: `screens` in the status, `show_screen_source`, and
/// `tray_attached`. v15: a machine's gallery — `gallery_list`,
/// `gallery_thumbnail`, `gallery_read`, `gallery_save`, and `gallery` in the
/// status.
/// v16: `phone_screen`, a phone's screen shown here.
/// v17: `call_setup` in the status, `voice_on` on a phone's calls, and
/// `[calls]` in the config. v18: `kind` on each stream playing here.
/// v19: `dir` on `gallery_save`, a folder to save into other than
/// Pictures/Cursio.
pub const CONTROL_VERSION: u32 = 19;

/// The id a pushed message carries. Requests number from 1.
pub const PUSH_ID: u64 = 0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub reply: Reply,
}

/// A module switch in `config.toml`, by the name the file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleSwitch {
    Input,
    Clipboard,
    Audio,
    Files,
    Usb,
    RemoteScreen,
    ScreenHost,
}

impl ModuleSwitch {
    /// The switches a node built from `modules` can act on. Input is not a
    /// registry module, so whether it can is the caller's to say; one window
    /// module answers for both screen switches.
    pub fn available(modules: &[cursio_protocol::ModuleId], input: bool) -> Vec<Self> {
        use cursio_protocol::ModuleId;
        let mut switches = Vec::new();
        if input {
            switches.push(Self::Input);
        }
        for (id, switch) in [
            (ModuleId::CLIPBOARD, Self::Clipboard),
            (ModuleId::AUDIO, Self::Audio),
            (ModuleId::FILES, Self::Files),
            (ModuleId::USB, Self::Usb),
        ] {
            if modules.contains(&id) {
                switches.push(switch);
            }
        }
        if modules.contains(&ModuleId::WINDOW) {
            switches.extend([Self::RemoteScreen, Self::ScreenHost]);
        }
        switches
    }

    pub(super) fn field(self) -> &'static str {
        match self {
            Self::Input => "modules.input",
            Self::Clipboard => "modules.clipboard",
            Self::Audio => "modules.audio",
            Self::Files => "modules.files",
            Self::Usb => "modules.usb",
            Self::RemoteScreen => "modules.remote_screen",
            Self::ScreenHost => "modules.screen_host",
        }
    }

    pub(super) fn slot(self, config: &mut Config) -> &mut bool {
        let m = &mut config.modules;
        match self {
            Self::Input => &mut m.input,
            Self::Clipboard => &mut m.clipboard,
            Self::Audio => &mut m.audio,
            Self::Files => &mut m.files,
            Self::Usb => &mut m.usb,
            Self::RemoteScreen => &mut m.remote_screen,
            Self::ScreenHost => &mut m.screen_host,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Ping,
    Status,
    /// The file as written, the effective config, and what flags pin.
    Config,
    /// Replace the file with this config.
    SetConfig {
        config: Box<Config>,
    },
    SetModule {
        module: ModuleSwitch,
        enabled: bool,
    },
    SetPosition {
        position: Position,
    },
    /// Open a window with a fresh code for this machine to show.
    PairShow,
    /// Open a window with a code read off another machine.
    PairEnter {
        code: String,
    },
    PairCancel,
    Pairing,
    /// Restart the node from the top, re-reading everything.
    Restart,
    /// Push the status on this connection whenever it changes, starting now.
    Subscribe,
    /// This connection is the Cursio app, whose tray carries the
    /// remote-screen views' selector: the views' own notification-area icons
    /// stand aside until it closes. See [`crate::screen_views`].
    TrayAttached,
    /// Show `source` in the remote-screen view of `peer`, or nothing when
    /// `source` is absent.
    ShowScreenSource {
        peer: String,
        #[serde(default)]
        source: Option<cursio_protocol::SourceId>,
    },
    /// Switch to the build from the mesh that is installed and waiting. The
    /// node stops and comes back as that build, or as this one if it fails.
    UpdateApply,
    /// Remove a paired machine from the mesh: its key, its link, and builder
    /// trust. It has to be paired again to come back. See [`crate::forget`].
    Forget {
        peer: PeerId,
    },
    /// Rename this machine. Saved at once; peers see it once their links are
    /// brought up again, so the status names `name` until a restart.
    SetName {
        name: String,
    },
    /// Act on a phone's calls: `phone` is its name or id. Goes by Bluetooth
    /// hands-free when the phone is reached that way, else over the mesh.
    Call {
        phone: String,
        action: crate::calls_hub::CallAction,
    },
    /// Send a link, text or a file on this machine to a peer, to open, copy
    /// or keep. `peer` is its id or its name.
    Share {
        peer: String,
        target: String,
        action: cursio_protocol::share::ShareAction,
    },
    /// Set the listener's volume (linear, up to [`MAX_LOCAL_VOLUME`]) and
    /// mute on one peer stream playing here. See [`crate::mixer`].
    ///
    /// [`MAX_LOCAL_VOLUME`]: crate::mixer::MAX_LOCAL_VOLUME
    SetStreamVolume {
        // Not `id`: a command is flattened into its `Request`, whose own `id`
        // would collide with it on the wire.
        stream: u32,
        volume: f32,
        muted: bool,
    },
    /// Make a connected phone ring until it is found. `peer` is its id or its
    /// name.
    Ring {
        peer: String,
    },
    /// Show a connected phone's screen in a window here (`show`), driven
    /// with this machine's mouse, or stop. `peer` is its name. The phone asks
    /// its user first, so the window appears only once they allow it.
    PhoneScreen {
        peer: String,
        #[serde(default = "default_true")]
        show: bool,
    },
    /// A phone's conversations, newest first, older than `before_ms` when
    /// given. `peer` is the phone's id or name. Answered by the phone, so it
    /// takes as long as the link does; see [`crate::messages_hub`].
    ListConversations {
        peer: String,
        #[serde(default = "default_page")]
        limit: u32,
        #[serde(default)]
        before_ms: Option<i64>,
    },
    /// One conversation's messages, newest first.
    ListMessages {
        peer: String,
        thread_id: i64,
        #[serde(default = "default_page")]
        limit: u32,
        #[serde(default)]
        before_ms: Option<i64>,
    },
    /// Send an SMS from the phone.
    SendSms {
        peer: String,
        addresses: Vec<String>,
        body: String,
    },
    /// Mark a conversation read on the phone, which only the phone's default
    /// SMS app may do; the refusal says so.
    MarkSmsRead {
        peer: String,
        thread_id: i64,
    },
    /// A machine's photos and videos, newest first, after `after` when
    /// given. `peer` is its id or name; see [`crate::gallery_hub`].
    GalleryList {
        peer: String,
        #[serde(default = "default_page")]
        limit: u32,
        #[serde(default)]
        after: Option<cursio_protocol::gallery::PageMark>,
        #[serde(default)]
        filter: cursio_protocol::gallery::MediaFilter,
    },
    /// One item's thumbnail, a JPEG at most `max_px` on its longer edge.
    GalleryThumbnail {
        peer: String,
        // Not `id`, which the request's own id would collide with.
        item: String,
        max_px: u32,
    },
    /// `len` bytes of one item from `offset`, for a viewer showing it whole.
    GalleryRead {
        peer: String,
        item: String,
        offset: u64,
        len: u32,
    },
    /// Copy one item to this machine's `Pictures/Cursio/<peer>`, or into `dir`
    /// when given: a folder the user picked, or the app's cache for a file it
    /// is about to open, copy or send on.
    GallerySave {
        peer: String,
        item: String,
        name: String,
        #[serde(default)]
        dir: Option<PathBuf>,
    },
}

pub(super) fn default_page() -> u32 {
    50
}

/// A gallery command, ready to put to the machine it names.
pub(super) enum GalleryAsk<'a> {
    Request(&'a str, cursio_protocol::gallery::GalleryRequest),
    Save {
        peer: &'a str,
        item: &'a str,
        name: &'a str,
        dir: Option<&'a Path>,
    },
}

pub(super) fn default_true() -> bool {
    true
}

impl Command {
    /// The request a messages command puts to a phone, and which phone.
    pub(super) fn messages_request(
        &self,
    ) -> Option<(&str, cursio_protocol::messages::MessagesRequest)> {
        use cursio_protocol::messages::MessagesRequest;
        Some(match self {
            Command::ListConversations {
                peer,
                limit,
                before_ms,
            } => (
                peer,
                MessagesRequest::ListConversations {
                    limit: *limit,
                    before_ms: *before_ms,
                },
            ),
            Command::ListMessages {
                peer,
                thread_id,
                limit,
                before_ms,
            } => (
                peer,
                MessagesRequest::ListMessages {
                    thread_id: *thread_id,
                    limit: *limit,
                    before_ms: *before_ms,
                },
            ),
            Command::SendSms {
                peer,
                addresses,
                body,
            } => (
                peer,
                MessagesRequest::Send {
                    addresses: addresses.clone(),
                    body: body.clone(),
                },
            ),
            Command::MarkSmsRead { peer, thread_id } => (
                peer,
                MessagesRequest::MarkRead {
                    thread_id: *thread_id,
                },
            ),
            _ => return None,
        })
    }

    pub(super) fn gallery_ask(&self) -> Option<GalleryAsk<'_>> {
        use cursio_protocol::gallery::GalleryRequest;
        Some(match self {
            Command::GalleryList {
                peer,
                limit,
                after,
                filter,
            } => GalleryAsk::Request(
                peer,
                GalleryRequest::List {
                    limit: *limit,
                    after: after.clone(),
                    filter: *filter,
                },
            ),
            Command::GalleryThumbnail { peer, item, max_px } => GalleryAsk::Request(
                peer,
                GalleryRequest::Thumbnail {
                    id: item.clone(),
                    max_px: *max_px,
                },
            ),
            Command::GalleryRead {
                peer,
                item,
                offset,
                len,
            } => GalleryAsk::Request(
                peer,
                GalleryRequest::Read {
                    id: item.clone(),
                    offset: *offset,
                    len: *len,
                },
            ),
            Command::GallerySave {
                peer,
                item,
                name,
                dir,
            } => GalleryAsk::Save {
                peer,
                item,
                name,
                dir: dir.as_deref(),
            },
            _ => return None,
        })
    }

    /// Whether answering waits on a peer rather than on this node alone. Such
    /// a command is answered off the connection's loop, so the status pushes
    /// and quick requests behind it are not held for seconds.
    pub fn waits_on_peer(&self) -> bool {
        self.messages_request().is_some()
            || self.gallery_ask().is_some()
            || matches!(self, Command::Ring { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Pong {
        version: u32,
    },
    Status(Box<Status>),
    Config {
        path: PathBuf,
        file: Box<Config>,
        effective: Box<Config>,
        pinned: Vec<String>,
    },
    /// The file was written and what it changed is already running.
    /// `pinned` names fields the change could not reach; `restart_needed`
    /// names settings, this one or earlier, that wait for a restart.
    Saved {
        pinned: Vec<String>,
        restart_needed: Vec<String>,
    },
    Pairing {
        state: Option<PairingState>,
        last: Option<PairingOutcome>,
        /// Who a code could reach. Carried with the window rather than only
        /// in the status, because "nobody is visible" is the answer to "why
        /// does the code never get used". Defaulted from a v10 node.
        #[serde(default)]
        nearby: Vec<NearbyMachine>,
    },
    Done,
    /// Status will now be pushed on this connection.
    Subscribed,
    Error {
        message: String,
    },
    /// The machine is no longer part of this mesh.
    Forgotten {
        name: String,
        /// It had been trusted to sign builds for this machine.
        was_builder: bool,
    },
    Conversations {
        conversations: Vec<cursio_protocol::messages::Conversation>,
    },
    Messages {
        messages: Vec<cursio_protocol::messages::TextMessage>,
    },
    /// A page of a machine's gallery.
    Media {
        items: Vec<cursio_protocol::gallery::MediaItem>,
    },
    /// A thumbnail as base64 JPEG, or none where the machine cannot make one.
    Thumbnail {
        jpeg: Option<String>,
    },
    /// Bytes of a gallery item, base64.
    Slice {
        data: String,
    },
    /// Where a gallery item was saved on this machine.
    SavedFile {
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::PeerId;
    use cursio_protocol::Position;

    #[test]
    fn every_command_and_reply_survives_the_wire() {
        let commands = [
            Command::Ping,
            Command::Status,
            Command::SetModule {
                module: ModuleSwitch::RemoteScreen,
                enabled: true,
            },
            Command::SetPosition {
                position: Position::new(-1, 2),
            },
            Command::PairEnter {
                code: "ABCD-EFGH".into(),
            },
            Command::SetConfig {
                config: Box::default(),
            },
            Command::UpdateApply,
            Command::Forget {
                peer: PeerId::from_u128(3),
            },
            Command::SetStreamVolume {
                stream: 4,
                volume: 0.5,
                muted: true,
            },
            Command::ListMessages {
                peer: "POCO".into(),
                thread_id: 12,
                limit: 30,
                before_ms: Some(1_700_000_000_000),
            },
            Command::SendSms {
                peer: "POCO".into(),
                addresses: vec!["+380501234567".into()],
                body: "on my way".into(),
            },
        ];
        for (id, command) in commands.into_iter().enumerate() {
            let request = Request {
                id: id as u64,
                command,
            };
            let line = serde_json::to_string(&request).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        }
        let reply = Response {
            id: 9,
            reply: Reply::Saved {
                pinned: vec!["position".into()],
                restart_needed: vec!["port".into()],
            },
        };
        let line = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&line).unwrap(), reply);
    }

    #[test]
    fn a_ring_request_is_the_line_the_shell_extension_writes() {
        let request: Request =
            serde_json::from_str(r#"{"id":1,"cmd":"ring","peer":"POCO X7 Pro"}"#).unwrap();
        assert_eq!(
            request.command,
            Command::Ring {
                peer: "POCO X7 Pro".into()
            }
        );
    }

    #[test]
    fn a_phone_screen_request_shows_unless_told_to_stop() {
        let request: Request =
            serde_json::from_str(r#"{"id":1,"cmd":"phone_screen","peer":"POCO X7 Pro"}"#).unwrap();
        assert_eq!(
            request.command,
            Command::PhoneScreen {
                peer: "POCO X7 Pro".into(),
                show: true
            }
        );
    }

    #[test]
    fn a_pairing_reply_from_an_older_node_reads_as_nobody_visible() {
        let reply: Reply =
            serde_json::from_str(r#"{"reply":"pairing","state":null,"last":null}"#).unwrap();
        assert_eq!(
            reply,
            Reply::Pairing {
                state: None,
                last: None,
                nearby: Vec::new(),
            }
        );
    }
}
