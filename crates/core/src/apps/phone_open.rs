//! Opening an application on a phone, asked from another machine
//! (`cursio apps launch <phone> <app>`).
//!
//! A computer starts an installed build's file; a phone cannot — an app there
//! is a package the system starts — so the phone opens the package's own
//! launcher screen instead. Which package is never taken from the request:
//! it is the one the phone's person allowed to talk to Cursio as that
//! application ([`super::access`]), so a peer can open only what the person
//! already tied to that id.
//!
//! Android lets an app start another's screen from the background only in a
//! few cases; one is an accessibility service the system has bound, which
//! Cursio's touch service is when it is on. Without it the phone shows a
//! notification whose tap opens the app, and the answer says so rather than
//! claiming it opened. A phone cannot end another app either, so `restart`
//! only opens.

use std::sync::Arc;

use cursio_app_protocol::AppId;

use crate::config::PackageGrant;

/// What the phone did to open a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// Its screen was started.
    Direct { version_name: Option<String> },
    /// Its person was asked to open it, by a notification.
    Asked { version_name: Option<String> },
}

/// What opens a package on this phone: Android's package manager, through
/// `crates/android`. `Err` says why it cannot be opened.
pub type AppOpener = Arc<dyn Fn(&str) -> Result<Opened, String> + Send + Sync>;

/// The package this phone's person allowed to be `app`, if any.
pub fn granted_package<'a>(grants: &'a [PackageGrant], app: &AppId) -> Option<&'a str> {
    grants
        .iter()
        .find(|g| g.allowed && g.app == app.as_str())
        .map(|g| g.package.as_str())
}

/// Open `app` on this phone, answering what happened. `wanted` is the
/// version the asker expects; `restart` is answered, not done.
pub fn open(
    grants: &[PackageGrant],
    app: &AppId,
    wanted: Option<&str>,
    restart: bool,
    opener: &dyn Fn(&str) -> Result<Opened, String>,
) -> Result<String, String> {
    let package = granted_package(grants, app).ok_or_else(|| {
        format!("no app on this phone has been allowed to be {app}; open it once and allow it in Cursio")
    })?;
    let (mut answer, version_name) = match opener(package)? {
        // MIUI/HyperOS drop a background start without an error unless Cursio
        // may "display pop-up windows while running in the background"; the
        // phone cannot tell, so the answer says where to look.
        Opened::Direct { version_name } => (
            format!(
                "opened {app} ({package}) on this phone (if it did not appear: on MIUI/HyperOS, allow Cursio to display pop-up windows while running in the background)"
            ),
            version_name,
        ),
        Opened::Asked { version_name } => (
            format!(
                "asked this phone's person to open {app} ({package}); turn on Cursio's accessibility service on the phone to open apps directly"
            ),
            version_name,
        ),
    };
    if let (Some(wanted), Some(installed)) = (wanted, version_name.as_deref()) {
        if installed != wanted {
            answer.push_str(&format!(
                "; the installed copy is {installed}, so {wanted} may still be waiting at the phone's installer"
            ));
        }
    }
    if restart {
        answer.push_str(
            "; nothing was restarted: an app on a phone cannot end another, so a copy already open stays as it is",
        );
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(package: &str, app: &str, allowed: bool) -> PackageGrant {
        PackageGrant {
            package: package.into(),
            app: app.into(),
            allowed,
        }
    }

    fn leavepulse() -> AppId {
        AppId::parse("leavepulse").unwrap()
    }

    #[test]
    fn only_the_package_allowed_as_the_application_is_opened() {
        let grants = [
            grant("evil.app", "leavepulse", false),
            grant("dev.leavepulse.launcher", "leavepulse", true),
        ];
        let asked = std::sync::Mutex::new(None);
        let answer = open(&grants, &leavepulse(), None, false, &|pkg| {
            *asked.lock().unwrap() = Some(pkg.to_owned());
            Ok(Opened::Direct { version_name: None })
        });
        assert_eq!(
            asked.lock().unwrap().as_deref(),
            Some("dev.leavepulse.launcher")
        );
        assert!(answer
            .unwrap()
            .starts_with("opened leavepulse (dev.leavepulse.launcher)"));
    }

    #[test]
    fn an_application_no_package_was_allowed_as_is_not_opened() {
        let grants = [grant("dev.leavepulse.launcher", "leavepulse", false)];
        let error = open(&grants, &leavepulse(), None, false, &|_| {
            panic!("nothing may be opened")
        })
        .unwrap_err();
        assert_eq!(
            error,
            "no app on this phone has been allowed to be leavepulse; open it once and allow it in Cursio"
        );
    }

    #[test]
    fn without_the_accessibility_service_the_person_is_asked_and_told_why() {
        let grants = [grant("dev.leavepulse.launcher", "leavepulse", true)];
        let answer = open(&grants, &leavepulse(), None, false, &|_| {
            Ok(Opened::Asked { version_name: None })
        })
        .unwrap();
        assert!(answer.starts_with("asked this phone's person to open leavepulse"));
        assert!(answer.contains("accessibility service"));
    }

    #[test]
    fn a_version_other_than_the_one_asked_for_and_a_restart_are_both_said() {
        let grants = [grant("dev.leavepulse.launcher", "leavepulse", true)];
        let answer = open(&grants, &leavepulse(), Some("0.6.1"), true, &|_| {
            Ok(Opened::Direct {
                version_name: Some("0.6.0".into()),
            })
        })
        .unwrap();
        assert!(answer.contains("the installed copy is 0.6.0, so 0.6.1 may still be waiting"));
        assert!(answer.contains("nothing was restarted"));
    }

    #[test]
    fn why_the_phone_cannot_open_it_is_passed_on() {
        let grants = [grant("dev.leavepulse.launcher", "leavepulse", true)];
        let error = open(&grants, &leavepulse(), None, false, &|_| {
            Err("dev.leavepulse.launcher is not installed on this phone".into())
        })
        .unwrap_err();
        assert_eq!(
            error,
            "dev.leavepulse.launcher is not installed on this phone"
        );
    }
}
