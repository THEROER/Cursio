//! A phone's notifications, shown on the machine the user is sitting at.
//!
//! The phone is the source: Android hands an app with notification access
//! every notification as it is posted and removed, with its buttons and its
//! reply field. A desktop shows each one natively and sends back what the user
//! did with it — pressed a button, typed a reply, closed it — so the phone acts
//! as if that had happened on its own screen.
//!
//! ## Only where the user is
//!
//! Every desktop that got a notification would show it, so a two-monitor,
//! two-machine desk lit up twice for every message. Each desktop says instead
//! whether the user is at it ([`NotifyMsg::Presence`]) — it holds its own
//! cursor and has seen input recently — and the phone sends to those that are.
//! When none is, it sends to all: nobody is looking, and the notification
//! should be waiting wherever they come back to.
//!
//! ## Keys, not ids
//!
//! A notification is named by the phone's own key for it. Android updates a
//! notification in place under the same key (a download's progress, a chat
//! that gained a message), and the desktop replaces what it shows rather than
//! stacking a new one each time.
//!
//! ## Ringing a lost phone
//!
//! The same link carries the one thing a desktop asks of a phone outright:
//! ring, loud, until someone finds it ([`NotifyMsg::Ring`]). It is a device
//! event like the rest, and riding this module means a phone that switched
//! notifications off is also one nobody can make ring — which is the same
//! choice, made once.

use serde::{Deserialize, Serialize};

/// The phone's key for one notification, stable across its updates.
pub type NotificationKey = String;

/// The largest icon carried, in bytes of PNG. An app icon at the size a
/// desktop notification draws it is a few kilobytes; anything far beyond that
/// is a picture, not an icon.
pub const MAX_ICON_BYTES: usize = 64 * 1024;

/// What kind of notification, where it changes how a desktop shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyKind {
    /// An incoming call: shown urgent, and kept until the phone removes it.
    Call,
    /// A conversation: worth a reply field when the phone offers one.
    Message,
    Other,
}

/// One button on a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyAction {
    pub title: String,
    /// The phone takes text with it: a reply.
    pub reply: bool,
}

/// One notification, as the phone posted it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub key: NotificationKey,
    /// The app's package, for filtering and for telling two apps apart.
    pub app_id: String,
    /// The app's name as the user knows it.
    pub app_name: String,
    pub title: String,
    pub text: String,
    /// The app's icon as PNG, at most [`MAX_ICON_BYTES`].
    pub icon: Option<Vec<u8>>,
    /// When the phone says it happened, in Unix milliseconds.
    pub when_ms: i64,
    pub kind: NotifyKind,
    /// In the phone's order; [`NotifyMsg::Invoke`] names one by position.
    pub actions: Vec<NotifyAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyMsg {
    /// Desktop -> phone: whether the user is at this machine now. Sent when
    /// the link comes up and whenever it changes.
    Presence { here: bool },
    /// Phone -> desktop: show this, or replace what is shown under its key.
    Posted(Notification),
    /// Either way: this notification is gone. From the phone, it was dismissed
    /// or withdrawn there; from a desktop, the user closed it here.
    Removed { key: NotificationKey },
    /// Desktop -> phone: the user pressed `action` (a position in
    /// [`Notification::actions`]), with the text typed into it if it is a
    /// reply.
    Invoke {
        key: NotificationKey,
        action: u8,
        reply: Option<String>,
    },
    /// Desktop -> phone: ring loudly until found (`on`), or stop ringing.
    /// The phone stops by itself too, when the user answers it there or after
    /// a while — a phone that is lost may never be reached to be told.
    Ring { on: bool },
    /// Phone -> desktop, after [`NotifyMsg::Posted`] for a call: which of its
    /// buttons answer, decline and end it, as positions in
    /// [`Notification::actions`]. A messenger's call (WhatsApp, Telegram) is
    /// no telephony call the calls module could see, and its buttons are
    /// titled in the phone's language; Android names their roles
    /// (`Notification.CallStyle`), so the desktop can offer it in the same
    /// call window as a phone call.
    CallButtons {
        key: NotificationKey,
        answer: Option<u8>,
        decline: Option<u8>,
        hang_up: Option<u8>,
    },
    /// Desktop -> phone: whether this machine has had input in the last few
    /// seconds (`active`), and how long ago the last input was when that
    /// changed. Finer than [`NotifyMsg::Presence`], which stays true for
    /// minutes: it is what lets the phone tell that the user got up and took
    /// it. Added without a version bump on purpose — a phone that cannot
    /// decode it drops it and keeps deciding by `Presence` alone, which is
    /// exactly what it did before.
    Activity { active: bool, idle_ms: u64 },
}

impl NotifyMsg {
    /// Above bulk: a message that arrives behind a file transfer arrives late,
    /// and late is the one thing a notification must not be. Below audio and
    /// input, which stay smooth while it passes.
    pub const PRIORITY: crate::Priority = crate::Priority::Video;
}
