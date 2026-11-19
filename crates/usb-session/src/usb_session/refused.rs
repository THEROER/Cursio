//! What a peer is told when this machine cannot give it a device, in the
//! same words whichever kernel refused.

use cursio_protocol::usb::UsbError;

/// Turn a local failure into what a peer is told.
///
/// The mapping is deliberately not "everything becomes a string": the peer
/// shows the user a different thing for each of these. A missing module is the
/// host's to fix and is worth naming; a privilege failure is a prompt on the
/// host, not a retry here; a device that is gone means stop asking.
pub(super) fn backend_error_to_wire(error: &cursio_usb::UsbBackendError) -> UsbError {
    use cursio_usb::UsbBackendError;
    match error {
        UsbBackendError::NoSuchDevice(_) => UsbError::Gone,
        UsbBackendError::DriverMissing(what) => UsbError::DriverMissing(what.clone()),
        UsbBackendError::NeedsPrivilege(_) => UsbError::NeedsPrivilege,
        other => UsbError::Backend(other.to_string()),
    }
}

/// What the peer is told when an export is refused, and what this machine
/// reports about it.
///
/// A hold by a local program is the one refusal the *host's* user has to act
/// on — the peer can only wait — so it becomes a problem here as well as the
/// reason the peer reads. Both carry the same sentence: the peer used to be
/// told `no such device` about a phone plugged in the whole time.
pub(super) fn refusal(
    error: &cursio_usb::UsbBackendError,
    busid: &str,
    label: Option<&str>,
) -> UsbError {
    let cursio_usb::UsbBackendError::HeldLocally { holder, .. } = error else {
        return backend_error_to_wire(error);
    };
    let summary = cursio_usb::holder::held_sentence(label.unwrap_or(busid), holder.as_ref());
    let hint = cursio_usb::holder::held_hint(holder.as_ref());
    crate::diagnostics::raise(crate::diagnostics::Problem {
        id: held_problem_id(busid),
        severity: crate::diagnostics::Severity::Error,
        summary: format!("{summary}, so a peer could not attach it"),
        hint: Some(hint.clone()),
        actions: Vec::new(),
        since: std::time::SystemTime::now(),
    });
    UsbError::Backend(format!("{summary}; on that machine, {hint}"))
}

pub(super) fn held_problem_id(busid: &str) -> String {
    format!("usb.held.{busid}")
}
