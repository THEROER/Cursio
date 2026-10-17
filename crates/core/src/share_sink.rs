//! What a machine does with something shared to it, and how a share is sent.
//!
//! Policy for the share module (`share_session.rs`), split out because the
//! two halves have different owners: the session moves bytes, while acting on
//! them is the machine's — a desktop opens a link itself, a phone may only ask
//! (Android forbids starting an activity from the background). Each node hands
//! the module the [`ShareSink`] that fits it.
//!
//! Sending goes the other way: whoever wants to share — the CLI, the app, a
//! phone's share sheet — reaches the session for a peer through
//! [`ShareOutbox`], the one registry every link registers with.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use cursio_protocol::share::{ShareAction, ShareContent, ShareOutcome};
use cursio_protocol::PeerId;
use tokio::sync::{mpsc, oneshot};

/// How this machine acts on a share that has arrived.
pub trait ShareSink: Send + Sync {
    /// Act on `content`; `file` is where a file's bytes were received.
    /// Called on the share module's own thread, so it may take a moment.
    fn act(
        &self,
        from: &str,
        content: &ShareContent,
        file: Option<&Path>,
        action: ShareAction,
    ) -> ShareOutcome;

    /// Where a file is written while it arrives, before [`Self::act`] moves it.
    fn incoming_dir(&self) -> Option<PathBuf>;
}

/// One share on its way out.
pub struct Outgoing {
    pub content: ShareContent,
    pub action: ShareAction,
    /// The file to read from, for [`ShareContent::File`].
    pub path: Option<PathBuf>,
    /// Told what the receiver did, once it says.
    pub reply: Option<oneshot::Sender<ShareOutcome>>,
}

/// Every link that can take a share, by peer. Process-wide: a link registers
/// when its module starts and is gone when the module drops.
#[derive(Default)]
pub struct ShareOutbox {
    links: Mutex<HashMap<PeerId, Link>>,
    next_token: std::sync::atomic::AtomicU64,
}

struct Link {
    name: String,
    tx: mpsc::UnboundedSender<Outgoing>,
    /// Which registration this is. A reconnect registers the new link before
    /// the old one has finished dropping, and the old one's unregister must
    /// not take the new one away.
    token: u64,
}

pub fn outbox() -> &'static ShareOutbox {
    static OUTBOX: OnceLock<ShareOutbox> = OnceLock::new();
    OUTBOX.get_or_init(ShareOutbox::default)
}

impl ShareOutbox {
    pub(crate) fn register(
        &self,
        peer: PeerId,
        name: String,
    ) -> (mpsc::UnboundedReceiver<Outgoing>, u64) {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = self
            .next_token
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.lock().insert(peer, Link { name, tx, token });
        (rx, token)
    }

    pub(crate) fn unregister(&self, peer: &PeerId, token: u64) {
        let mut links = self.lock();
        if links.get(peer).is_some_and(|l| l.token == token) {
            links.remove(peer);
        }
    }

    /// Peers a share can go to now, by id and name.
    pub fn targets(&self) -> Vec<(PeerId, String)> {
        let mut out: Vec<_> = self
            .lock()
            .iter()
            .filter(|(_, link)| !link.tx.is_closed())
            .map(|(id, link)| (*id, link.name.clone()))
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// Hand a share to `peer`'s link. Fails when no link to it is up.
    pub fn send(&self, peer: &PeerId, share: Outgoing) -> Result<(), String> {
        let links = self.lock();
        let link = links
            .get(peer)
            .ok_or_else(|| "that machine is not connected, or does not take shares".to_string())?;
        link.tx
            .send(share)
            .map_err(|_| format!("the link to {} has just closed", link.name))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, Link>> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Send `target` to `peer` (an id or a name): a file when it names one on
/// this machine, a link when it is a web address, otherwise text. Returns once
/// it is on its way; the receiver's answer is logged.
pub fn share(peer: &str, target: &str, action: ShareAction) -> Result<(), String> {
    if Path::new(target).is_file() {
        let path = Path::new(target);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shared file".into());
        return share_file(peer, path, name, None, action);
    }
    share_text(peer, target, action)
}

/// Send text to `peer`, as a link when it is a web address.
pub fn share_text(peer: &str, text: &str, action: ShareAction) -> Result<(), String> {
    if text.len() > cursio_protocol::share::MAX_INLINE_TEXT {
        return Err("that is too long to send as text; save it to a file and send the file".into());
    }
    let content = if openable_url(text) {
        ShareContent::Url(text.trim().to_string())
    } else {
        ShareContent::Text(text.to_string())
    };
    send_to(peer, content, None, action)
}

/// Send the file at `path` to `peer` under `name` — which a phone needs, since
/// what it shares is first copied to a private file with a name of its own.
pub fn share_file(
    peer: &str,
    path: &Path,
    name: String,
    mime: Option<String>,
    action: ShareAction,
) -> Result<(), String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?
        .len();
    let content = ShareContent::File { name, size, mime };
    send_to(peer, content, Some(path.to_path_buf()), action)
}

fn send_to(
    peer: &str,
    content: ShareContent,
    path: Option<PathBuf>,
    action: ShareAction,
) -> Result<(), String> {
    let targets = outbox().targets();
    let (id, _) = targets
        .iter()
        .find(|(id, name)| id.to_string() == peer || name.eq_ignore_ascii_case(peer))
        .ok_or_else(|| {
            let names: Vec<_> = targets.iter().map(|(_, n)| n.as_str()).collect();
            format!(
                "no connected machine called {peer}; connected: {}",
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            )
        })?;
    outbox().send(
        id,
        Outgoing {
            content,
            action,
            path,
            reply: None,
        },
    )
}

/// Whether a link may be opened: a web page, nothing that runs or reads local
/// files. A trusted peer may drive this machine's cursor, but `file:`,
/// `javascript:` or a custom scheme registered by some installed program is a
/// command, not a page.
pub fn openable_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && !url.chars().any(|c| c.is_control() || c == ' ')
}

/// A file name off the wire made safe to create in a directory: the last
/// component only, no separators or characters Windows refuses, no names
/// Windows reserves, never empty.
pub fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        return "shared file".into();
    }
    let stem = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// A path in `dir` for `name` that does not exist yet: `name`, then
