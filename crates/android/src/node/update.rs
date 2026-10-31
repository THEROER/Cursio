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
    .with_built_at(cursio_core::update::running_built_at());
    let agent: Arc<Agent> = cursio_core::update::agent::install(agent);
    tracing::info!(platform = agent.platform(), "taking builds from the mesh");
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
    .map_err(|why| UpdateError::Refused(format!("the phone could not offer the update: {why}")))
}
