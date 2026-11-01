//! What an application and the Cursio node on its machine say to each other.
//!
//! An application is never a member of the mesh: it does not pair, holds no
//! key and opens no session. It talks to the one node on its own machine over
//! a local socket, and the node carries what the application asked for across
//! the mesh. This crate is that local vocabulary and nothing else, so that a
//! program linking `cursio-sdk` links no Cursio internals — the node is GPL,
//! and the line between the two is a socket, not a library.
//!
//! ## Stable by construction
//!
//! Cursio updates itself across the mesh on its own schedule, and an
//! application built against an older SDK must keep working through that. So
//! the first message names [`PROTOCOL`], the node answers with the highest
//! version both know, messages are added and never changed, and the bytes of
//! every message are pinned by the tests at the bottom of this file. A node
//! that does not know a message answers [`NodeMsg::Unsupported`] rather than
//! closing, because silence would be indistinguishable from a node that hung.
//!
//! ## Framing
//!
//! One JSON object per line, each way, at most [`MAX_LINE`] bytes. JSON for
//! the same reason the node's control socket uses it: a person debugging an
//! application can read the conversation with `socat`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The version of this vocabulary an SDK built from this crate speaks.
pub const PROTOCOL: u32 = 1;

/// Longest line either side accepts.
pub const MAX_LINE: usize = 64 * 1024;

/// The socket's file name inside the node's private runtime directory.
pub const SOCKET_NAME: &str = "apps.sock";

/// Longest [`AppId`].
pub const MAX_APP_ID: usize = 32;

/// Longest detail a [`ClientMsg::Health`] carries.
pub const MAX_DETAIL: usize = 512;

/// An application's name on the mesh: what a machine agrees to take builds
/// of, and what `cursio logs --app` asks for. The same id on two machines is
/// the same application.
///
/// Lowercase ASCII letters, digits and `-`, at most [`MAX_APP_ID`]: it ends up
/// in directory names on every platform and in commands people type. `cursio`
/// is Cursio's own and cannot be taken.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AppId(String);

