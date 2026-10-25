//! Files the window opens, reveals, copies or sends to a peer.

use crate::node::*;
use tauri::State;

/// Open a file on this computer in its default application.
#[tauri::command]
pub fn file_open(path: String) -> Result<(), NodeError> {
    crate::gallery_files::open(std::path::Path::new(&path)).map_err(NodeError::failed)
}

/// Show a file on this computer selected in the file manager.
#[tauri::command]
pub fn file_reveal(path: String) -> Result<(), NodeError> {
    crate::gallery_files::reveal(std::path::Path::new(&path)).map_err(NodeError::failed)
}

/// Put files on this computer's clipboard as files.
#[tauri::command]
pub async fn files_copy(paths: Vec<String>) -> Result<(), NodeError> {
    let paths: Vec<std::path::PathBuf> = paths.into_iter().map(Into::into).collect();
    // The clipboard backend takes ownership on a thread and waits for it.
    tauri::async_runtime::spawn_blocking(move || crate::gallery_files::copy(&paths))
        .await
        .map_err(NodeError::failed)?
        .map_err(NodeError::failed)
}

/// Send a file on this computer to a peer's downloads. `peer` is its id or
/// its name.
#[tauri::command]
pub async fn file_send(
    link: State<'_, NodeLink>,
    peer: String,
    path: String,
) -> Result<Reply, NodeError> {
    link.request(Command::Share {
        peer,
        target: path,
        action: cursio_protocol::share::ShareAction::Save,
    })
    .await
}
