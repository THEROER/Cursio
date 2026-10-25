//! A phone's text messages, read and sent from this window.

use crate::node::*;
use tauri::State;

/// A phone's conversations, newest first. `peer` is its id.
#[tauri::command]
pub async fn messages_conversations(
    link: State<'_, NodeLink>,
    peer: String,
    limit: u32,
    before_ms: Option<i64>,
) -> Result<Reply, NodeError> {
    link.request_within(
        Command::ListConversations {
            peer,
            limit,
            before_ms,
        },
        PHONE_TIMEOUT,
    )
    .await
}

/// One conversation's messages, newest first.
#[tauri::command]
pub async fn messages_thread(
    link: State<'_, NodeLink>,
    peer: String,
    thread_id: i64,
    limit: u32,
    before_ms: Option<i64>,
) -> Result<Reply, NodeError> {
    link.request_within(
        Command::ListMessages {
            peer,
            thread_id,
            limit,
            before_ms,
        },
        PHONE_TIMEOUT,
    )
    .await
}

#[tauri::command]
pub async fn messages_send(
    link: State<'_, NodeLink>,
    peer: String,
    addresses: Vec<String>,
    body: String,
) -> Result<Reply, NodeError> {
    link.request_within(
        Command::SendSms {
            peer,
            addresses,
            body,
        },
        PHONE_TIMEOUT,
    )
    .await
}

#[tauri::command]
pub async fn messages_mark_read(
    link: State<'_, NodeLink>,
    peer: String,
    thread_id: i64,
) -> Result<Reply, NodeError> {
    link.request_within(Command::MarkSmsRead { peer, thread_id }, PHONE_TIMEOUT)
        .await
}
