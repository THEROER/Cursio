//! Reading, writing, and watching the system clipboard — the platform layer.
//!
//! This crate is the foundation two very different things are built on:
//!
//! - **`clipd`**, a standalone clipboard-history manager (a Win+V for Linux):
//!   watch the clipboard, keep the last N items, put one back on paste.
//! - **Cursio**, which shares a clipboard between two machines over the network.
//!
//! They look unrelated, but both are the same job — *get multi-format content
//! off the local clipboard, and put multi-format content back on it* — differing
//! only in where the content then goes (a database, or a socket). That common
//! job lives here, so neither has to reimplement the fiddly platform parts, and
//! this crate depends on neither of them.
//!
//! Because the meeting point is the *system* clipboard, the two compose for
//! free when both run: content Cursio receives from another machine lands on
//! the local clipboard, which `clipd` observes as an ordinary copy and records.
//! No integration code exists between them, and none is needed.
//!
//! ## Shape
//!
//! [`ClipboardContent`] (in [`format`]) is the currency: one copy, all its
//! representations. [`Clipboard`] is the backend trait — read the current
//! content, write new content, and watch for changes — implemented per
//! platform behind [`open`], the same trait-per-backend split the rest of the
//! workspace uses for input and audio.
//!
//! ## Laziness
//!
//! [`read`](Clipboard::read) does **not** fetch payloads. It returns entries
//! that know their format and size, with bytes left unloaded, because a copy
//! can be tens of megabytes and most copies are never used. Call
//! [`load`](Clipboard::load) for the representation you actually want. A
//! watcher that ignores this and eagerly loads everything will do a great deal
//! of pointless I/O on every copy.

pub mod format;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod windows;

pub use format::{ClipboardContent, ClipboardEntry, ClipboardFormat};

/// Which system clipboard to act on.
///
/// X11 and Wayland both carry two independent selections, and conflating them
/// is a classic source of surprise: the *primary* selection is middle-click
/// paste, updated merely by selecting text with the mouse. A history manager
/// that recorded it would fill with fragments the user never deliberately
/// copied, so [`Clipboard`](Selection::Clipboard) is the default and the only
/// one worth recording by default. Windows has no equivalent of primary, and
/// its backend ignores the distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Selection {
    /// The ordinary clipboard: explicit Ctrl+C / Ctrl+V.
    #[default]
    Clipboard,
    /// X11/Wayland primary selection: select-to-copy, middle-click-to-paste.
    Primary,
}

/// Errors from a clipboard backend.
#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    /// The clipboard holds nothing (or nothing in a usable format).
    #[error("clipboard is empty")]
    Empty,
    /// The requested representation isn't offered by the current owner.
    ///
    /// Possible even right after a [`read`](Clipboard::read) listed it: some
    /// sources render formats on demand and can fail at that point, and the
    /// clipboard owner may have changed in between.
    #[error("clipboard format not available: {0}")]
    FormatUnavailable(String),
    /// The clipboard owner vanished mid-transfer (the source application quit).
    ///
    /// Worth distinguishing from a generic failure because it is routine rather
    /// than exceptional: content is served *by the application that copied it*,
    /// so closing that application legitimately destroys the clipboard.
    #[error("clipboard owner went away before the content could be read")]
    OwnerGone,
    /// The platform clipboard facility failed or is unavailable.
    #[error("clipboard backend failure: {0}")]
    Backend(String),
    /// No backend exists for this platform/session yet.
    #[error("clipboard access is not supported in this session")]
    Unsupported,
}

/// Read/write access to one system clipboard.
///
/// Implementations are platform-specific but must all honour the laziness
/// contract described in the crate docs: [`read`](Clipboard::read) is cheap and
/// payload-free, [`load`](Clipboard::load) is where bytes are actually fetched.
pub trait Clipboard: Send {
    /// What is on the clipboard right now: the available representations and
    /// their sizes, with payloads **not** loaded.
    ///
    /// Returns [`ClipboardError::Empty`] when nothing is on the clipboard.
    fn read(&mut self, selection: Selection) -> Result<ClipboardContent, ClipboardError>;

