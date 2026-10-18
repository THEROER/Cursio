//! Where a machine's gallery meets the rest of the node.
//!
//! The same two halves as [`crate::messages_hub`], but every machine has both:
//! its own photos answer peers through a [`MediaStore`], and a window on this
//! machine reaches any peer's through [`GalleryHub::ask`]. Process-wide because
//! the callers — the control socket, a phone's in-process window, the store's
//! change observer — hold no handle to any session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use cursio_protocol::gallery::{GalleryRequest, GalleryResponse, MAX_SLICE};
use cursio_protocol::PeerId;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// Longest a window waits for one answer. A thumbnail is one decode on the
/// other machine and tens of kilobytes on the link; past this the peer is
/// asleep or the link is about to be declared dead.
pub const ANSWER_WAIT: Duration = Duration::from_secs(20);

/// How a machine answers for its photos. Called on the module's blocking pool:
/// a listing walks a folder or queries a provider, a thumbnail decodes a photo.
pub trait MediaStore: Send + Sync {
    fn handle(&self, request: &GalleryRequest) -> Result<GalleryResponse, String>;
}

pub struct Outgoing {
    pub request: GalleryRequest,
    pub reply: oneshot::Sender<Result<GalleryResponse, String>>,
}

/// A peer whose gallery can be browsed now, as the status shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryPeer {
    pub peer: PeerId,
    pub name: String,
    /// Bumped each time the peer says its gallery changed.
    pub revision: u64,
}

struct Link {
    name: String,
    tx: mpsc::UnboundedSender<Outgoing>,
    revision: u64,
    token: u64,
}

#[derive(Default)]
pub struct GalleryHub {
    links: Mutex<HashMap<PeerId, Link>>,
    next_token: AtomicU64,
    /// How many times this machine's own gallery has changed.
    generation: AtomicU64,
}

pub fn hub() -> &'static GalleryHub {
    static HUB: OnceLock<GalleryHub> = OnceLock::new();
    HUB.get_or_init(GalleryHub::default)
}

impl GalleryHub {
    /// This machine's gallery changed; every link tells its peer once.
    pub fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub(crate) fn register(
        &self,
        peer: PeerId,
        name: String,
    ) -> (mpsc::UnboundedReceiver<Outgoing>, u64) {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(
            peer,
            Link {
                name,
                tx,
                revision: 0,
                token,
            },
        );
        (rx, token)
    }

    pub(crate) fn unregister(&self, peer: &PeerId, token: u64) {
        let mut links = self.lock();
        if links.get(peer).is_some_and(|l| l.token == token) {
            links.remove(peer);
        }
    }

    pub(crate) fn bump(&self, peer: &PeerId) {
        if let Some(link) = self.lock().get_mut(peer) {
            link.revision = link.revision.wrapping_add(1);
        }
    }

