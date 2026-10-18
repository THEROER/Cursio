//! This phone's calls, reported to computers and acted on at their request.
//!
//! `CallMonitor` (Kotlin) watches the phone's call state and passes the whole
//! picture down through `nativeCallsChanged`, as JSON; it lands in the mesh's
//! [`hub`](cursio_core::calls_hub::hub). A computer's request comes back up
//! as `CursioNative.onCallRequest`, which answers with why it failed, if it
//! did.

use std::sync::Arc;

use cursio_core::calls_hub::CallActor;
use cursio_protocol::calls::{Call, CallDirection, CallState, CallsMsg};
use jni::objects::{JObject, JString, JValue};
use serde::Deserialize;

pub struct AndroidCallActor;

impl AndroidCallActor {
    pub fn shared() -> Arc<dyn CallActor> {
        Arc::new(AndroidCallActor)
    }
}

impl CallActor for AndroidCallActor {
    fn act(&self, msg: &CallsMsg) -> Result<(), String> {
        let (kind, number, muted) = match msg {
            CallsMsg::Answer { .. } => ("answer", None, false),
            CallsMsg::Decline { .. } => ("decline", None, false),
            CallsMsg::HangUp { .. } => ("hangup", None, false),
            CallsMsg::Dial { number } => ("dial", Some(number.as_str()), false),
            CallsMsg::Mute { muted } => ("mute", None, *muted),
            other => return Err(format!("a phone does not take {other:?}")),
        };
        let answer = crate::jvm::with_native(|env, class| {
            let kind = JObject::from(env.new_string(kind)?);
            let number = match number {
                Some(n) => JObject::from(env.new_string(n)?),
                None => JObject::null(),
            };
            let result = env.call_static_method(
                class,
                "onCallRequest",
                "(Ljava/lang/String;Ljava/lang/String;Z)Ljava/lang/String;",
                &[
                    JValue::Object(&kind),
                    JValue::Object(&number),
                    JValue::Bool(u8::from(muted)),
                ],
            )?;
            let refusal = result.l()?;
            if refusal.is_null() {
                return Ok(None);
            }
            let refusal: JString = refusal.into();
            let text: String = env.get_string(&refusal)?.into();
            Ok(Some(text))
        })?;
        match answer {
            None => Ok(()),
            Some(reason) => Err(reason),
        }
    }
}

#[derive(Deserialize)]
struct Reported {
    id: u32,
    incoming: bool,
    state: String,
    number: Option<String>,
    name: Option<String>,
}

/// The monitor's JSON as the mesh carries it. A state the monitor does not
/// name is left out rather than guessed.
pub fn calls(json: &str) -> Result<Vec<Call>, serde_json::Error> {
    let reported: Vec<Reported> = serde_json::from_str(json)?;
    Ok(reported
        .into_iter()
        .filter_map(|r| {
            let state = match r.state.as_str() {
                "ringing" => CallState::Ringing,
                "waiting" => CallState::Waiting,
                "dialing" => CallState::Dialing,
                "alerting" => CallState::Alerting,
                "active" => CallState::Active,
                "held" => CallState::Held,
                other => {
                    tracing::warn!(
                        state = other,
                        "a call state the phone reported is not known"
                    );
                    return None;
                }
            };
            Some(Call {
                id: r.id,
                direction: if r.incoming {
                    CallDirection::Incoming
                } else {
                    CallDirection::Outgoing
                },
                state,
                number: r.number.filter(|n| !n.is_empty()),
                name: r.name.filter(|n| !n.is_empty()),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_monitors_report_becomes_the_call_list() {
        let json = r#"[{"id":1,"incoming":true,"state":"active","number":"+380501234567","name":"Mom"},
                       {"id":2,"incoming":true,"state":"waiting","number":null,"name":null}]"#;
        let calls = calls(json).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].state, CallState::Active);
        assert_eq!(calls[0].who(), "Mom");
        assert_eq!(calls[1].who(), "Unknown caller");
    }

    #[test]
    fn an_unknown_state_is_left_out() {
        let json = r#"[{"id":1,"incoming":false,"state":"teleporting","number":"1","name":null}]"#;
        assert!(calls(json).unwrap().is_empty());
    }
}