impl AppId {
    pub fn parse(id: &str) -> Result<Self, Error> {
        let well_formed = !id.is_empty()
            && id.len() <= MAX_APP_ID
            && !id.starts_with('-')
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !well_formed {
            return Err(Error::BadAppId(id.to_owned()));
        }
        if id == "cursio" {
            return Err(Error::ReservedAppId);
        }
        Ok(AppId(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AppId {
    type Error = Error;
    fn try_from(id: String) -> Result<Self, Error> {
        AppId::parse(&id)
    }
}

impl From<AppId> for String {
    fn from(id: AppId) -> String {
        id.0
    }
}

/// Where an application writes its log: the directory, and the file's base
/// name as its appender writes it (`leavepulse.log`). Daily-rotated siblings
/// (`leavepulse.log.2026-11-01`) are read with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSpec {
    pub dir: PathBuf,
    pub file: String,
}

impl LogSpec {
    /// An absolute directory and a bare file name. Only the application
    /// names these, over a socket only its user can open, and the node then
    /// reads only that file — but a relative directory would mean whatever
    /// the node's working directory is, and a file name with a separator in
    /// it would reach outside the directory the application named.
    pub fn check(&self) -> Result<(), Error> {
        if !self.dir.is_absolute() {
            return Err(Error::BadLogSpec(format!(
                "the log directory {} is not absolute",
                self.dir.display()
            )));
        }
        let bare = !self.file.is_empty()
            && self.file != "."
            && self.file != ".."
            && !self.file.contains(['/', '\\'])
            && Path::new(&self.file).file_name().is_some();
        if !bare {
            return Err(Error::BadLogSpec(format!(
                "{:?} is not a bare file name",
                self.file
            )));
        }
        Ok(())
    }
}

/// How the application says it is doing, in its own judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    #[default]
    Ok,
    /// Working, but something it relies on is not.
    Degraded,
    /// Not doing its job.
    Failing,
}

/// What an application says to the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ClientMsg {
    /// The first line on every connection.
    Hello {
        protocol: u32,
        app: AppId,
        version: String,
        pid: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        logs: Option<LogSpec>,
    },
    Health {
        state: HealthState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// What the node says to an application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum NodeMsg {
    /// The answer to `Hello`: the version both sides speak from now on, and
    /// the name of this machine on the mesh.
    Welcome { protocol: u32, machine: String },
    /// The node will not take this application; the connection closes.
    Refused { reason: String },
    /// The node does not know the message named `feature`; the connection
    /// stays open.
    Unsupported { feature: String },
    /// A new build of this application came over the mesh and is installed
    /// in `dir`, which its install root's `current` now names. What runs is
    /// the application's to restart; nothing has been stopped.
    BuildReady { version: String, dir: PathBuf },
}

/// Something wrong with a message or a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    BadAppId(String),
    ReservedAppId,
    BadLogSpec(String),
    TooLong,
    Malformed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadAppId(id) => write!(
                f,
                "{id:?} is not an application id; use up to {MAX_APP_ID} lowercase letters, digits and `-`"
            ),
            Error::ReservedAppId => f.write_str("`cursio` is Cursio's own id; pick another"),
            Error::BadLogSpec(why) => f.write_str(why),
            Error::TooLong => write!(f, "a line is longer than {MAX_LINE} bytes"),
            Error::Malformed(why) => write!(f, "not a message: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// One message as a line, newline included.
pub fn encode_line<T: Serialize>(message: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(message).expect("protocol messages always serialize");
    line.push(b'\n');
    line
}

/// One line, without its newline, as a message.
pub fn decode_line<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Result<T, Error> {
    if line.len() > MAX_LINE {
        return Err(Error::TooLong);
    }
    serde_json::from_slice(line).map_err(|e| Error::Malformed(e.to_string()))
}

/// The `msg` a line names, when it is an object that names one: what a side
/// that cannot decode the line reports as [`NodeMsg::Unsupported`].
pub fn message_kind(line: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    value.get("msg")?.as_str().map(str::to_owned)
}

/// Where the node listens for applications of this user.
///
/// Unix: [`SOCKET_NAME`] in `$XDG_RUNTIME_DIR/cursio`, or in
/// `<temp>/cursio-<uid>` when that variable is unset — the directory the
/// node makes private to its user. Windows: `\\.\pipe\cursio-apps-<user>`.
pub fn endpoint() -> PathBuf {
    #[cfg(unix)]
    {
        runtime_dir().join(SOCKET_NAME)
    }
    #[cfg(not(unix))]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
        PathBuf::from(format!(r"\\.\pipe\cursio-apps-{}", pipe_safe(&user)))
    }
}

/// The node's private runtime directory, as [`endpoint`] resolves it.
#[cfg(unix)]
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("cursio"),
        _ => {
            // SAFETY: getuid cannot fail and touches no memory.
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("cursio-{uid}"))
        }
    }
}

