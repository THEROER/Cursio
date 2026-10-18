//! This phone's text messages, answered for a desktop.
//!
//! `SmsStore` in Kotlin reads the telephony provider and sends through
//! `SmsManager`; this side turns a wire request into the one JSON object it
//! takes and its JSON answer back into the wire vocabulary. JSON rather than a
//! JNI method per request: one entry point for R8 to keep, and Kotlin builds
//! nested lists far more easily as `org.json` than as Java arrays.

use std::sync::Arc;

use cursio_core::messages_hub::MessageStore;
use cursio_protocol::messages::{
    Conversation, MessageKind, MessagesRequest, MessagesResponse, TextMessage,
};
use jni::objects::{JObject, JString, JValue};
use serde::Deserialize;
use serde_json::json;

pub struct AndroidStore;

impl AndroidStore {
    pub fn shared() -> Arc<dyn MessageStore> {
        Arc::new(AndroidStore)
    }
}

impl MessageStore for AndroidStore {
    fn handle(&self, request: &MessagesRequest) -> Result<MessagesResponse, String> {
        let question = request_json(request);
        let answer = crate::jvm::with_native(|env, class| {
            let question = JObject::from(env.new_string(&question)?);
            let result = env.call_static_method(
                class,
                "onSmsRequest",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&question)],
            )?;
            let answer: JString = result.l()?.into();
            let text: String = env.get_string(&answer)?.into();
            Ok(text)
        })?;
        parse_answer(&answer)
    }
}

fn request_json(request: &MessagesRequest) -> String {
    match request {
        MessagesRequest::ListConversations { limit, before_ms } => {
            json!({ "op": "conversations", "limit": limit, "before": before_ms })
        }
        MessagesRequest::ListMessages {
            thread_id,
            limit,
            before_ms,
        } => json!({ "op": "messages", "thread": thread_id, "limit": limit, "before": before_ms }),
        MessagesRequest::Send { addresses, body } => {
            json!({ "op": "send", "addresses": addresses, "body": body })
        }
        MessagesRequest::MarkRead { thread_id } => {
            json!({ "op": "mark_read", "thread": thread_id })
        }
    }
    .to_string()
}

#[derive(Deserialize)]
struct Answer {
    error: Option<String>,
    conversations: Option<Vec<JsonConversation>>,
    messages: Option<Vec<JsonMessage>>,
    #[serde(default)]
    sent: bool,
    #[serde(default)]
    done: bool,
}

#[derive(Deserialize)]
struct JsonConversation {
    thread: i64,
    addresses: Vec<String>,
    names: Vec<String>,
    snippet: String,
    date: i64,
    unread: bool,
}

#[derive(Deserialize)]
struct JsonMessage {
    id: i64,
    mms: bool,
    thread: i64,
    address: String,
    body: String,
    date: i64,
    kind: String,
    read: bool,
    attachment: bool,
}

fn parse_answer(answer: &str) -> Result<MessagesResponse, String> {
    let answer: Answer = serde_json::from_str(answer)
        .map_err(|e| format!("the phone's answer could not be read: {e}"))?;
    if let Some(error) = answer.error {
        return Err(error);
    }
    if let Some(list) = answer.conversations {
        return Ok(MessagesResponse::Conversations(
            list.into_iter()
                .map(|c| Conversation {
                    thread_id: c.thread,
                    addresses: c.addresses,
                    names: c.names,
                    snippet: c.snippet,
                    date_ms: c.date,
                    unread: c.unread,
                })
                .collect(),
        ));
    }
    if let Some(list) = answer.messages {
        return Ok(MessagesResponse::Messages(
            list.into_iter()
                .map(|m| TextMessage {
                    id: m.id,
                    mms: m.mms,
                    thread_id: m.thread,
                    address: m.address,
                    body: m.body,
                    date_ms: m.date,
                    kind: match m.kind.as_str() {
                        "inbox" => MessageKind::Inbox,
                        "sent" => MessageKind::Sent,
                        "outbox" => MessageKind::Outbox,
                        "failed" => MessageKind::Failed,
                        _ => MessageKind::Other,
                    },
                    read: m.read,
                    attachment: m.attachment,
                })
                .collect(),
        ));
    }
    if answer.sent {
        return Ok(MessagesResponse::Sent);
    }
    if answer.done {
        return Ok(MessagesResponse::Done);
    }
    Err("the phone answered with nothing".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_of_messages_from_the_phone_becomes_the_wire_vocabulary() {
        let answer = r#"{"messages":[{"id":5,"mms":true,"thread":2,"address":"+3805","body":"","date":1700000000000,"kind":"inbox","read":false,"attachment":true}]}"#;
        let Ok(MessagesResponse::Messages(list)) = parse_answer(answer) else {
            panic!("expected messages");
        };
        assert_eq!(list[0].kind, MessageKind::Inbox);
        assert!(list[0].attachment && list[0].mms);
    }

    #[test]
    fn the_phones_refusal_is_passed_on_in_its_words() {
        assert_eq!(
            parse_answer(r#"{"error":"allow Cursio to read and send SMS"}"#),
            Err("allow Cursio to read and send SMS".into())
        );
    }

    #[test]
    fn a_request_without_a_date_asks_from_the_newest() {
        let json = request_json(&MessagesRequest::ListConversations {
            limit: 30,
            before_ms: None,
        });
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["op"], "conversations");
        assert!(value["before"].is_null());
    }
}
