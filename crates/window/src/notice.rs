//! Conditions the window backends notice that the user has to act on.
//!
//! A portal dialog the user cancelled and a GNOME Shell running an older
//! extension are both found deep inside a capture backend, on a thread the
//! session never sees, and both outlast the log line that reports them: the
//! refusal stays until the user approves, and the old extension until they log
//! in again. This crate sits below `cursio-core`, which keeps the machine's
//! problem list, so the backends report here and the core installs the
//! reporter — the same shape the files crate uses for disk walks.

use std::sync::OnceLock;

/// Something a window backend found that stays true until the user acts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The Shell extension is there but lacks a method this build calls.
    ShellExtensionOutdated { missing: String },
    /// The Shell extension did not answer at all.
    ShellExtensionMissing { detail: String },
    /// The Shell extension answered a call this build needs.
    ShellExtensionCurrent,
    /// The screen-share dialog was cancelled or refused.
    ScreenCastRefused { detail: String },
    /// A screen-share dialog was approved.
    ScreenCastGranted,
}

static REPORTER: OnceLock<fn(&Notice)> = OnceLock::new();

/// Where notices go. The first call wins: one process, one problem list.
pub fn set_reporter(reporter: fn(&Notice)) {
    let _ = REPORTER.set(reporter);
}

/// Hand a notice to whoever installed a reporter; nothing when nobody has.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn report(notice: Notice) {
    if let Some(reporter) = REPORTER.get() {
        reporter(&notice);
    }
}
