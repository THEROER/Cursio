//! Showing a phone's notifications as Windows toasts, so they stack in the
//! Action Center like every other notification on the machine.
//!
//! A toast belongs to an AppUserModelID. A packaged app has one from its
//! manifest and an installer usually stamps one on a Start-menu shortcut; this
//! node has neither, so it registers its own under
//! `HKCU\Software\Classes\AppUserModelId`, the documented way for an unpackaged
//! program, and names it "Cursio" with the logo. Without that the system shows
//! nothing and says nothing.
//!
//! Unlike the freedesktop server on Linux, a toast carries a text field, so a
//! reply is typed into the toast itself instead of a dialog of its own.
//!
//! WinRT wants a COM apartment on the calling thread, and `show`/`close` run
//! on a module's host thread inside the runtime. Every call therefore goes to
//! one thread that joined the multithreaded apartment, as the Linux display
//! does with the session bus.
//!
//! Duplicates of what a desktop client already showed are found the way Linux
//! finds them (see [`crate::notify_dedup`]), from `UserNotificationListener`.
//! That listener asks the user's consent and may refuse a program without
//! package identity outright; refused, copies are shown at once, as before
//! deduplication existed, and the reason is said once.

// The XML and the argument parsing are tested on every platform, used on one.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use cursio_protocol::notify::{Notification, NotifyKind};
use cursio_protocol::PeerId;

/// The identity toasts are shown under.
const AUMID: &str = "dev.cursio.Cursio";

/// Every toast of ours, so one group can be told apart from any other app's.
const GROUP: &str = "cursio";

/// The toast's text field, named by its reply buttons.
const REPLY_INPUT: &str = "reply";

/// Windows shows at most five buttons; the rest would make the toast invalid.
const MAX_ACTIONS: usize = 5;