    /// Peers whose galleries can be browsed now, by name.
    pub fn peers(&self) -> Vec<GalleryPeer> {
        let mut out: Vec<_> = self
            .lock()
            .iter()
            .filter(|(_, link)| !link.tx.is_closed())
            .map(|(peer, link)| GalleryPeer {
                peer: *peer,
                name: link.name.clone(),
                revision: link.revision,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Ask `peer` (an id or a name) and wait for its answer, at most
    /// [`ANSWER_WAIT`].
    pub async fn ask(
        &self,
        peer: &str,
        request: GalleryRequest,
    ) -> Result<GalleryResponse, String> {
        let (reply, answer) = oneshot::channel();
        {
            let links = self.lock();
            let link = links
                .iter()
                .find(|(id, link)| id.to_string() == peer || link.name.eq_ignore_ascii_case(peer))
                .map(|(_, link)| link)
                .ok_or_else(|| {
                    let names: Vec<_> = links.values().map(|l| l.name.as_str()).collect();
                    format!(
                        "no connected machine called {peer} shares its gallery; connected: {}",
                        if names.is_empty() {
                            "none".into()
                        } else {
                            names.join(", ")
                        }
                    )
                })?;
            link.tx
                .send(Outgoing { request, reply })
                .map_err(|_| format!("the link to {} has just closed", link.name))?;
        }
        match tokio::time::timeout(ANSWER_WAIT, answer).await {
            Err(_) => Err(format!(
                "{peer} did not answer within {} s",
                ANSWER_WAIT.as_secs()
            )),
            Ok(Err(_)) => Err(format!("the link to {peer} closed before it answered")),
            Ok(Ok(answer)) => answer,
        }
    }

    /// Copy item `id` of `peer` into `dir`, slice by slice, as `name` — or
    /// `name (2)` and so on beside a file already there. The name came from
    /// the peer, so only its last component is used.
    pub async fn save(
        &self,
        peer: &str,
        id: &str,
        name: &str,
        dir: &Path,
    ) -> Result<PathBuf, String> {
        let path = free_name(dir, &safe_name(name)?)?;
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        // Written beside the target and renamed at the end, so a link lost
        // half way leaves no half photo under the photo's name.
        let partial = path.with_extension("cursio-partial");
        let result = self.copy_into(peer, id, &partial).await;
        match result {
            Ok(()) => tokio::fs::rename(&partial, &path)
                .await
                .map(|()| path)
                .map_err(|e| format!("cannot finish {}: {e}", partial.display())),
            Err(e) => {
                let _ = tokio::fs::remove_file(&partial).await;
                Err(e)
            }
        }
    }

    async fn copy_into(&self, peer: &str, id: &str, path: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        let mut offset = 0u64;
        loop {
            let request = GalleryRequest::Read {
                id: id.to_string(),
                offset,
                len: MAX_SLICE,
            };
            let bytes = match self.ask(peer, request).await? {
                GalleryResponse::Slice(bytes) => bytes,
                other => return Err(format!("{peer} answered a read with {other:?}")),
            };
            if bytes.is_empty() {
                break;
            }
            file.write_all(&bytes)
                .await
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            offset += bytes.len() as u64;
            if bytes.len() < MAX_SLICE as usize {
                break;
            }
        }
        file.flush()
            .await
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, Link>> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Where a machine's photos are saved on this one: `Pictures/Cursio/<peer>`,
/// or the downloads folder where the platform names no Pictures.
pub fn save_dir(peer_name: &str) -> Option<PathBuf> {
    let dirs = directories::UserDirs::new()?;
    let base = dirs
        .picture_dir()
        .map(|p| p.join("Cursio"))
        .or_else(|| dirs.download_dir().map(Path::to_path_buf))?;
    Some(base.join(safe_name(peer_name).unwrap_or_else(|_| "peer".into())))
}

fn safe_name(name: &str) -> Result<String, String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    let clean: String = last
        .chars()
        .map(|c| {
            if c.is_control() || ":*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    if clean.is_empty() || clean == "." || clean == ".." {
        return Err(format!("{name:?} is not a file name"));
    }
    Ok(clean)
}

fn free_name(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let first = dir.join(name);
    if !first.exists() {
        return Ok(first);
    }
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let extension = Path::new(name)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..1000)
        .map(|n| dir.join(format!("{stem} ({n}){extension}")))
        .find(|p| !p.exists())
        .ok_or_else(|| {
            format!(
                "{} already holds a thousand copies of {name}",
                dir.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_from_a_peer_cannot_reach_outside_the_save_folder() {
        assert_eq!(safe_name("../../.bashrc").unwrap(), ".bashrc");
        assert_eq!(safe_name("C:\\Windows\\a.jpg").unwrap(), "a.jpg");
        assert!(safe_name("..").is_err());
        assert!(safe_name("dir/").is_err());
    }

    #[test]
    fn a_second_save_of_one_photo_gets_a_numbered_name() {
        let dir = std::env::temp_dir().join(format!("cursio-gallery-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("IMG.jpg"), b"x").unwrap();
        assert_eq!(free_name(&dir, "IMG.jpg").unwrap(), dir.join("IMG (2).jpg"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
