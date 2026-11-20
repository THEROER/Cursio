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

/// Every setting of this machine, or of `machine` through the mesh. The
/// window builds its settings page from this, so a setting a module gains
/// shows without a change here.
#[tauri::command]
pub async fn node_settings(
    link: State<'_, NodeLink>,
    machine: Option<String>,
) -> Result<Reply, NodeError> {
    link.request_settings(Command::Settings { machine }).await
}

/// Set one setting by key, here or on `machine`; the node checks the value
/// against the setting's kind and applies it at once where it can.
#[tauri::command]
pub async fn node_set_setting(
    link: State<'_, NodeLink>,
    machine: Option<String>,
    key: String,
    value: serde_json::Value,
) -> Result<Reply, NodeError> {
    link.request_settings(Command::SetSetting {
        machine,
        key,
        value,
    })
    .await
}

/// Take one setting out of the file, back to its default.
#[tauri::command]
pub async fn node_reset_setting(
    link: State<'_, NodeLink>,
    machine: Option<String>,
    key: String,
) -> Result<Reply, NodeError> {
    link.request_settings(Command::ResetSetting { machine, key })
        .await
}

/// Make one change a problem offers, or a settings control names: the
/// config file read, edited and written back.
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
