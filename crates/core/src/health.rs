//! The problems the mesh and its modules already notice, raised by name.
//!
//! Each of these used to be a single `warn!` in the middle of a log: a link
//! that dropped, a module whose turn took a second, a Windows machine without
//! the virtual audio driver, a GNOME Shell still running last week's extension,
//! a portal the user said no to. They are facts about the machine that stay
//! true after the line scrolls away, so each is a [`diagnostics`] problem with
//! a fixed id, raised where it is noticed and cleared where it stops being
//! true.
//!
//! One function per condition rather than `raise` at every call site, so the
//! sentence, the severity and the id live in one place and a caller cannot get
//! the id subtly wrong and leave a problem that is never cleared.
//!
//! ## For callers elsewhere
//!
//! [`module_stalled`] and [`module_recovered`] exist for the loop that times
//! module turns (`module_host.rs`): it already measures every `pump` and
//! `handle_message`, and is the one place that can say a module has stopped
//! keeping up — and that it has caught up again.

use std::time::{Duration, SystemTime};

use cursio_protocol::PeerId;

use crate::diagnostics::{self, Problem, Severity};

fn raise(id: String, severity: Severity, summary: String, hint: Option<String>) {
    diagnostics::raise(Problem {
        id,
        severity,
        summary,
        hint,
        since: SystemTime::now(),
    });
}

fn link_id(peer: PeerId) -> String {
    format!("peer.{peer}.down")
}

/// A session with `peer` ended with an error rather than a goodbye.
///
/// A peer that said goodbye is not a problem; one that vanished is, and stays
/// one until it is back — a machine that went to sleep is worth seeing in the
/// list, with the reason the link gave.
pub fn link_dropped(peer: PeerId, name: &str, reason: &str) {
    raise(
        link_id(peer),
        Severity::Warning,
        format!("the link to {name} dropped: {reason}"),
        Some(format!(
            "cursio redials it on its own; if {name} is awake and on this network, check that \
             cursio is running there"
        )),
    );
}

/// `peer` greeted this machine again.
pub fn link_restored(peer: PeerId) {
    diagnostics::clear(&link_id(peer));
}

fn module_id(module: &str, peer: &str) -> String {
    format!("module.{module}.{peer}.stalled")
}

/// A module's turn took `took` on its link to `peer`.
///
/// Its own traffic waited that long; input did not, since each module runs on
/// a thread of its own — which is why this is a warning about that module and
/// not about the link.
pub fn module_stalled(module: &str, peer: &str, took: Duration) {
    raise(
        module_id(module, peer),
        Severity::Warning,
        format!(
            "the {module} module is not keeping up with {peer}: one turn took {} ms",
            took.as_millis()
        ),
        Some(format!(
            "{module} traffic with {peer} is delayed while this lasts; if it does not pass, \
             the log around it names what the module was waiting on"
        )),
    );
}

/// A module that had stalled is keeping up again.
pub fn module_recovered(module: &str, peer: &str) {
    diagnostics::clear(&module_id(module, peer));
}

/// This machine cannot publish a peer's audio devices.
///
/// `hint` is what to install, which only the audio module knows per platform.
pub fn audio_devices_unavailable(hint: &str, installable: bool) {
    raise(
        "audio.virtual-devices".into(),
        match installable {
            true => Severity::Warning,
            false => Severity::Info,
        },
        "peers' audio outputs and microphones cannot appear on this machine; application \
         audio still plays"
            .into(),
        Some(hint.to_string()),
    );
}

/// The GNOME Shell extension answered, but is older than this Cursio.
pub fn shell_extension_outdated(what_is_missing: &str) {
    raise(
        "window.shell-extension".into(),
        Severity::Warning,
        format!("the running GNOME Shell has an older Cursio extension ({what_is_missing})"),
        Some(
            "log out and back in so GNOME Shell loads the installed extension; until then a \
             fullscreen app on a shared screen freezes the peer's view"
                .into(),
        ),
    );
}

/// The GNOME Shell extension did not answer at all.
pub fn shell_extension_missing(detail: &str) {
    raise(
        "window.shell-extension".into(),
        Severity::Warning,
        format!("the Cursio GNOME Shell extension is not reachable: {detail}"),
        Some(
            "install and enable native/cursio-shell-extension (`gnome-extensions enable \
             cursio@cursio.dev`), then log out and back in"
                .into(),
        ),
    );
}

/// The GNOME Shell extension answered a call this build needs.
pub fn shell_extension_current() {
    diagnostics::clear("window.shell-extension");
}

/// A portal request was refused or cancelled.
///
/// `portal` names it the way the user saw the dialog: "screen sharing",
/// "input capture".
pub fn portal_refused(portal: &str, detail: &str) {
    raise(
        format!("portal.{}", portal.replace(' ', "-")),
        Severity::Error,
        format!("the {portal} permission was not granted: {detail}"),
        Some(format!(
            "approve the {portal} dialog when it appears (it can open behind other windows); \
             a refusal is remembered until you reset it in the desktop's privacy settings"
        )),
    );
}

/// The portal that was refused has now been granted.
pub fn portal_granted(portal: &str) {
    diagnostics::clear(&format!("portal.{}", portal.replace(' ', "-")));
}

/// Local input could not be captured at all.
pub fn input_capture_unavailable(detail: &str) {
    let hint = if cfg!(target_os = "macos") {
        "grant Accessibility to the app that started cursio (System Settings → Privacy & \
         Security → Accessibility), then restart it"
    } else if cfg!(target_os = "linux") {
        "on Wayland approve the input capture dialog; elsewhere add yourself to the `input` \
         group (`sudo usermod -aG input $USER`) and log in again"
    } else {
        "restart cursio; if it persists, the log line before this names the failing call"
    };
    raise(
        "input.capture".into(),
        Severity::Error,
        format!("this machine's keyboard and mouse cannot be shared: {detail}"),
        Some(hint.into()),
    );
}

/// Local input capture is running.
pub fn input_capture_started() {
    diagnostics::clear("input.capture");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(id: &str) -> Option<Problem> {
        diagnostics::snapshot().into_iter().find(|p| p.id == id)
    }

    #[test]
    fn a_dropped_link_stays_raised_until_the_peer_is_back() {
        let peer: PeerId = uuid::Uuid::new_v4();
        link_dropped(peer, "Lightmoon", "peer timed out");
        let raised = find(&link_id(peer)).expect("raised");
        assert_eq!(
            raised.summary,
            "the link to Lightmoon dropped: peer timed out"
        );

        link_restored(peer);
        assert!(find(&link_id(peer)).is_none());
    }

    #[test]
    fn a_stalled_module_names_itself_the_peer_and_the_time() {
        module_stalled("t-files", "Lightmoon", Duration::from_millis(1234));
        let raised = find("module.t-files.Lightmoon.stalled").expect("raised");
        assert!(raised.summary.contains("1234 ms"), "{}", raised.summary);
        module_recovered("t-files", "Lightmoon");
        assert!(find("module.t-files.Lightmoon.stalled").is_none());
    }

    /// A refusal and the later grant must agree on the id, or the problem
    /// outlives the permission.
    #[test]
    fn a_granted_portal_clears_its_refusal() {
        portal_refused("t screen sharing", "cancelled");
        assert!(find("portal.t-screen-sharing").is_some());
        portal_granted("t screen sharing");
        assert!(find("portal.t-screen-sharing").is_none());
    }
}
