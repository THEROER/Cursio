//! What the control tests share: a node state, one that applies settings,
//! and a scratch directory for its config.

use crate::config::Config;
use crate::config::Overrides;
use crate::control::Applier;
use crate::control::NodeState;
use crate::pairing_window::PairingWindow;
use cursio_protocol::PeerId;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn state(dir: &std::path::Path, overrides: Overrides) -> NodeState {
    state_applying(dir, overrides, Arc::new(|_: &Config, _: &Config| {}))
}

pub(super) fn state_applying(
    dir: &std::path::Path,
    overrides: Overrides,
    applier: Applier,
) -> NodeState {
    let identity = crate::NodeIdentity {
        peer_id: PeerId::new_v4(),
        display_name: "desk".into(),
    };
    NodeState::new(
        &identity,
        dir.join("config.toml"),
        overrides,
        Config::default(),
        PairingWindow::closed(),
        applier,
    )
}

pub(super) fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cursio-control-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
