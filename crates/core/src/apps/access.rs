//! Who may reach the node's apps socket on a phone.
//!
//! On a computer the socket lives in a directory only its user can enter,
//! so whoever connects is that user's program. On a phone the node lives in
//! Cursio's own app, whose private directory no other app can reach, so the
//! socket is abstract — and any app on the phone can connect to an abstract
//! socket. The node therefore asks Android who connected: the peer's uid
//! (`SO_PEERCRED`, which the kernel fills and the app cannot forge), and the
//! packages that run as it. A package is taken only once the phone's person
//! allowed it, and only as the application id it was allowed as: otherwise
//! any app could register as `leavepulse` and read its messages and calls.

use std::path::PathBuf;
use std::sync::Arc;

use cursio_app_protocol::AppId;

use crate::config::{Config, ConfigEdit, PackageGrant};
use crate::diagnostics::{self, Problem, ProblemAction, Severity};

/// One package running as the uid that connected: its name and the label
/// Android shows for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub label: String,
}

/// What to do with a hello from `packages` claiming `app`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Refuse(String),
    /// Nobody has answered for this package and application yet.
    Ask(Package),
}

/// Decide from the grants the person made.
pub fn decide(grants: &[PackageGrant], packages: &[Package], app: &AppId) -> Decision {
    let Some(first) = packages.first() else {
        return Decision::Refuse(
            "the phone cannot tell which app is connecting, so it takes none".into(),
        );
    };
    let ours = |g: &&PackageGrant| packages.iter().any(|p| p.name == g.package);
    if grants
        .iter()
        .filter(ours)
        .any(|g| g.allowed && g.app == app.as_str())
    {
        return Decision::Allow;
    }
    if let Some(owner) = grants
        .iter()
        .find(|g| g.allowed && g.app == app.as_str() && !ours(g))
    {
        return Decision::Refuse(format!(
            "{app} belongs to {} on this phone; another app cannot take its name",
            owner.package
        ));
    }
    if let Some(other) = grants.iter().filter(ours).find(|g| g.allowed) {
        return Decision::Refuse(format!(
            "{} talks to Cursio as {}; it cannot also be {app}",
            other.package, other.app
        ));
    }
    if grants
        .iter()
        .filter(ours)
        .any(|g| !g.allowed && g.app == app.as_str())
    {
        return Decision::Refuse(format!(
            "this phone's person did not let {} talk to Cursio",
            first.label
        ));
    }
    Decision::Ask(first.clone())
}

fn problem_id(package: &str) -> String {
    format!("android.package.{package}")
}

/// Raise the question for `package` claiming `app`, with both answers.
fn ask(package: &Package, app: &AppId) {
    let answer = |label: String, allowed| ProblemAction {
        label,
        edit: ConfigEdit::PackageAccess {
            package: package.name.clone(),
            app: app.as_str().to_owned(),
            allowed,
        },
    };
    diagnostics::raise(Problem {
        id: problem_id(&package.name),
        severity: Severity::Warning,
        summary: format!(
            "{} ({}) asks to talk to Cursio as {app}",
            package.label, package.name
        ),
        hint: Some(
            "allowed, it can log, take builds, and send and answer messages through Cursio".into(),
        ),
        actions: vec![
            answer(format!("Let {} talk to Cursio", package.label), true),
            answer("Don't".into(), false),
        ],
        since: std::time::SystemTime::now(),
    });
}

/// What the abstract socket asks of each connection: may the app running as
/// this uid register as this application? `Err` says why not, for its hello.
pub type Gate = Arc<dyn Fn(u32, &AppId) -> Result<(), String> + Send + Sync>;

