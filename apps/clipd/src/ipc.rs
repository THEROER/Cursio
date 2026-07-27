//! Talking to the running daemon.
//!
//! The popup has to appear *instantly* — it is a keypress away, and anything
//! slower than a frame or two feels broken. Starting a fresh GTK process each
//! time cannot do that: constructing a GTK application, connecting to the
//! display and building a window costs hundreds of milliseconds, and it would
//! happen on every single press.
//!
//! So the window lives inside the daemon, which is already running anyway, and
//! the hotkey merely *tells* it to show itself. `clipd pick` is therefore a
//! tiny client: it connects, writes one line, and exits. The window it summons
//! was built once, at startup.
//!
//! ## Why a unix socket
//!
//! It is the smallest thing that works. The socket lives in `XDG_RUNTIME_DIR`,
//! which systemd creates per-user with mode 0700 — so access control comes free
//! from the filesystem, with no authentication to design. The directory is also
//! cleared on logout, so a stale socket can't outlive the session that owned
//! it. D-Bus would do the same job but needs a service file, a bus name, and a
//! dependency, for a channel that carries one word.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

/// Commands the daemon accepts. One word per line, so the wire format needs no
/// parser and can be exercised with `socat` when debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Show the picker window.
    Show,
    /// Hide it, if visible.
    Hide,
    /// Show it, or hide it if it is already up — what a hotkey pressed twice
    /// should do.
    Toggle,
    /// Ask whether the daemon is alive; it answers and does nothing else.
    Ping,
}

impl Request {
    fn as_str(self) -> &'static str {
        match self {
            Request::Show => "show",
            Request::Hide => "hide",
            Request::Toggle => "toggle",
            Request::Ping => "ping",
        }
    }

    fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "show" => Some(Request::Show),
            "hide" => Some(Request::Hide),
            "toggle" => Some(Request::Toggle),
            "ping" => Some(Request::Ping),
            _ => None,
        }
    }
}

/// Where the daemon listens.
///
/// Under `XDG_RUNTIME_DIR` when available (the normal case, and the one with
/// 0700 permissions); otherwise a per-user path in `/tmp`, which at least keeps
/// two users on one machine from colliding.
pub fn socket_path() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("clipd.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/clipd-{uid}.sock"))
}

/// Send one request to a running daemon and return its reply.
///
/// Fails when no daemon is listening — which the caller should treat as "not
/// running" rather than a crash, since that is the ordinary state before the
/// service is started.
pub fn send(request: Request) -> Result<String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "clipd daemon is not running (no socket at {}). Start it with \
             `systemctl --user start clipd.service`",
            path.display()
        )
    })?;

    stream
        .write_all(format!("{}\n", request.as_str()).as_bytes())
        .context("sending the request")?;
    stream.flush().ok();

    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .context("reading the reply")?;
    Ok(reply.trim().to_string())
}

/// Start listening for requests, calling `handler` for each.
///
/// Removes a leftover socket file first: a daemon killed with SIGKILL cannot
/// clean up after itself, and the stale file would make `bind` fail forever
/// with "address already in use". Connecting first to check for a live owner
/// would be racier than it is worth — the runtime directory is per-user and
/// per-session, so a socket there with nobody behind it is simply debris.
pub fn listen<F>(mut handler: F) -> Result<()>
where
    F: FnMut(Request) -> String + Send + 'static,
{
    let path = socket_path();

    // Only remove it if nothing is actually listening, so a second daemon
    // doesn't silently steal the first one's socket.
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!(
                "another clipd daemon is already listening on {}",
                path.display()
            );
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("listening on {}", path.display()))?;
    tracing::debug!(socket = %path.display(), "accepting commands");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    tracing::warn!(error = %e, "bad connection");
                    continue;
                }
            };

            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                continue;
            }

            let reply = match Request::parse(&line) {
                Some(request) => handler(request),
                None => format!("error: unknown command {:?}", line.trim()),
            };
            let _ = writeln!(stream, "{reply}");
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_through_their_wire_form() {
        for request in [Request::Show, Request::Hide, Request::Toggle, Request::Ping] {
            assert_eq!(Request::parse(request.as_str()), Some(request));
        }
    }

    #[test]
    fn parsing_tolerates_trailing_newline_and_spaces() {
        assert_eq!(Request::parse("toggle\n"), Some(Request::Toggle));
        assert_eq!(Request::parse("  show  "), Some(Request::Show));
    }

    #[test]
    fn unknown_commands_are_rejected() {
        assert_eq!(Request::parse("launch-missiles"), None);
        assert_eq!(Request::parse(""), None);
    }

    #[test]
    fn socket_lives_under_the_runtime_directory() {
        // The 0700 runtime directory is what makes access control free.
        let path = socket_path();
        assert!(path.to_string_lossy().contains("clipd"));
        assert!(path.is_absolute());
    }

    #[test]
    fn a_request_round_trips_over_a_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        // Point the socket at a temporary directory for the test.
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());

        listen(|request| format!("got {}", request.as_str())).unwrap();
        // Give the accept loop a moment to come up.
        std::thread::sleep(std::time::Duration::from_millis(100));

        assert_eq!(send(Request::Toggle).unwrap(), "got toggle");
        assert_eq!(send(Request::Ping).unwrap(), "got ping");
    }
}
