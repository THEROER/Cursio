//! A phone's text messages, read and written from a desktop.
//!
//! The phone holds the conversations: Android keeps every SMS (and the text of
//! every MMS) in its telephony provider, and an app with `READ_SMS` may read
//! them all. A desktop asks for what it is about to show — the conversation
//! list, one conversation's latest page — and the phone answers that page.
//!
//! ## Asked for, never pushed
//!
//! Years of messages are far larger than anything worth sending on the chance
//! someone looks, and most links never open the messages page at all. So the
//! exchange is a request with an id and a response carrying the same id, like
//! file sharing; the only thing the phone sends unasked is
//! [`MessagesMsg::Changed`], a few bytes saying "look again". A new SMS still
//! appears on the desktop at once without this module: it is a notification,
//! and the notification module shows it.
//!
//! ## Pages run backwards in time
//!
//! Both lists come newest first and are paged with `before_ms`: the date of
//! the oldest item already shown. A conversation is read from its end, which
//! is where the user starts, and a page boundary by date stays correct while
//! new messages arrive — an offset would shift under them.
//!
//! ## What a phone cannot do
//!
//! Since Android 4.4 only the default SMS app may write to the provider, so
//! marking a conversation read can be refused; the refusal says so instead of
//! pretending. MMS pictures, audio and the like are not carried: a message
//! that had one says [`TextMessage::attachment`] so the desktop can show that
//! something is missing rather than an empty bubble.

use serde::{Deserialize, Serialize};

/// The phone's id for one conversation (Android's `thread_id`).
pub type ThreadId = i64;

/// Most items one page may hold. A desktop asks for fewer; this only bounds a
/// request from a peer that asks for everything.
pub const MAX_PAGE: u32 = 200;

/// Longest message body sent, in bytes. A multipart SMS of this length is
/// already dozens of parts; anything longer is a mistake, not a message.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// One conversation in the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub thread_id: ThreadId,
    /// Every other party's address (phone number or short code), in the
    /// phone's order.
    pub addresses: Vec<String>,
    /// The contact's name for each address where the phone has one, else the
    /// address itself; same order as [`Self::addresses`].
    pub names: Vec<String>,
    /// The latest message, as the phone abbreviates it.
    pub snippet: String,
    /// When the latest message was sent or received, in Unix milliseconds.
    pub date_ms: i64,
    /// Whether anything in it is still unread on the phone.
    pub unread: bool,
}

/// Which way a message went, and how far it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageKind {
    /// Received.
    Inbox,
    /// Sent and accepted by the network.
    Sent,
    /// Being sent, or queued to be.
    Outbox,
    /// The network refused it.
    Failed,
    /// A draft or anything else the provider stores.
    Other,
}

/// One message in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMessage {
    /// The phone's id, unique within its kind (`sms` or `mms`) only, so it is
    /// carried together with [`Self::mms`].
    pub id: i64,
    pub mms: bool,
    pub thread_id: ThreadId,
    /// The other party: the sender of a received message, the recipient of a
    /// sent one.
    pub address: String,
    pub body: String,
    /// Unix milliseconds.
    pub date_ms: i64,
    pub kind: MessageKind,
    pub read: bool,
    /// An MMS that carried more than text, which is not shown here.
    pub attachment: bool,
}

/// What a desktop asks a phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessagesRequest {
    /// Conversations, newest first, older than `before_ms` when given.
    ListConversations { limit: u32, before_ms: Option<i64> },
    /// One conversation's messages, newest first, older than `before_ms`.
    ListMessages {
        thread_id: ThreadId,
        limit: u32,
        before_ms: Option<i64>,
    },
    /// Send `body` as an SMS to `addresses` (one for a conversation with one
    /// person; several is a group message sent to each).
    Send {
        addresses: Vec<String>,
        body: String,
    },
    /// Mark every message in the conversation read.
    MarkRead { thread_id: ThreadId },
}

/// A phone's answer to one [`MessagesRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessagesResponse {
    Conversations(Vec<Conversation>),
    Messages(Vec<TextMessage>),
    /// Handed to the phone's radio; how it went shows as the message's
    /// [`MessageKind`] once [`MessagesMsg::Changed`] says to look.
    Sent,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessagesMsg {
    /// Desktop -> phone.
    Request { id: u32, request: MessagesRequest },
    /// Phone -> desktop: the answer to request `id`, or why there is none.
    Response {
        id: u32,
        response: Result<MessagesResponse, String>,
    },
    /// Phone -> desktop: something in the provider changed — a message
    /// arrived, one was sent or failed. Whatever is shown should be asked for
    /// again.
    Changed,
}

impl MessagesMsg {
    /// Below audio and input, which a page of history must never stutter;
    /// above bulk, because someone is looking at the screen waiting for the
    /// page, and a file copy should not stand between them.
    pub const PRIORITY: crate::Priority = crate::Priority::Video;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_request_crosses_the_wire_with_its_reason() {
        let msg = MessagesMsg::Response {
            id: 7,
            response: Err("Android lets only the default SMS app mark messages read".into()),
        };
        let bytes = bincode::serialize(&msg).unwrap();
        assert_eq!(bincode::deserialize::<MessagesMsg>(&bytes).unwrap(), msg);
    }

    #[test]
    fn a_page_of_messages_round_trips() {
        let msg = MessagesMsg::Response {
            id: 1,
            response: Ok(MessagesResponse::Messages(vec![TextMessage {
                id: 42,
                mms: true,
                thread_id: 3,
                address: "+380501234567".into(),
                body: "photo".into(),
                date_ms: 1_700_000_000_000,
                kind: MessageKind::Inbox,
                read: false,
                attachment: true,
            }])),
        };
        let bytes = bincode::serialize(&msg).unwrap();
        assert_eq!(bincode::deserialize::<MessagesMsg>(&bytes).unwrap(), msg);
    }
}
