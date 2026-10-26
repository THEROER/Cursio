//! The node's own settings and pairing, as the window changes them.

use crate::node::*;
use tauri::State;

#[tauri::command]
pub async fn node_status(link: State<'_, NodeLink>) -> Result<Status, NodeError> {
    link.status().await
}

#[tauri::command]
pub async fn node_config(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
    link.request(Command::Config).await
}

#[tauri::command]
pub async fn node_set_module(
    link: State<'_, NodeLink>,
    module: ModuleSwitch,
    enabled: bool,
) -> Result<Reply, NodeError> {
    link.request(Command::SetModule { module, enabled }).await
}

/// Desktop only: a phone never takes the cursor, so it has no place in the
/// layout to move.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_set_position(
    link: State<'_, NodeLink>,
    x: i32,
    y: i32,
) -> Result<Reply, NodeError> {
    link.request(Command::SetPosition {
        position: Position::new(x, y),
    })
    .await
}

/// Stand a machine on the shared desk; `machine` absent is this one.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_arrange(
    link: State<'_, NodeLink>,
    machine: Option<cursio_protocol::PeerId>,
    x_mm: i32,
    y_mm: i32,
) -> Result<Reply, NodeError> {
    link.request(Command::Arrange {
        machine,
        place: cursio_protocol::Place { x_mm, y_mm },
    })
    .await
}

/// Stand one screen of a machine apart from where its pixels put it.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_arrange_screen(
    link: State<'_, NodeLink>,
    machine: Option<cursio_protocol::PeerId>,
    screen: String,
    dx_mm: i32,
    dy_mm: i32,
) -> Result<Reply, NodeError> {
    link.request(Command::ArrangeScreen {
        machine,
        screen,
        dx_mm,
        dy_mm,
    })
    .await
}

/// Close or open the seam between one of this machine's screens and a
/// peer's.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_set_barrier(
    link: State<'_, NodeLink>,
    screen: String,
    peer: cursio_protocol::PeerId,
    peer_screen: String,
    closed: bool,
) -> Result<Reply, NodeError> {
    link.request(Command::Barrier {
        screen,
        peer,
        peer_screen,
        closed,
    })
    .await
}

/// The node has no command for one audio field, so this edits the file as
/// written (not the effective config, which would bake flags into it) and
/// hands the whole of it back. `value` is what the file takes: `auto`,
/// `off` or a level in dBFS.
///
/// Checked here because the node does not check it on save: a running
/// node only declines to apply a bad value, but the file keeps it and the
/// next start fails on it. The rule is `audio_gate::MicGate`'s, which the
/// app cannot call — it lives behind the core's `audio` feature.
#[tauri::command]
pub async fn node_set_mic_gate(
    link: State<'_, NodeLink>,
    value: String,
) -> Result<Reply, NodeError> {
    let level_ok = value
        .parse::<f32>()
        .is_ok_and(|db| (-100.0..=0.0).contains(&db));
    if !matches!(value.as_str(), "auto" | "off") && !level_ok {
        return Err(NodeError::failed(format!(
            "mic gate {value:?} is not \"auto\", \"off\" or a level in dBFS between -100 and 0"
        )));
    }
    let mut config = match link.request(Command::Config).await? {
        Reply::Config { file, .. } => *file,
        other => {
            return Err(NodeError::failed(format!(
                "the node answered a config request with {other:?}"
            )));
        }
    };
    config.audio.mic_gate = Some(value);
    link.request(Command::SetConfig {
        config: Box::new(config),
    })
    .await
}

/// Replace the `[calls]` section of the config file, like
/// [`node_set_mic_gate`]: the file's own config, not the effective one.
#[tauri::command]
pub async fn node_set_calls(
    link: State<'_, NodeLink>,
    calls: cursio_core::config::CallsConfig,
) -> Result<Reply, NodeError> {
    let mut config = match link.request(Command::Config).await? {
        Reply::Config { file, .. } => *file,
        other => {
            return Err(NodeError::failed(format!(
                "the node answered a config request with {other:?}"
            )));
        }
    };
    config.calls = calls;
    link.request(Command::SetConfig {
        config: Box::new(config),
    })
    .await
}

/// Keep a Bluetooth path open beside the network, or not: the config
/// file's `[bluetooth] standby`, like [`node_set_calls`].
#[tauri::command]
pub async fn node_set_bluetooth_standby(
    link: State<'_, NodeLink>,
    on: bool,
) -> Result<Reply, NodeError> {
    let mut config = match link.request(Command::Config).await? {
        Reply::Config { file, .. } => *file,
        other => {
            return Err(NodeError::failed(format!(
                "the node answered a config request with {other:?}"
            )));
        }
    };
    config.bluetooth.standby = on;
    link.request(Command::SetConfig {
        config: Box::new(config),
    })
    .await
}

/// Make one change a problem offers, or a settings control names: the
/// config file read, edited and written back like the setters above.
#[tauri::command]
pub async fn node_apply_config_edit(
    link: State<'_, NodeLink>,
    edit: cursio_core::config::ConfigEdit,
) -> Result<Reply, NodeError> {
    let mut config = match link.request(Command::Config).await? {
        Reply::Config { file, .. } => *file,
        other => {
            return Err(NodeError::failed(format!(
                "the node answered a config request with {other:?}"
            )));
        }
    };
    edit.apply(&mut config);
    link.request(Command::SetConfig {
        config: Box::new(config),
    })
    .await
}

#[tauri::command]
pub async fn node_pair_show(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
    link.request(Command::PairShow).await
}

#[tauri::command]
pub async fn node_pair_enter(link: State<'_, NodeLink>, code: String) -> Result<Reply, NodeError> {
    link.request(Command::PairEnter { code }).await
}

#[tauri::command]
pub async fn node_pair_cancel(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
    link.request(Command::PairCancel).await
}

#[tauri::command]
pub async fn node_pairing(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
    link.request(Command::Pairing).await
}

/// Desktop only: restarting re-executes the `cursio` process, and a phone's
/// node is this app's process.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_restart(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
    link.request(Command::Restart).await
}

/// Desktop only: a phone is updated by installing the app.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_update_apply(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
    link.request(Command::UpdateApply).await
}

/// `peer` as the status carries it: the id, not the name, since two
/// machines may share a name and only one is meant.
#[tauri::command]
pub async fn node_forget(link: State<'_, NodeLink>, peer: String) -> Result<Reply, NodeError> {
    let peer = peer
        .parse()
        .map_err(|e| NodeError::failed(format!("`{peer}` is not a machine id: {e}")))?;
    link.request(Command::Forget { peer }).await
}

/// Rename this machine. Saved at once; links take it when they next come
/// up, so the status lists `name` among what waits for a restart.
#[tauri::command]
pub async fn node_set_name(link: State<'_, NodeLink>, name: String) -> Result<Reply, NodeError> {
    link.request(Command::SetName { name }).await
}

/// The listener's volume on one peer stream playing here.
#[tauri::command]
pub async fn node_set_stream_volume(
    link: State<'_, NodeLink>,
    stream: u32,
    volume: f32,
    muted: bool,
) -> Result<Reply, NodeError> {
    link.request(Command::SetStreamVolume {
        stream,
        volume,
        muted,
    })
    .await
}