/// `name (1)`, `name (2)`… keeping the extension last.
pub fn unused_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("an unbounded range finds a free name")
}

/// The desktop's sink: opens links and files itself, writes the clipboard,
/// keeps files in Downloads.
pub struct DesktopSink;

impl DesktopSink {
    pub fn shared() -> Arc<dyn ShareSink> {
        Arc::new(DesktopSink)
    }

    fn downloads() -> Option<PathBuf> {
        directories::UserDirs::new().and_then(|d| d.download_dir().map(Path::to_path_buf))
    }

    fn keep(file: &Path, name: &str) -> Result<PathBuf, String> {
        let dir = Self::downloads().ok_or("this machine has no Downloads folder")?;
        let dest = unused_path(&dir, &safe_file_name(name));
        std::fs::rename(file, &dest)
            .or_else(|_| {
                std::fs::copy(file, &dest)
                    .map(drop)
                    .and_then(|_| std::fs::remove_file(file))
            })
            .map_err(|e| format!("could not save into {}: {e}", dir.display()))?;
        Ok(dest)
    }

    fn copy_text(text: &str) -> ShareOutcome {
        let mut content = clipboard_core::ClipboardContent::default();
        content.insert(clipboard_core::ClipboardEntry::loaded(
            clipboard_core::ClipboardFormat::Text,
            text.as_bytes().to_vec(),
        ));
        match clipboard_core::open()
            .and_then(|mut c| c.write(clipboard_core::Selection::Clipboard, &content))
        {
            Ok(()) => ShareOutcome::Copied,
            Err(e) => ShareOutcome::Failed(format!("the clipboard refused it: {e}")),
        }
    }
}

impl ShareSink for DesktopSink {
    fn act(
        &self,
        from: &str,
        content: &ShareContent,
        file: Option<&Path>,
        action: ShareAction,
    ) -> ShareOutcome {
        tracing::info!(from, what = %content.summary(), ?action, "acting on a share");
        match (content, action) {
            (ShareContent::Url(url), ShareAction::Open) if openable_url(url) => {
                match crate::opener::open(url) {
                    Ok(()) => ShareOutcome::Opened,
                    Err(e) => ShareOutcome::Failed(e),
                }
            }
            (ShareContent::Url(_), ShareAction::Open) => {
                ShareOutcome::Refused("only http and https links are opened".into())
            }
            // Text that is a link opens like one; any other text has nowhere
            // to open, so it is put where it can be pasted.
            (ShareContent::Text(text), ShareAction::Open) if openable_url(text) => self.act(
                from,
                &ShareContent::Url(text.trim().to_string()),
                None,
                action,
            ),
            (ShareContent::Url(text) | ShareContent::Text(text), _) => Self::copy_text(text),
            (ShareContent::File { name, .. }, action) => {
                let Some(file) = file else {
                    return ShareOutcome::Failed("the file never arrived".into());
                };
                let saved = match Self::keep(file, name) {
                    Ok(path) => path,
                    Err(e) => return ShareOutcome::Failed(e),
                };
                let name = saved
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match action {
                    ShareAction::Open => match crate::opener::open(&saved.to_string_lossy()) {
                        Ok(()) => ShareOutcome::Opened,
                        Err(e) => ShareOutcome::Failed(e),
                    },
                    ShareAction::Copy | ShareAction::Save => ShareOutcome::Saved { name },
                }
            }
        }
    }

    fn incoming_dir(&self) -> Option<PathBuf> {
        Self::downloads()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_links_are_openable() {
        assert!(openable_url("https://example.com/a?b=c"));
        assert!(openable_url("HTTP://example.com"));
        assert!(!openable_url("file:///etc/passwd"));
        assert!(!openable_url("javascript:alert(1)"));
        assert!(!openable_url("steam://run/440"));
        assert!(!openable_url("https://example.com/ evil"));
    }

    #[test]
    fn a_file_name_off_the_wire_cannot_leave_its_directory() {
        assert_eq!(safe_file_name("../../.bashrc"), "bashrc");
        assert_eq!(safe_file_name("C:\\Windows\\win.ini"), "win.ini");
        assert_eq!(safe_file_name("a<b>:c.txt"), "a_b__c.txt");
        assert_eq!(safe_file_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_file_name("COM1"), "_COM1");
        assert_eq!(safe_file_name("..."), "shared file");
        assert_eq!(safe_file_name("photo.jpg"), "photo.jpg");
    }

    #[test]
    fn a_taken_name_gets_a_number_before_its_extension() {
        let dir = std::env::temp_dir().join(format!("cursio-share-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"").unwrap();
        assert_eq!(unused_path(&dir, "a.txt"), dir.join("a (1).txt"));
        assert_eq!(unused_path(&dir, "b.txt"), dir.join("b.txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
