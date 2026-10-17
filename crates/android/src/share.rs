//! Shares on a phone: acting on one that arrives, and sending one from the
//! system share sheet.
//!
//! Acting is the JVM's: writing the clipboard, keeping a file in Downloads
//! (MediaStore, since Android 10 gives apps no path there) and asking to open
//! a link (a notification — Android stops a background app from starting an
//! activity) are all Android APIs. So the sink calls up into
//! `CursioNative.onShare` (see [`crate::jvm`]) and reports what it answers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cursio_core::share_sink::ShareSink;
use cursio_protocol::share::{ShareAction, ShareContent, ShareOutcome};
use jni::objects::{JObject, JString, JValue};

pub struct AndroidSink;

impl AndroidSink {
    pub fn shared() -> Arc<dyn ShareSink> {
        Arc::new(AndroidSink)
    }
}

fn action_name(action: ShareAction) -> &'static str {
    match action {
        ShareAction::Open => "open",
        ShareAction::Copy => "copy",
        ShareAction::Save => "save",
    }
}

/// What Kotlin answered, as the sender is told it.
fn outcome(answer: &str) -> ShareOutcome {
    match answer.split_once(':') {
        Some(("saved", name)) => ShareOutcome::Saved { name: name.into() },
        Some(("failed", why)) => ShareOutcome::Failed(why.into()),
        Some(("refused", why)) => ShareOutcome::Refused(why.into()),
        _ => match answer {
            "opened" => ShareOutcome::Opened,
            "copied" => ShareOutcome::Copied,
            "offered" => ShareOutcome::Offered,
            other => ShareOutcome::Failed(format!("the phone answered {other:?}")),
        },
    }
}

impl ShareSink for AndroidSink {
    fn act(
        &self,
        from: &str,
        content: &ShareContent,
        file: Option<&Path>,
        action: ShareAction,
    ) -> ShareOutcome {
        let (kind, text, name, mime) = match content {
            ShareContent::Url(url) => ("url", url.as_str(), "", ""),
            ShareContent::Text(text) => ("text", text.as_str(), "", ""),
            ShareContent::File { name, mime, .. } => {
                ("file", "", name.as_str(), mime.as_deref().unwrap_or(""))
            }
        };
        let path = file
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let answer = crate::jvm::with_native(|env, class| {
            let args = [from, kind, text, &path, name, mime, action_name(action)]
                .iter()
                .map(|s| env.new_string(s).map(JObject::from))
                .collect::<jni::errors::Result<Vec<_>>>()?;
            let values: Vec<JValue> = args.iter().map(JValue::Object).collect();
            let result = env.call_static_method(
                class,
                "onShare",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &values,
            )?;
            let answer: JString = result.l()?.into();
            let text: String = env.get_string(&answer)?.into();
            Ok(text)
        });
        match answer {
            Ok(answer) => outcome(&answer),
            Err(e) => ShareOutcome::Failed(format!("the phone could not take it: {e}")),
        }
    }

    fn incoming_dir(&self) -> Option<PathBuf> {
        cursio_protocol::paths::data_dir().map(|d| d.join("share-incoming"))
    }
}

/// The machines a share can go to, as `[[id, name], …]` JSON for the share
/// sheet.
pub fn targets_json() -> String {
    let targets: Vec<(String, String)> = cursio_core::share_sink::outbox()
        .targets()
        .into_iter()
        .map(|(id, name)| (id.to_string(), name))
        .collect();
    serde_json::to_string(&targets).unwrap_or_else(|_| "[]".into())
}

/// Send what the share sheet handed over. `path` is set for a file (a private
/// copy the JVM made of it), otherwise `text` is sent.
pub fn send(
    peer: &str,
    text: &str,
    path: &str,
    name: &str,
    mime: &str,
    action: &str,
) -> Result<(), String> {
    let action = match action {
        "open" => ShareAction::Open,
        "copy" => ShareAction::Copy,
        "save" => ShareAction::Save,
        other => return Err(format!("unknown action {other:?}")),
    };
    if path.is_empty() {
        cursio_core::share_sink::share_text(peer, text, action)
    } else {
        let mime = (!mime.is_empty()).then(|| mime.to_string());
        cursio_core::share_sink::share_file(peer, Path::new(path), name.to_string(), mime, action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phones_answers_become_outcomes() {
        assert_eq!(outcome("opened"), ShareOutcome::Opened);
        assert_eq!(
            outcome("saved:photo (1).jpg"),
            ShareOutcome::Saved {
                name: "photo (1).jpg".into()
            }
        );
        assert_eq!(
            outcome("failed:no space"),
            ShareOutcome::Failed("no space".into())
        );
        assert!(matches!(outcome("what"), ShareOutcome::Failed(_)));
    }
}
