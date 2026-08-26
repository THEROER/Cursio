//! Making this machine able to share USB, once.
//!
//! Everything USB/IP needs is privileged: loading two kernel modules, and
//! writing the sysfs files that bind and attach devices. Asking for that
//! privilege *per device* is what the first version did, and it was wrong in
//! the way that matters — four devices meant four prompts, and a retry after a
//! refusal meant four more. Privilege is a property of the machine, not of a
//! device, so it is arranged once and then never mentioned again.
//!
//! ## What "once" actually means
//!
//! Three things, all of which survive a reboot:
//!
//! - `/etc/modules-load.d/cursio-usbip.conf` — the modules load at boot rather
//!   than being modprobed by whoever happens to need them first.
//! - `/etc/udev/rules.d/70-cursio-usbip.rules` — the bind and attach files
//!   become writable by a group the user is in, so Cursio itself never needs
//!   root again.
//! - A `modprobe` and a `udevadm trigger` now, so the machine is usable
//!   without a reboot.
//!
//! After that Cursio runs unprivileged forever. That is the whole point:
//! holding root for the life of a session to avoid a one-time prompt would be
//! a much larger thing to have made — and on this codebase it would also break
//! file sharing, since volumes mounted as root are invisible to the user's own
//! file manager.
//!
//! ## Why a script and not a series of calls
//!
//! `pkexec` raises exactly one authentication dialog per invocation. Four
//! privileged operations done as four calls are four dialogs; done as one
//! script they are one. The script is written to a temporary file rather than
//! passed as `sh -c` so the policy agent shows the user a path they can read
//! before agreeing to run it as root.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use crate::UsbBackendError;

/// Where the module list goes, so `usbip-host` and `vhci-hcd` load at boot.
const MODULES_CONF: &str = "/etc/modules-load.d/cursio-usbip.conf";

/// Where the permission rule goes.
///
/// The number orders it after the distribution's own rules, so nothing later
/// overwrites the mode this sets.
const UDEV_RULE: &str = "/etc/udev/rules.d/70-cursio-usbip.rules";

/// The group given write access to the USB/IP control files.
///
/// `plugdev` is the conventional "may touch removable hardware" group on
/// Debian and Ubuntu, and a desktop user is normally already in it — which is
/// what makes this a permission change rather than a group to join and a
/// session to restart.
const GROUP: &str = "plugdev";

/// What this machine still needs before it can share USB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readiness {
    /// `usbip-host` is loaded, so devices here can be offered.
    pub can_export: bool,
    /// `vhci-hcd` is loaded, so a peer's devices can appear here.
    pub can_attach: bool,
    /// The control files are writable without root.
    pub unprivileged: bool,
}

impl Readiness {
    /// Whether Cursio can do everything it needs as an ordinary user.
    pub fn is_complete(&self) -> bool {
        self.can_export && self.can_attach && self.unprivileged
    }

    /// A line naming what is missing, for a log the user will read.
    pub fn missing(&self) -> String {
        let mut parts = Vec::new();
        if !self.can_export {
            parts.push("the usbip-host module");
        }
        if !self.can_attach {
            parts.push("the vhci-hcd module");
        }
        if self.unprivileged {
            // Nothing to say: permissions are fine.
        } else if parts.is_empty() {
            parts.push("write access to the usb/ip control files");
        } else {
            parts.push("write access to their control files");
        }
        parts.join(" and ")
    }
}

/// What this machine can do right now.
pub fn readiness() -> Readiness {
    let export = std::path::Path::new("/sys/bus/usb/drivers/usbip-host/bind");
    let attach = std::path::Path::new("/sys/devices/platform/vhci_hcd.0/attach");
    Readiness {
        can_export: export.exists(),
        can_attach: attach.exists(),
        // Writability rather than mode bits: a rule may grant access through a
        // group, an ACL, or something a future distribution invents, and the
        // only question that matters is whether this process can write.
        unprivileged: is_writable(export) && is_writable(attach),
    }
}

/// Whether this process can write `path`, without writing anything.
fn is_writable(path: &std::path::Path) -> bool {
    std::fs::OpenOptions::new().write(true).open(path).is_ok()
}

/// Set this machine up to share USB, asking for privilege exactly once.
///
/// Does nothing and asks nothing when [`readiness`] already reports everything
/// in place, so calling this at every startup is free after the first.
///
/// Returns `Ok(false)` when the user declined — an ordinary answer, not an
/// error: USB sharing simply stays off, and the module is not advertised.
pub fn ensure_ready() -> Result<bool, UsbBackendError> {
    if readiness().is_complete() {
        return Ok(true);
    }
    if which("pkexec").is_none() {
        return Err(UsbBackendError::NeedsPrivilege(format!(
            "usb sharing needs {}, and pkexec is not installed to ask for it",
            readiness().missing()
        )));
    }

    let script = write_script()?;
    tracing::info!(
        missing = %readiness().missing(),
        "asking for privilege once to set up usb sharing"
    );

    let status = Command::new("pkexec")
        .arg("/bin/sh")
        .arg(&script)
        .status()
        .map_err(|e| UsbBackendError::NeedsPrivilege(format!("cannot run pkexec: {e}")));
    // Best-effort: a leftover script in the temp directory is harmless, and
    // failing the setup over it would be absurd.
    let _ = std::fs::remove_file(&script);
    let status = status?;

    if !status.success() {
        // Declined, or the policy refused. Either way this machine is not set
        // up, and the caller decides what that means.
        return Ok(false);
    }

    let now = readiness();
    if !now.is_complete() {
        return Err(UsbBackendError::NeedsPrivilege(format!(
            "setup ran but this machine still lacks {}",
            now.missing()
        )));
    }
    tracing::info!("usb sharing is set up; cursio will not need privilege again");
    Ok(true)
}

