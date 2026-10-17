//! Shares on a phone: acting on one that arrives, and sending one from the
//! system share sheet.
//!
//! Acting is the JVM's: writing the clipboard, keeping a file in Downloads
//! (MediaStore, since Android 10 gives apps no path there) and asking to open
//! a link (a notification — Android stops a background app from starting an
//! activity) are all Android APIs. So the sink calls up into
//! `CursioNative.onShare` and reports what it answers.
//!
//! Calling up needs the JVM and the class, both taken in `nativeInit`: on a
//! thread the JVM did not start, `FindClass` searches the system class loader,
//! which has never heard of this app's classes.
//!
//! The class is looked up by name rather than taken from `nativeInit`'s
//! second argument: `CursioNative` is a Kotlin `object`, so its non-`@JvmStatic`
//! natives receive the `INSTANCE`, not the class. Handing that instance to
//! `GetMethodID` as a `jclass` is a segfault on the first share received.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use cursio_core::share_sink::ShareSink;
use cursio_protocol::share::{ShareAction, ShareContent, ShareOutcome};
use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::{JNIEnv, JavaVM};

static JVM: OnceLock<(JavaVM, GlobalRef)> = OnceLock::new();

const NATIVE_CLASS: &str = "dev/cursio/mesh/CursioNative";

/// Keep the JVM and `CursioNative`'s class for calls from the node's threads.
/// Must run on a thread inside a native call from the app, whose class loader
/// is the one `FindClass` then uses.
pub(crate) fn remember(env: &mut JNIEnv) -> jni::errors::Result<()> {
    if JVM.get().is_none() {
        let vm = env.get_java_vm()?;
        let class = env.find_class(NATIVE_CLASS)?;
        let class = env.new_global_ref(class)?;
        let _ = JVM.set((vm, class));
    }
    Ok(())
}

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
        let Some((vm, class)) = JVM.get() else {
            return ShareOutcome::Failed("the phone's app is not initialised".into());
        };
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
        let answer = (|| -> jni::errors::Result<String> {
            let mut env = vm.attach_current_thread_as_daemon()?;
            let args = [from, kind, text, &path, name, mime, action_name(action)]
                .iter()
                .map(|s| env.new_string(s).map(JObject::from))
                .collect::<jni::errors::Result<Vec<_>>>()?;
            let values: Vec<JValue> = args.iter().map(JValue::Object).collect();
            let class: &JClass = class.as_obj().into();
            let result = env.call_static_method(
                class,
                "onShare",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &values,
            )?;
            let answer: JString = result.l()?.into();
            let text: String = env.get_string(&answer)?.into();
            Ok(text)
        })();
        match answer {
            Ok(answer) => outcome(&answer),
            Err(jni::errors::Error::JavaException) => ShareOutcome::Failed(format!(
                "the phone could not take it: {}",
                take_exception(vm)
            )),
            Err(e) => ShareOutcome::Failed(format!("the phone could not take it: {e}")),
        }
    }

    fn incoming_dir(&self) -> Option<PathBuf> {
        cursio_protocol::paths::data_dir().map(|d| d.join("share-incoming"))
    }
}

/// Clear the pending exception and say what it was. Left pending, it makes the
/// next JNI call on this thread — the module's next share — abort the process.
fn take_exception(vm: &JavaVM) -> String {
    let Ok(mut env) = vm.attach_current_thread_as_daemon() else {
        return "a Java exception".into();
    };
    let Ok(thrown) = env.exception_occurred() else {
        return "a Java exception".into();
    };
    let _ = env.exception_clear();
    let described = env
        .call_method(&thrown, "toString", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .and_then(|s| Ok(String::from(env.get_string(&JString::from(s))?)));
    let _ = env.exception_clear();
    described.unwrap_or_else(|_| "a Java exception".into())
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