    /// Fetch the bytes of one representation.
    ///
    /// Separate from [`read`](Clipboard::read) so callers pay for only what
    /// they use. May fail even for a format `read` just reported — see
    /// [`ClipboardError::FormatUnavailable`] and
    /// [`ClipboardError::OwnerGone`].
    fn load(
        &mut self,
        selection: Selection,
        format: &ClipboardFormat,
    ) -> Result<Vec<u8>, ClipboardError>;

    /// Read the clipboard and load every representation in one go.
    ///
    /// The eager convenience path, for when content must be captured *now* —
    /// notably a history watcher, which has to take a durable copy before the
    /// source application exits and takes the content with it (see
    /// [`ClipboardError::OwnerGone`]).
    ///
    /// Representations that fail to load are dropped rather than failing the
    /// whole call: a partial capture (say, text but not an exotic sibling
    /// format) is far more useful than none. Fails only if nothing at all could
    /// be read.
    fn read_loaded(&mut self, selection: Selection) -> Result<ClipboardContent, ClipboardError> {
        let listed = self.read(selection)?;
        let mut loaded = ClipboardContent::default();
        for entry in &listed.entries {
            match self.load(selection, &entry.format) {
                Ok(bytes) => loaded.insert(ClipboardEntry::loaded(entry.format.clone(), bytes)),
                Err(e) => {
                    tracing::debug!(format = %entry.format.mime(), error = %e, "skipping representation that failed to load")
                }
            }
        }
        if loaded.is_empty() {
            return Err(ClipboardError::Empty);
        }
        Ok(loaded)
    }

    /// Put content on the clipboard, replacing what is there.
    ///
    /// Every loaded entry is offered, so a consumer can pick the format it
    /// prefers. Entries without loaded bytes are skipped — there is nothing to
    /// serve for them.
    ///
    /// Note that on X11 and Wayland the clipboard is served by a live process:
    /// whoever writes must stay running to hand the content over on paste. A
    /// short-lived process that writes and exits leaves an empty clipboard,
    /// which is why the Linux backend keeps a serving thread alive.
    fn write(
        &mut self,
        selection: Selection,
        content: &ClipboardContent,
    ) -> Result<(), ClipboardError>;
}

/// Notification that the clipboard changed. Carries no content: the watcher
/// tells you *that* something was copied, and the caller decides what (and
/// whether) to read, keeping the laziness contract intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardChanged {
    /// Which selection changed.
    pub selection: Selection,
}

/// A running clipboard watcher. Dropping it stops the watch.
///
/// Changes arrive on a channel rather than through a callback so a slow
/// consumer can never stall the platform's clipboard thread — the same pattern
/// `cursio-input`'s capture handle and the audio backends use.
pub struct WatchHandle {
    /// Clipboard-change notifications.
    pub changes: tokio::sync::mpsc::UnboundedReceiver<ClipboardChanged>,
    /// Kept so the backend's thread/connection lives as long as the handle;
    /// dropping this shuts the watch down.
    pub(crate) _stop: Box<dyn std::any::Any + Send>,
}

/// Open the clipboard backend for this platform and session.
///
/// On Linux this picks Wayland or X11 by inspecting the session, so callers
/// never branch on display server themselves.
pub fn open() -> Result<Box<dyn Clipboard>, ClipboardError> {
    #[cfg(target_os = "linux")]
    {
        linux::open()
    }
    #[cfg(target_os = "windows")]
    {
        windows::open()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(ClipboardError::Unsupported)
    }
}

/// Start watching the clipboard for changes.
pub fn watch(selection: Selection) -> Result<WatchHandle, ClipboardError> {
    #[cfg(target_os = "linux")]
    {
        linux::watch(selection)
    }
    #[cfg(target_os = "windows")]
    {
        windows::watch(selection)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = selection;
        Err(ClipboardError::Unsupported)
    }
}
