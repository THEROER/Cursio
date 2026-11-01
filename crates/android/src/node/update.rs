//! The phone takes its own builds from the mesh, like the computers do.
//!
//! What crosses is the same signed statement and the same files, fetched by
//! the same lane, for the platform `android-aarch64`, whose one file is the
//! APK. The last step is the phone's own: Android replaces an app only through
//! its installer, which asks the person, so instead of switching versions the
//! agent hands the APK up to Kotlin (`CursioNative.offerUpdate`), which posts
//! a notification whose tap opens that installer. Before this, a phone took a
//! new build only over USB, and fixes for it sat unshipped until somebody
//! plugged it in.

use std::path::PathBuf;
use std::sync::Arc;

use cursio_core::config::Config;
use cursio_core::update::agent::Agent;
use cursio_core::update::layout::InstallLayout;
use cursio_core::update::UpdateError;
use jni::objects::JValue;

/// The component of a phone build: the APK itself.
pub const APK_COMPONENT: &str = "apk";

/// Start this phone's update agent, reading `[update]` from `config_path` at
/// every decision.
pub fn start(config_path: PathBuf) {
    let Some(root) = cursio_protocol::paths::data_dir().map(|d| d.join("updates")) else {
        tracing::warn!("no data directory; this phone takes no builds from the mesh");
        return;
    };
    let agent = Agent::new(
        InstallLayout::at(root),
        cursio_core::update::platform(),
        move || match Config::load(&config_path) {
            Ok(config) => config.update,
            Err(error) => {
                tracing::warn!(%error, "cannot read the config; updates fall back to asking");
                Default::default()
            }
        },
    )
    .with_switcher(Box::new(offer_to_installer))
    .with_built_at(cursio_core::update::running_built_at())
    .recover_ready();
    let agent: Arc<Agent> = cursio_core::update::agent::install(agent);
    tracing::info!(platform = agent.platform(), "taking builds from the mesh");
}

/// Take other applications' builds from the mesh too (`docs/app-sdk-design.md`,
/// step 4): each under `data_dir/apps/<id>`, its APK handed to the installer
/// the way this app's own is.
pub fn start_apps(config_path: PathBuf) {
    let Some(root) = cursio_protocol::paths::data_dir().map(|d| d.join("apps")) else {
        tracing::warn!("no data directory; this phone takes no applications' builds");
        return;
    };
    cursio_core::apps::builds::start_with(
        root,
        config_path,
        Arc::new(offer_app_to_installer),
        Arc::new(open_package),
    );
    tracing::info!("taking applications' builds from the mesh");
}

/// The agent whose builds `app` names: Cursio's own for `None`.
fn agent_for(app: Option<&str>) -> Result<Arc<Agent>, String> {
    match app {
        None => cursio_core::update::agent::global()
            .ok_or_else(|| "this phone is not taking builds from the mesh".to_owned()),
        Some(app) => cursio_core::apps::builds::builds()
            .and_then(|b| b.agent(app))
            .ok_or_else(|| format!("this phone takes no builds of {app}")),
    }
}

/// The APK of the build waiting for `app` (`None`: Cursio's own), for the
/// window's Install button to hand straight to the installer.
pub fn waiting_apk(app: Option<&str>) -> Result<String, String> {
    let agent = agent_for(app)?;
    let ready = agent.ready().ok_or_else(|| match app {
        None => "no Cursio build is waiting to be installed".to_owned(),
        Some(app) => format!("no build of {app} is waiting to be installed"),
    })?;
    let apk = match app {
        None => agent.layout().executable(&ready.version, APK_COMPONENT),
        Some(_) => app_apk(agent.layout(), &ready.version),
    }
    .map_err(|e| e.to_string())?;
    Ok(apk.to_string_lossy().into_owned())
}

/// The window came to the front: drop application builds the person has put
/// on since, then post the notification again for each build still waiting.
/// Swiping it away, or backing out of the installer, used to lose the only
/// way back to it.
pub fn window_shown() {
    if let Some(builds) = cursio_core::apps::builds::builds() {
        for agent in builds.known() {
            let Some(waiting) = agent.waiting() else {
                continue;
            };
            match app_apk(agent.layout(), &waiting.version) {
                Ok(apk) if apk_installed(&apk) => agent.now_running(&waiting.version),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(app = agent.subject(), %error, "a waiting build has no APK to offer")
                }
            }
        }
    }
    for waiting in cursio_core::update::agent::waiting() {
        let Ok(agent) = agent_for(waiting.app.as_deref()) else {
            continue;
        };
        let offered = match waiting
            .app
            .as_deref()
            .map(cursio_app_protocol::AppId::parse)
        {
            None => offer_to_installer(agent.layout(), &waiting.version),
            Some(Ok(app)) => offer_app_to_installer(&app, agent.layout(), &waiting.version),
            Some(Err(_)) => continue,
        };
        if let Err(error) = offered {
            tracing::warn!(app = ?waiting.app, version = %waiting.version, %error, "could not offer a waiting build again");
        }
    }
}

/// Whether the package `apk` holds is installed at its version or later.
fn apk_installed(apk: &std::path::Path) -> bool {
    let path = apk.to_string_lossy().into_owned();
    crate::jvm::with_native(|env, class| {
        let path = env.new_string(&path)?;
        env.call_static_method(
            class,
            "apkInstalled",
            "(Ljava/lang/String;)Z",
            &[JValue::Object(&path)],
        )?
        .z()
    })
    .unwrap_or_else(|why| {
        tracing::warn!(%why, "could not ask whether a build is installed");
        false
    })
}

