//! What the window asks a peer to do: call, show its screen, ring, wake —
//! and starting the node itself.

use crate::node::*;
use tauri::State;

/// Answer, decline, end or move the voice of a phone's call. `phone` is
/// its name as the status's `calls` gives it.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_call(
    link: State<'_, NodeLink>,
    phone: String,
    action: cursio_core::calls_hub::CallAction,
) -> Result<Reply, NodeError> {
    link.request(Command::Call { phone, action }).await
}

/// Show a phone's screen in a window here, or stop.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_phone_screen(
    link: State<'_, NodeLink>,
    phone: String,
    show: bool,
) -> Result<Reply, NodeError> {
    link.request(Command::PhoneScreen { peer: phone, show })
        .await
}

/// Make a connected phone ring until it is found.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_ring(link: State<'_, NodeLink>, phone: String) -> Result<Reply, NodeError> {
    link.request(Command::Ring { peer: phone }).await
}

/// Wake a paired machine that is asleep. Not through the node: the
/// packet needs only the address the trust store keeps, and a machine
/// worth waking is often woken while this one's node is still starting.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_wake(machine: String) -> Result<String, NodeError> {
    tauri::async_runtime::spawn_blocking(move || cursio_core::wake::wake(&machine))
        .await
        .map_err(NodeError::failed)?
        .map_err(NodeError::failed)
}

/// Off the window's thread: `systemctl` or a process spawn can take a
/// noticeable moment, and a synchronous command would freeze the window
/// for it.
#[cfg(desktop)]
#[tauri::command]
pub async fn node_start() -> Result<(), NodeError> {
    tauri::async_runtime::spawn_blocking(start_node)
        .await
        .map_err(NodeError::failed)?
}

/// On a phone the node belongs to the foreground service, not the window:
/// starting it means starting the service, which outlives this Activity.
#[cfg(mobile)]
#[tauri::command]
pub async fn node_start(app: AppHandle) -> Result<(), NodeError> {
    crate::mobile::start_service(&app).map_err(NodeError::failed)
}