/// A user name as it may appear in a pipe name: anything but letters,
/// digits, `-` and `_` becomes `_`.
#[cfg_attr(unix, allow(dead_code))]
fn pipe_safe(user: &str) -> String {
    user.chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                true => c,
                false => '_',
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<T: Serialize>(message: &T) -> String {
        String::from_utf8(encode_line(message)).unwrap()
    }

    #[test]
    fn hello_has_the_bytes_old_nodes_read() {
        let hello = ClientMsg::Hello {
            protocol: 1,
            app: AppId::parse("leavepulse").unwrap(),
            version: "0.6.0".into(),
            pid: 42,
            logs: Some(LogSpec {
                dir: "/home/u/.local/share/leavepulse/logs".into(),
                file: "leavepulse.log".into(),
            }),
        };
        let text = line(&hello);
        assert_eq!(
            text,
            "{\"msg\":\"hello\",\"protocol\":1,\"app\":\"leavepulse\",\"version\":\"0.6.0\",\"pid\":42,\"logs\":{\"dir\":\"/home/u/.local/share/leavepulse/logs\",\"file\":\"leavepulse.log\"}}\n"
        );
        assert_eq!(
            decode_line::<ClientMsg>(text.trim_end().as_bytes()).unwrap(),
            hello
        );
    }

    #[test]
    fn hello_without_logs_leaves_the_field_out() {
        let hello = ClientMsg::Hello {
            protocol: 1,
            app: AppId::parse("demo").unwrap(),
            version: "1".into(),
            pid: 7,
            logs: None,
        };
        assert_eq!(
            line(&hello),
            "{\"msg\":\"hello\",\"protocol\":1,\"app\":\"demo\",\"version\":\"1\",\"pid\":7}\n"
        );
    }

    #[test]
    fn health_has_the_bytes_old_nodes_read() {
        let health = ClientMsg::Health {
            state: HealthState::Degraded,
            detail: Some("no api".into()),
        };
        assert_eq!(
            line(&health),
            "{\"msg\":\"health\",\"state\":\"degraded\",\"detail\":\"no api\"}\n"
        );
    }

    #[test]
    fn node_messages_have_the_bytes_old_sdks_read() {
        assert_eq!(
            line(&NodeMsg::Welcome {
                protocol: 1,
                machine: "Nocturne".into()
            }),
            "{\"msg\":\"welcome\",\"protocol\":1,\"machine\":\"Nocturne\"}\n"
        );
        assert_eq!(
            line(&NodeMsg::Refused {
                reason: "no".into()
            }),
            "{\"msg\":\"refused\",\"reason\":\"no\"}\n"
        );
        assert_eq!(
            line(&NodeMsg::Unsupported {
                feature: "call".into()
            }),
            "{\"msg\":\"unsupported\",\"feature\":\"call\"}\n"
        );
        assert_eq!(
            line(&NodeMsg::BuildReady {
                version: "0.6.1".into(),
                dir: PathBuf::from("/apps/leavepulse/versions/0.6.1"),
            }),
            "{\"msg\":\"build_ready\",\"version\":\"0.6.1\",\"dir\":\"/apps/leavepulse/versions/0.6.1\"}\n"
        );
    }

    #[test]
    fn an_app_id_is_a_short_lowercase_name_and_never_cursio() {
        assert!(AppId::parse("leavepulse").is_ok());
        assert!(AppId::parse("my-app-2").is_ok());
        assert!(AppId::parse("").is_err());
        assert!(AppId::parse("LeavePulse").is_err());
        assert!(AppId::parse("../etc").is_err());
        assert!(AppId::parse("-flag").is_err());
        assert!(AppId::parse(&"a".repeat(33)).is_err());
        assert_eq!(AppId::parse("cursio"), Err(Error::ReservedAppId));
    }

    #[test]
    fn an_invalid_app_id_on_the_wire_does_not_decode() {
        let line = br#"{"msg":"hello","protocol":1,"app":"../x","version":"1","pid":1}"#;
        assert!(decode_line::<ClientMsg>(line).is_err());
    }

    #[test]
    fn a_log_spec_names_one_file_in_an_absolute_directory() {
        let spec = |dir: &str, file: &str| LogSpec {
            dir: dir.into(),
            file: file.into(),
        };
        assert!(spec("/var/log/app", "app.log").check().is_ok());
        assert!(spec("logs", "app.log").check().is_err());
        assert!(spec("/var/log/app", "../secret").check().is_err());
        assert!(spec("/var/log/app", "a\\b").check().is_err());
        assert!(spec("/var/log/app", "..").check().is_err());
        assert!(spec("/var/log/app", "").check().is_err());
    }

    #[test]
    fn an_unknown_message_still_says_what_it_is() {
        let line = br#"{"msg":"call","name":"reload"}"#;
        assert!(decode_line::<ClientMsg>(line).is_err());
        assert_eq!(message_kind(line).as_deref(), Some("call"));
    }

    #[test]
    fn an_overlong_line_is_refused_before_parsing() {
        let line = vec![b' '; MAX_LINE + 1];
        assert_eq!(decode_line::<ClientMsg>(&line), Err(Error::TooLong));
    }

    #[test]
    fn a_user_name_is_made_safe_for_a_pipe_name() {
        assert_eq!(pipe_safe("Иван Smith"), "_____Smith");
        assert_eq!(pipe_safe("the-roer_1"), "the-roer_1");
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_lives_in_the_nodes_private_runtime_directory() {
        assert_eq!(endpoint(), runtime_dir().join("apps.sock"));
    }
}
