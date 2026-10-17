//! Sending one thing to another machine to act on: a link to open, text to
//! paste, a file to keep.
//!
//! The clipboard already moves what was copied, but only into the other
//! clipboard, and only as a side effect of copying. Sharing is the deliberate
//! act — "open this page on the PC", "put this photo in its Downloads" — so it
//! names what the receiver should do with it ([`ShareAction`]) rather than
//! leaving the user to finish the job on the far screen.
//!
//! ## Offer, then pull, like everything else that can be large
//!
//! A link or a few lines of text ride the offer itself. A file does not: the
//! offer carries its name and size, and the receiver pulls the bytes with
//! [`ShareMsg::Request`], streamed as [`ShareMsg::Data`] on the bulk class so a
//! video never delays the cursor. Pulling also makes refusal free — a receiver
//! that will not take it says so before a byte has crossed.
//!
//! ## Every share is answered
//!
//! [`ShareMsg::Done`] tells the sender what became of it — opened, saved where,
//! refused why — so the phone can say "opened on Nocturne" instead of hoping.

use serde::{Deserialize, Serialize};

/// Identifies one share within a link, chosen by its sender.
pub type ShareId = u64;

/// The longest text or link carried inline in an offer. Anything longer is
/// not something a person meant to send as text.
pub const MAX_INLINE_TEXT: usize = 64 * 1024;

/// What is being sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareContent {
    /// A web address. Only `http` and `https` are opened by a receiver: a
    /// trusted peer may drive this machine's cursor, but a link that runs a
    /// program is not a page to look at.
    Url(String),
    Text(String),
    /// A file, pulled on request. `name` is the sender's file name and nothing
    /// more — a receiver treats it as untrusted input and never as a path.
    File {
        name: String,
        size: u64,
        mime: Option<String>,
    },
}

/// What the receiver is asked to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShareAction {
    /// Open it: a link in the browser, a file in its default application.
    Open,
    /// Put it on the clipboard.
    Copy,
    /// Keep the file in the receiver's downloads.
    Save,
}

/// What became of a share, as its receiver reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareOutcome {
    Opened,
    Copied,
    /// Saved, under the name it was saved as (which may differ, to avoid
    /// overwriting a file already there).
    Saved {
        name: String,
    },
    /// Waiting for the receiving person to act — a phone cannot open a page
    /// from the background, so it asks with a notification.
    Offered,
    /// Not done, and why, in words for the sender to show.
    Refused(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareMsg {
    /// Sender -> receiver: here is something to act on.
    Offer {
        id: ShareId,
        content: ShareContent,
        action: ShareAction,
    },
    /// Receiver -> sender: send the file's bytes.
    Request { id: ShareId },
    /// Sender -> receiver: one chunk of a requested file, in order.
    Data {
        id: ShareId,
        seq: u32,
        last: bool,
        data: Vec<u8>,
    },
    /// Either way: this share is over. From the receiver, what it did; from the
    /// sender, that it cannot deliver (its file went away mid-transfer).
    Done { id: ShareId, outcome: ShareOutcome },
}

impl ShareMsg {
    /// Bulk, all of it: an offer must not overtake nor be overtaken by the
    /// file it announces, and the file is why the class exists.
    pub const PRIORITY: crate::Priority = crate::Priority::Bulk;
}

impl ShareContent {
    /// A short description for logs and notifications: the link, the start of
    /// the text, or the file name.
    pub fn summary(&self) -> String {
        match self {
            ShareContent::Url(url) => url.clone(),
            ShareContent::Text(text) => text.chars().take(60).collect(),
            ShareContent::File { name, .. } => name.clone(),
        }
    }
}