/// A toast's tag. Windows caps it at 64 characters and a phone's key is
/// longer, so the pair is hashed; the same notification keeps the same tag,
/// which is how an update replaces it rather than stacking a second one.
fn tag(peer: PeerId, key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    peer.hash(&mut hasher);
    key.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Control characters other than line breaks make the document
            // invalid, and the toast is dropped whole.
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// The toast's XML. `icon` is a `file:///` URI of the app's icon, if any.
fn toast_xml(n: &Notification, peer_name: &str, icon: Option<&str>) -> String {
    let title = if n.title.is_empty() {
        &n.app_name
    } else {
        &n.title
    };
    // A call stays until the phone withdraws it, like an urgent notification
    // on Linux; anything else follows the system's own timing.
    let scenario = if n.kind == NotifyKind::Call {
        r#" scenario="incomingCall""#
    } else {
        ""
    };
    let mut xml = format!(r#"<toast{scenario}><visual><binding template="ToastGeneric">"#);
    xml.push_str(&format!("<text>{}</text>", escape(title)));
    if !n.text.is_empty() {
        xml.push_str(&format!("<text>{}</text>", escape(&n.text)));
    }
    if let Some(icon) = icon {
        xml.push_str(&format!(
            r#"<image placement="appLogoOverride" hint-crop="circle" src="{}"/>"#,
            escape(icon)
        ));
    }
    xml.push_str(&format!(
        r#"<text placement="attribution">{} · {}</text>"#,
        escape(&n.app_name),
        escape(peer_name)
    ));
    xml.push_str("</binding></visual>");
    let actions: Vec<_> = n.actions.iter().take(MAX_ACTIONS).enumerate().collect();
    if !actions.is_empty() {
        xml.push_str("<actions>");
        if actions.iter().any(|(_, a)| a.reply) {
            xml.push_str(&format!(
                r#"<input id="{REPLY_INPUT}" type="text" placeHolderContent="Reply"/>"#
            ));
        }
        for (i, action) in actions {
            let input = if action.reply {
                format!(r#" hint-inputId="{REPLY_INPUT}""#)
            } else {
                String::new()
            };
            xml.push_str(&format!(
                r#"<action content="{}" arguments="action={i}"{input}/>"#,
                escape(&action.title)
            ));
        }
        xml.push_str("</actions>");
    }
    xml.push_str("</toast>");
    xml
}

/// Which button an activation's arguments name. A click on the toast's body
/// carries none, and does nothing on the phone, as on Linux.
fn action_from(arguments: &str) -> Option<u8> {
    arguments.strip_prefix("action=")?.parse().ok()
}

#[cfg(target_os = "windows")]
mod platform;
#[cfg(target_os = "windows")]
pub use platform::ToastDisplay;

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::notify::NotifyAction;

    fn note(kind: NotifyKind, actions: Vec<NotifyAction>) -> Notification {
        Notification {
            key: "0|org.telegram.messenger|7|null|10324".into(),
            app_id: "org.telegram.messenger".into(),
            app_name: "Telegram".into(),
            title: "Alice <work>".into(),
            text: "see you & bye".into(),
            icon: None,
            when_ms: 0,
            kind,
            actions,
        }
    }

    fn action(title: &str, reply: bool) -> NotifyAction {
        NotifyAction {
            title: title.into(),
            reply,
        }
    }

    #[test]
    fn escapes_what_the_phone_wrote_into_the_toast() {
        let xml = toast_xml(&note(NotifyKind::Message, vec![]), "POCO", None);
        assert!(xml.contains("<text>Alice &lt;work&gt;</text>"));
        assert!(xml.contains("<text>see you &amp; bye</text>"));
        assert!(xml.contains("Telegram · POCO"));
    }

    #[test]
    fn a_reply_button_points_at_the_one_text_field() {
        let xml = toast_xml(
            &note(
                NotifyKind::Message,
                vec![action("Reply", true), action("Mark as read", false)],
            ),
            "POCO",
            None,
        );
        assert_eq!(xml.matches("<input ").count(), 1);
        assert!(xml.contains(r#"arguments="action=0" hint-inputId="reply""#));
        assert!(xml.contains(r#"<action content="Mark as read" arguments="action=1"/>"#));
    }

    #[test]
    fn a_toast_without_a_reply_has_no_text_field() {
        let xml = toast_xml(
            &note(NotifyKind::Other, vec![action("Open", false)]),
            "POCO",
            None,
        );
        assert!(!xml.contains("<input"));
    }

    #[test]
    fn keeps_at_most_the_five_buttons_windows_shows() {
        let many = (0..8).map(|i| action(&format!("b{i}"), false)).collect();
        let xml = toast_xml(&note(NotifyKind::Other, many), "POCO", None);
        assert_eq!(xml.matches("<action ").count(), 5);
    }

    #[test]
    fn a_call_stays_up_as_an_incoming_call() {
        let xml = toast_xml(&note(NotifyKind::Call, vec![]), "POCO", None);
        assert!(xml.starts_with(r#"<toast scenario="incomingCall">"#));
    }

    #[test]
    fn drops_control_characters_that_would_void_the_toast() {
        assert_eq!(escape("a\u{1}b\nc"), "ab\nc");
    }

    #[test]
    fn reads_the_button_from_an_activation_and_ignores_a_body_click() {
        assert_eq!(action_from("action=3"), Some(3));
        assert_eq!(action_from(""), None);
        assert_eq!(action_from("action=x"), None);
    }

    #[test]
    fn the_same_notification_keeps_one_short_tag() {
        let peer = PeerId::from_bytes([7; 16]);
        let t = tag(
            peer,
            "a very long key from the phone that is well over sixty-four characters long",
        );
        assert_eq!(t.len(), 16);
        assert_eq!(
            t,
            tag(
                peer,
                "a very long key from the phone that is well over sixty-four characters long"
            )
        );
        assert_ne!(t, tag(peer, "another"));
    }
}