/// The APK of an application's build: the component named `apk`, else the
/// one file ending in `.apk`.
fn app_apk(layout: &InstallLayout, version: &str) -> Result<PathBuf, UpdateError> {
    let manifest = layout.manifest(version)?;
    let name = match manifest.components.get(APK_COMPONENT) {
        Some(_) => APK_COMPONENT.to_owned(),
        None => {
            let apks: Vec<&String> = manifest
                .components
                .iter()
                .filter(|(_, c)| c.path.ends_with(".apk"))
                .map(|(name, _)| name)
                .collect();
            match apks.as_slice() {
                [one] => (*one).clone(),
                [] => {
                    return Err(UpdateError::Refused(format!(
                        "build {version} carries no APK for a phone to install"
                    )))
                }
                _ => {
                    return Err(UpdateError::Refused(format!(
                        "build {version} carries several APKs; a phone build carries one"
                    )))
                }
            }
        }
    };
    layout.executable(version, &name)
}

/// Hand an application's installed APK to the system installer. The phone's
/// side checks first that it can replace what is installed: an APK signed by
/// another key is refused by the installer only after the person taps, with
/// an error that names nothing.
fn offer_app_to_installer(
    app: &cursio_app_protocol::AppId,
    layout: &InstallLayout,
    version: &str,
) -> Result<(), UpdateError> {
    let apk = app_apk(layout, version)?;
    let path = apk.to_string_lossy().into_owned();
    let refusal = crate::jvm::with_native(|env, class| {
        let path = env.new_string(&path)?;
        let name = env.new_string(app.as_str())?;
        let version = env.new_string(version)?;
        let answer = env
            .call_static_method(
                class,
                "offerAppUpdate",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[
                    JValue::Object(&path),
                    JValue::Object(&name),
                    JValue::Object(&version),
                ],
            )?
            .l()?;
        let answer = jni::objects::JString::from(answer);
        let answer: String = env.get_string(&answer)?.into();
        Ok(answer)
    })
    .map_err(|why| UpdateError::Refused(format!("the phone could not offer {app}: {why}")))?;
    let problem = format!("app.{app}.signer");
    if !refusal.is_empty() {
        tracing::warn!(%app, %version, %refusal, "an application's build cannot replace what is installed");
        cursio_core::diagnostics::raise(cursio_core::diagnostics::Problem {
            id: problem,
            severity: cursio_core::diagnostics::Severity::Warning,
            summary: format!("{app} {version} cannot be installed here: {refusal}"),
            hint: Some(
                "Android replaces an app only with one signed by the same key; build the \
                 development variant under its own id (applicationIdSuffix \".dev\")"
                    .into(),
            ),
            actions: Vec::new(),
            since: std::time::SystemTime::now(),
        });
        return Err(UpdateError::Refused(refusal));
    }
    cursio_core::diagnostics::clear(&problem);
    tracing::info!(%app, %version, "handed an application's build to the system installer");
    Ok(())
}

/// Open `package`'s launcher screen, as `CursioNative.openPackage` answers:
/// `direct` or `asked`, then the installed versionName, tab-separated; or
/// `error` and why.
fn open_package(package: &str) -> Result<cursio_core::apps::phone_open::Opened, String> {
    let answer = crate::jvm::with_native(|env, class| {
        let package = env.new_string(package)?;
        let answer = env
            .call_static_method(
                class,
                "openPackage",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&package)],
            )?
            .l()?;
        let answer = jni::objects::JString::from(answer);
        let answer: String = env.get_string(&answer)?.into();
        Ok(answer)
    })
    .map_err(|why| format!("the phone could not open {package}: {why}"))?;
    parse_opened(&answer)
}

fn parse_opened(answer: &str) -> Result<cursio_core::apps::phone_open::Opened, String> {
    use cursio_core::apps::phone_open::Opened;
    let (kind, rest) = answer.split_once('\t').unwrap_or((answer, ""));
    let version_name = (!rest.is_empty()).then(|| rest.to_owned());
    match kind {
        "direct" => Ok(Opened::Direct { version_name }),
        "asked" => Ok(Opened::Asked { version_name }),
        "error" => Err(rest.to_owned()),
        other => Err(format!("the phone answered {other:?} to opening an app")),
    }
}

/// Hand an installed build's APK to the system installer, through a
/// notification the person taps.
fn offer_to_installer(layout: &InstallLayout, version: &str) -> Result<(), UpdateError> {
    let apk = layout.executable(version, APK_COMPONENT)?;
    let path = apk.to_string_lossy().into_owned();
    crate::jvm::with_native(|env, class| {
        let path = env.new_string(&path)?;
        let version = env.new_string(version)?;
        env.call_static_method(
            class,
            "offerUpdate",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[JValue::Object(&path), JValue::Object(&version)],
        )?;
        Ok(())
    })
    .map_err(|why| UpdateError::Refused(format!("the phone could not offer the update: {why}")))?;
    tracing::info!(%version, "handed the build to the system installer");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_core::apps::phone_open::Opened;

    #[test]
    fn the_phones_answer_to_opening_an_app_reads_back() {
        assert_eq!(
            parse_opened("direct\t0.6.1").unwrap(),
            Opened::Direct {
                version_name: Some("0.6.1".into())
            }
        );
        assert_eq!(
            parse_opened("asked\t").unwrap(),
            Opened::Asked { version_name: None }
        );
        assert_eq!(
            parse_opened("error\tx is not installed on this phone").unwrap_err(),
            "x is not installed on this phone"
        );
        assert!(parse_opened("what").is_err());
    }
}