/// The gate a phone puts in front of its abstract socket: `packages` answers
/// which packages run as a uid (Android's package manager), and the grants
/// are read from `config_path` at every connection, so a button pressed
/// takes effect on the app's next retry.
pub fn package_gate(
    packages: Arc<dyn Fn(u32) -> Vec<Package> + Send + Sync>,
    config_path: PathBuf,
) -> Gate {
    Arc::new(move |uid, app| {
        let grants = Config::load(&config_path)
            .map(|c| c.package_access)
            .unwrap_or_default();
        let found = packages(uid);
        match decide(&grants, &found, app) {
            Decision::Allow => {
                if let Some(package) = found.first() {
                    diagnostics::clear(&problem_id(&package.name));
                }
                Ok(())
            }
            Decision::Refuse(why) => Err(why),
            Decision::Ask(package) => {
                ask(&package, app);
                Err(format!(
                    "waiting for this phone's person to let {} talk to Cursio",
                    package.label
                ))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str) -> Package {
        Package {
            name: name.into(),
            label: "LeavePulse".into(),
        }
    }

    fn grant(package: &str, app: &str, allowed: bool) -> PackageGrant {
        PackageGrant {
            package: package.into(),
            app: app.into(),
            allowed,
        }
    }

    fn app(id: &str) -> AppId {
        AppId::parse(id).unwrap()
    }

    const LAUNCHER: &str = "dev.leavepulse.launcher";

    #[test]
    fn an_unknown_package_is_asked_about_and_an_allowed_one_taken() {
        let pkgs = [package(LAUNCHER)];
        assert_eq!(
            decide(&[], &pkgs, &app("leavepulse")),
            Decision::Ask(package(LAUNCHER))
        );
        let grants = [grant(LAUNCHER, "leavepulse", true)];
        assert_eq!(decide(&grants, &pkgs, &app("leavepulse")), Decision::Allow);
    }

    #[test]
    fn another_app_cannot_take_an_allowed_apps_name() {
        let grants = [grant(LAUNCHER, "leavepulse", true)];
        let Decision::Refuse(why) = decide(&grants, &[package("com.evil.app")], &app("leavepulse"))
        else {
            panic!("an impostor was not refused");
        };
        assert!(why.contains(LAUNCHER), "{why}");
    }

    #[test]
    fn an_allowed_package_keeps_the_name_it_was_allowed_as() {
        let grants = [grant(LAUNCHER, "leavepulse", true)];
        assert!(matches!(
            decide(&grants, &[package(LAUNCHER)], &app("other-app")),
            Decision::Refuse(_)
        ));
    }

    #[test]
    fn a_refused_package_stays_refused_and_nobody_is_taken_unseen() {
        let grants = [grant(LAUNCHER, "leavepulse", false)];
        assert!(matches!(
            decide(&grants, &[package(LAUNCHER)], &app("leavepulse")),
            Decision::Refuse(_)
        ));
        assert!(matches!(
            decide(&grants, &[], &app("leavepulse")),
            Decision::Refuse(_)
        ));
    }

    #[test]
    fn the_buttons_write_the_answer_the_gate_reads() {
        let mut config = Config::default();
        ConfigEdit::PackageAccess {
            package: LAUNCHER.into(),
            app: "leavepulse".into(),
            allowed: false,
        }
        .apply(&mut config);
        ConfigEdit::PackageAccess {
            package: LAUNCHER.into(),
            app: "leavepulse".into(),
            allowed: true,
        }
        .apply(&mut config);
        assert_eq!(config.package_access, [grant(LAUNCHER, "leavepulse", true)]);
    }

    #[test]
    fn the_gate_asks_once_then_takes_the_app_after_the_button() {
        let dir = std::env::temp_dir().join(format!("cursio-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        let gate = package_gate(Arc::new(|_| vec![package(LAUNCHER)]), config_path.clone());
        let why = gate(10_123, &app("leavepulse")).unwrap_err();
        assert!(why.contains("waiting"), "{why}");
        let problem = diagnostics::snapshot()
            .into_iter()
            .find(|p| p.id == problem_id(LAUNCHER))
            .expect("the person is asked");
        let mut config = Config::default();
        problem.actions[0].edit.clone().apply(&mut config);
        config.save(&config_path).unwrap();
        assert_eq!(gate(10_123, &app("leavepulse")), Ok(()));
        assert!(!diagnostics::snapshot()
            .iter()
            .any(|p| p.id == problem_id(LAUNCHER)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