/// Write the setup script to a temporary file and return its path.
fn write_script() -> Result<PathBuf, UsbBackendError> {
    let path = std::env::temp_dir().join(format!("cursio-usb-setup-{}.sh", std::process::id()));
    let mut file = std::fs::File::create(&path)
        .map_err(|e| UsbBackendError::NeedsPrivilege(format!("cannot write the setup: {e}")))?;
    file.write_all(script_body().as_bytes())
        .map_err(|e| UsbBackendError::NeedsPrivilege(format!("cannot write the setup: {e}")))?;
    Ok(path)
}

/// Everything the one privileged run does.
///
/// `set -e` on purpose: a half-applied setup — modules loaded but no rule, or
/// a rule the trigger never applied — is worse than none, because
/// [`readiness`] would keep reporting the machine as not ready with no
/// indication which step failed.
fn script_body() -> String {
    format!(
        "set -e\n\
         # Load at boot, so nothing has to modprobe them later.\n\
         printf 'usbip-host\\nvhci-hcd\\n' > {MODULES_CONF}\n\
         # And now, so this machine is usable without a reboot.\n\
         modprobe usbip-host\n\
         modprobe vhci-hcd\n\
         # Let the {GROUP} group drive USB/IP, so Cursio never needs root again.\n\
         cat > {UDEV_RULE} <<'EOF'\n\
         # Written by Cursio. Removing this file makes USB sharing need root again.\n\
         ACTION==\"add|change\", SUBSYSTEM==\"usb\", DRIVER==\"usbip-host\", \
         RUN+=\"/bin/sh -c 'chgrp {GROUP} /sys/bus/usb/drivers/usbip-host/bind \
         /sys/bus/usb/drivers/usbip-host/unbind \
         /sys/bus/usb/drivers/usbip-host/match_busid 2>/dev/null; \
         chmod g+w /sys/bus/usb/drivers/usbip-host/bind \
         /sys/bus/usb/drivers/usbip-host/unbind \
         /sys/bus/usb/drivers/usbip-host/match_busid 2>/dev/null'\"\n\
         EOF\n\
         # The vhci files belong to a platform device that exists only once the\n\
         # module is loaded, so they are chmodded directly rather than by rule.\n\
         for f in attach detach; do\n\
         \x20 p=/sys/devices/platform/vhci_hcd.0/$f\n\
         \x20 [ -e \"$p\" ] && chgrp {GROUP} \"$p\" && chmod g+w \"$p\"\n\
         done\n\
         # Same for the exporting side, which the rule only covers on the next\n\
         # device event.\n\
         for f in bind unbind match_busid; do\n\
         \x20 p=/sys/bus/usb/drivers/usbip-host/$f\n\
         \x20 [ -e \"$p\" ] && chgrp {GROUP} \"$p\" && chmod g+w \"$p\"\n\
         done\n\
         udevadm control --reload-rules\n"
    )
}

/// Whether a program is on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message is what a user reads to know what to do, so it must name
    /// the actual gap rather than saying "not ready".
    #[test]
    fn the_missing_line_names_what_is_absent() {
        let nothing = Readiness {
            can_export: false,
            can_attach: false,
            unprivileged: false,
        };
        assert!(nothing.missing().contains("usbip-host"));
        assert!(nothing.missing().contains("vhci-hcd"));

        let modules_only = Readiness {
            can_export: true,
            can_attach: true,
            unprivileged: false,
        };
        assert!(
            modules_only.missing().contains("write access"),
            "got: {}",
            modules_only.missing()
        );
    }

    /// Every part must be present: a machine that can export but not attach is
    /// half-configured, and the whole point of asking once is not to come back.
    #[test]
    fn readiness_needs_every_part() {
        let complete = Readiness {
            can_export: true,
            can_attach: true,
            unprivileged: true,
        };
        assert!(complete.is_complete());
        assert!(complete.missing().is_empty());

        for missing in [
            Readiness {
                can_export: false,
                ..complete
            },
            Readiness {
                can_attach: false,
                ..complete
            },
            Readiness {
                unprivileged: false,
                ..complete
            },
        ] {
            assert!(!missing.is_complete(), "{missing:?} counted as ready");
        }
    }

    /// The script is run as root, so what it contains is worth asserting: a
    /// missing `set -e` turns a failed step into a machine that reports itself
    /// unready with no clue which part did not happen.
    #[test]
    fn the_script_stops_at_the_first_failure_and_persists_its_work() {
        let body = script_body();
        assert!(body.starts_with("set -e"));
        // Survives a reboot, which is what makes this a one-time prompt.
        assert!(body.contains(MODULES_CONF));
        assert!(body.contains(UDEV_RULE));
        // And works right now, without one.
        assert!(body.contains("modprobe usbip-host"));
        assert!(body.contains("modprobe vhci-hcd"));
    }

    /// Reading the machine must never panic, whatever this host is.
    #[test]
    fn reading_readiness_answers_without_panicking() {
        let r = readiness();
        // A machine with no modules loaded cannot be unprivileged-ready.
        if !r.can_export && !r.can_attach {
            assert!(!r.is_complete());
        }
    }
}
