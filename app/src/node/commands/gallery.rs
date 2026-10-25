//! A machine's gallery: pages, thumbnails, and files saved here.

use crate::node::*;
use tauri::State;

/// Put a gallery command to the machine it names: through the node's
/// socket on a desktop, straight to the in-process node on a phone.
async fn gallery(
    #[cfg_attr(mobile, allow(unused_variables))] link: State<'_, NodeLink>,
    command: Command,
) -> Result<Reply, NodeError> {
    #[cfg(desktop)]
    {
        let deadline = match command {
            Command::GallerySave { .. } => GALLERY_SAVE_TIMEOUT,
            _ => GALLERY_TIMEOUT,
        };
        link.request_within(command, deadline).await
    }
    #[cfg(mobile)]
    match cursio_core::control::ask_gallery(&command).await {
        Some(Reply::Error { message }) => Err(NodeError::Failed { message }),
        Some(reply) => Ok(reply),
        None => Err(NodeError::failed("not a gallery command")),
    }
}

/// A page of a machine's gallery, newest first. `peer` is its id.
#[tauri::command]
pub async fn gallery_list(
    link: State<'_, NodeLink>,
    peer: String,
    limit: u32,
    after: Option<cursio_protocol::gallery::PageMark>,
    filter: cursio_protocol::gallery::MediaFilter,
) -> Result<Reply, NodeError> {
    gallery(
        link,
        Command::GalleryList {
            peer,
            limit,
            after,
            filter,
        },
    )
    .await
}

#[tauri::command]
pub async fn gallery_thumbnail(
    link: State<'_, NodeLink>,
    peer: String,
    item: String,
    max_px: u32,
) -> Result<Reply, NodeError> {
    gallery(link, Command::GalleryThumbnail { peer, item, max_px }).await
}

#[tauri::command]
pub async fn gallery_read(
    link: State<'_, NodeLink>,
    peer: String,
    item: String,
    offset: u64,
    len: u32,
) -> Result<Reply, NodeError> {
    gallery(
        link,
        Command::GalleryRead {
            peer,
            item,
            offset,
            len,
        },
    )
    .await
}

/// Copy one item into this computer's `Pictures/Cursio/<peer>`.
#[cfg(desktop)]
#[tauri::command]
pub async fn gallery_save(
    link: State<'_, NodeLink>,
    peer: String,
    item: String,
    name: String,
) -> Result<Reply, NodeError> {
    gallery(
        link,
        Command::GallerySave {
            peer,
            item,
            name,
            dir: None,
        },
    )
    .await
}

/// Copy one item where the user picks in the system's save dialog;
/// `None` when the dialog was cancelled.
#[cfg(desktop)]
#[tauri::command]
pub async fn gallery_save_as(
    app: tauri::AppHandle,
    link: State<'_, NodeLink>,
    peer: String,
    item: String,
    name: String,
) -> Result<Option<String>, NodeError> {
    use tauri_plugin_dialog::DialogExt;
    // The dialog blocks until answered, so it waits off the runtime.
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_file_name(&name)
            .blocking_save_file()
    })
    .await
    .map_err(NodeError::failed)?;
    let Some(picked) = picked else {
        return Ok(None);
    };
    let picked = picked
        .into_path()
        .map_err(|e| NodeError::failed(format!("not a place on this computer: {e}")))?;
    let (Some(dir), Some(name)) = (picked.parent(), picked.file_name()) else {
        return Err(NodeError::failed(format!(
            "{} names no file",
            picked.display()
        )));
    };
    let reply = gallery(
        link,
        Command::GallerySave {
            peer,
            item,
            name: name.to_string_lossy().into_owned(),
            dir: Some(dir.to_path_buf()),
        },
    )
    .await?;
    Ok(saved_path(reply))
}

/// Copy one item into the app's cache, for opening it in its own
/// application, copying it as a file or sending it on. Answers the path.
#[cfg(desktop)]
#[tauri::command]
pub async fn gallery_fetch(
    app: tauri::AppHandle,
    link: State<'_, NodeLink>,
    peer: String,
    item: String,
    name: String,
) -> Result<String, NodeError> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| NodeError::failed(format!("this computer names no cache folder: {e}")))?
        .join("gallery")
        // The peer's id, not its name: an id is always a safe folder name.
        .join(&peer);
    // Fetched afresh each time: a name kept from an earlier fetch may be a
    // different photo now, and `save` picks a free name beside it anyway.
    let reply = gallery(
        link,
        Command::GallerySave {
            peer,
            item,
            name,
            dir: Some(dir),
        },
    )
    .await?;
    saved_path(reply).ok_or_else(|| NodeError::failed("the node saved nothing"))
}

#[cfg(desktop)]
fn saved_path(reply: Reply) -> Option<String> {
    match reply {
        Reply::SavedFile { path } => Some(path.to_string_lossy().into_owned()),
        _ => None,
    }
}
