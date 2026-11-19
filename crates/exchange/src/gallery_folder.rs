//! A computer's gallery: its Pictures and Videos folders.
//!
//! A computer has no media index the way a phone has MediaStore, so this is
//! one: a walk of the user's Pictures and Videos folders (the XDG user dirs on
//! Linux, the Known Folders on Windows — `directories` asks each platform its
//! own way), screenshots included because they live inside Pictures. The walk
//! is redone when a window asks for the first page, which is when the user
//! opened the gallery or was told it changed, and kept for the pages after it,
//! so scrolling does not walk the disk again per page.
//!
//! An id is `<folder>/<path inside it>`. It comes back from a peer as an
//! untrusted string, so [`FolderGallery::resolve`] rejects anything that is
//! not a plain relative path and checks the result is still inside the folder
//! after links are followed — a gallery is not a way to read `~/.ssh`.
//!
//! Thumbnails are decoded here with `image` and cached on disk by path, size
//! and modification time, because a grid asks again every time it is opened.
//! A video has none: decoding one needs FFmpeg, which only the window
//! module links, and a gallery is not worth tying to it. The peer shows a
//! placeholder with the video's length instead.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cursio_protocol::gallery::{
    GalleryRequest, GalleryResponse, MediaFilter, MediaItem, MediaKind, PageMark,
};
use sha2::{Digest, Sha256};

use crate::gallery_hub::{hub, MediaStore};

/// How deep the walk goes below each folder. Albums nest a level or two; a
/// photo library's own internal tree (Shotwell's, darktable's) nests more and
/// is not where the user's photos are browsed from.
const MAX_DEPTH: usize = 6;

/// Most files indexed. A Pictures folder of more than this is an archive, and
/// a first page should not wait for all of it.
const MAX_ITEMS: usize = 50_000;

/// A walk younger than this answers a first page too. A grid opening asks for
/// its first page and thumbnails within moments; one walk is enough for that.
const FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(10);

struct Entry {
    /// What peers know it by; see the module docs.
    id: String,
    folder: usize,
    relative: PathBuf,
    kind: MediaKind,
    taken_ms: i64,
    size: u64,
}

struct Index {
    at: Instant,
    entries: Vec<Entry>,
}

pub struct FolderGallery {
    folders: Vec<PathBuf>,
    cache: Option<PathBuf>,
    index: Mutex<Option<Index>>,
}

impl FolderGallery {
    /// The user's Pictures and Videos, where the platform names them. `None`
    /// when it names neither, which a headless account may.
    pub fn for_user() -> Option<Self> {
        let dirs = directories::UserDirs::new()?;
        let mut folders: Vec<PathBuf> = Vec::new();
        for dir in [dirs.picture_dir(), dirs.video_dir()].into_iter().flatten() {
            if dir.is_dir()
                && !folders
                    .iter()
                    .any(|f| dir.starts_with(f) || f.starts_with(dir))
            {
                folders.push(dir.to_path_buf());
            }
        }
        if folders.is_empty() {
            return None;
        }
        let cache = directories::ProjectDirs::from("dev", "cursio", "cursio")
            .map(|d| d.cache_dir().join("thumbnails"));
        Some(Self::new(folders, cache))
    }

    pub fn new(folders: Vec<PathBuf>, cache: Option<PathBuf>) -> Self {
        FolderGallery {
            folders,
            cache,
            index: Mutex::new(None),
        }
    }

    fn walk(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        for (folder, root) in self.folders.iter().enumerate() {
            let mut stack = vec![(root.clone(), 0usize)];
            while let Some((dir, depth)) = stack.pop() {
                let Ok(read) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for item in read.flatten() {
                    if entries.len() >= MAX_ITEMS {
                        return entries;
                    }
                    let name = item.file_name();
                    if name.to_string_lossy().starts_with('.') {
                        continue;
                    }
                    // `file_type` does not follow links: a link out of the
                    // folder is skipped here rather than walked.
                    let Ok(kind) = item.file_type() else { continue };
                    let path = item.path();
                    if kind.is_dir() {
                        if depth < MAX_DEPTH {
                            stack.push((path, depth + 1));
                        }
                        continue;
                    }
                    if !kind.is_file() {
                        continue;
                    }
                    let Some(media) = kind_of(&path) else {
                        continue;
                    };
                    let Ok(meta) = item.metadata() else { continue };
                    let Ok(relative) = path.strip_prefix(root) else {
                        continue;
                    };
                    entries.push(Entry {
                        id: format!("{folder}/{}", relative.to_string_lossy().replace('\\', "/")),
                        folder,
                        relative: relative.to_path_buf(),
                        kind: media,
                        taken_ms: meta.modified().map(unix_ms).unwrap_or(0),
                        size: meta.len(),
                    });
                }
            }
        }
        entries.sort_by(|a, b| b.taken_ms.cmp(&a.taken_ms).then_with(|| a.id.cmp(&b.id)));
        entries
    }

    fn list(&self, limit: u32, after: Option<&PageMark>, filter: MediaFilter) -> Vec<MediaItem> {
        let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        let stale = index.as_ref().is_none_or(|i| i.at.elapsed() > FRESH_FOR);
        if after.is_none() && stale || index.is_none() {
            let entries = self.walk();
            let changed = index.as_ref().is_some_and(|old| {
                old.entries.len() != entries.len()
                    || old.entries.first().map(|e| (&e.id, e.taken_ms))
                        != entries.first().map(|e| (&e.id, e.taken_ms))
            });
            *index = Some(Index {
                at: Instant::now(),
                entries,
            });
            if changed {
                hub().changed();
            }
        }
        let Some(index) = index.as_ref() else {
            return Vec::new();
        };
        index
            .entries
            .iter()
            // The order the walk sorted by: newer first, then by id.
            .filter(|e| {
                after.is_none_or(|mark| {
                    e.taken_ms < mark.taken_ms || e.taken_ms == mark.taken_ms && e.id > mark.id
                })
            })
            .filter(|e| match filter {
                MediaFilter::All => true,
                MediaFilter::Images => e.kind == MediaKind::Image,
                MediaFilter::Videos => e.kind == MediaKind::Video,
            })
            .take(limit as usize)
            .map(|e| self.item(e))
            .collect()
    }

    fn item(&self, e: &Entry) -> MediaItem {
        let path = self.folders[e.folder].join(&e.relative);
        // The header only: a page of sizes without decoding a page of photos.
        let (width, height) = match e.kind {
            MediaKind::Image => image::image_dimensions(&path)
                .map(|(w, h)| (Some(w), Some(h)))
                .unwrap_or((None, None)),
            MediaKind::Video => (None, None),
        };
        MediaItem {
            id: e.id.clone(),
            name: e
                .relative
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            kind: e.kind,
            taken_ms: e.taken_ms,
            size: e.size,
            width,
            height,
            duration_ms: None,
            mime: mime_of(&path).to_string(),
        }
    }

    /// The file an id names, if it is one of this gallery's.
    pub fn resolve(&self, id: &str) -> Result<PathBuf, String> {
        let refused = || "not an item of this gallery".to_string();
        let (folder, relative) = id.split_once('/').ok_or_else(refused)?;
        let root = folder
            .parse::<usize>()
            .ok()
            .and_then(|i| self.folders.get(i))
            .ok_or_else(refused)?;
        let relative = Path::new(relative);
        if !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(refused());
        }
        let path = root.join(relative);
        let real = path.canonicalize().map_err(|_| refused())?;
        let real_root = root.canonicalize().map_err(|_| refused())?;
        if !real.starts_with(&real_root) || !real.is_file() || kind_of(&real).is_none() {
            return Err(refused());
        }
        Ok(real)
    }

    fn thumbnail(&self, id: &str, max_px: u32) -> Result<Option<Vec<u8>>, String> {
        let path = self.resolve(id)?;
        if kind_of(&path) != Some(MediaKind::Image) {
            return Ok(None);
        }
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(unix_ms)
            .unwrap_or(0);
        let cached = self.cache.as_ref().map(|dir| {
            let mut hash = Sha256::new();
            hash.update(path.to_string_lossy().as_bytes());
            hash.update(modified.to_le_bytes());
            hash.update(max_px.to_le_bytes());
            let name: String = hash.finalize()[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            dir.join(format!("{name}.jpg"))
        });
        if let Some(bytes) = cached.as_ref().and_then(|c| std::fs::read(c).ok()) {
            return Ok(Some(bytes));
        }
        let picture = match image::open(&path) {
            Ok(picture) => picture,
            // A file named .jpg that is not one, or a format the build does
            // not decode: a placeholder, not a failed page.
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "no thumbnail for a gallery item");
                return Ok(None);
            }
        };
        let small = picture.thumbnail(max_px, max_px).to_rgb8();
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 80)
            .encode_image(&small)
            .map_err(|e| format!("cannot encode a thumbnail: {e}"))?;
        if let Some(cached) = cached {
            if let Some(dir) = cached.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&cached, &bytes) {
                tracing::debug!(error = %e, "cannot cache a gallery thumbnail");
            }
        }
        Ok(Some(bytes))
    }

    fn read(&self, id: &str, offset: u64, len: u32) -> Result<Vec<u8>, String> {
        let path = self.resolve(id)?;
        let mut file = std::fs::File::open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut bytes = Vec::with_capacity(len as usize);
        file.take(len as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Ok(bytes)
    }
}

impl MediaStore for FolderGallery {
    fn handle(&self, request: &GalleryRequest) -> Result<GalleryResponse, String> {
        match request {
            GalleryRequest::List {
                limit,
                after,
                filter,
            } => Ok(GalleryResponse::Items(self.list(
                *limit,
                after.as_ref(),
                *filter,
            ))),
            GalleryRequest::Thumbnail { id, max_px } => {
                self.thumbnail(id, *max_px).map(GalleryResponse::Thumbnail)
            }
            GalleryRequest::Read { id, offset, len } => {
                self.read(id, *offset, *len).map(GalleryResponse::Slice)
            }
        }
    }
}

fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn kind_of(path: &Path) -> Option<MediaKind> {
    match extension(path).as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "heic" | "heif" | "avif" => {
            Some(MediaKind::Image)
        }
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" => Some(MediaKind::Video),
        _ => None,
    }
}

fn mime_of(path: &Path) -> &'static str {
    match extension(path).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cursio-gallery-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Screenshots")).unwrap();
        dir
    }

    fn photo(path: &Path) {
        image::RgbImage::from_pixel(64, 48, image::Rgb([200, 30, 30]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn photos_in_subfolders_are_listed_and_other_files_are_not() {
        let dir = folder("list");
        photo(&dir.join("a.png"));
        photo(&dir.join("Screenshots/b.png"));
        std::fs::write(dir.join("notes.txt"), b"no").unwrap();
        std::fs::write(dir.join(".hidden.png"), b"no").unwrap();
        let gallery = FolderGallery::new(vec![dir.clone()], None);
        let items = gallery.list(10, None, MediaFilter::All);
        let mut names: Vec<_> = items.iter().map(|i| i.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["a.png", "b.png"]);
        assert!(items
            .iter()
            .all(|i| i.width == Some(64) && i.height == Some(48)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_id_cannot_climb_out_of_the_gallery() {
        let dir = folder("escape");
        photo(&dir.join("a.png"));
        let gallery = FolderGallery::new(vec![dir.join("Screenshots")], None);
        assert!(gallery.resolve("0/../a.png").is_err());
        assert!(gallery.resolve("0//etc/passwd").is_err());
        assert!(gallery.resolve("7/a.png").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_gallery_is_not_followed() {
        let dir = folder("link");
        let outside = folder("link-outside");
        photo(&outside.join("secret.png"));
        std::os::unix::fs::symlink(outside.join("secret.png"), dir.join("in.png")).unwrap();
        let gallery = FolderGallery::new(vec![dir.clone()], None);
        assert!(gallery.resolve("0/in.png").is_err());
        assert!(gallery.list(10, None, MediaFilter::All).is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn a_thumbnail_is_a_small_jpeg_and_is_cached() {
        let dir = folder("thumb");
        let cache = dir.join(".cache");
        image::RgbImage::from_pixel(800, 600, image::Rgb([10, 200, 10]))
            .save(dir.join("big.png"))
            .unwrap();
        let gallery = FolderGallery::new(vec![dir.clone()], Some(cache.clone()));
        let bytes = gallery.thumbnail("0/big.png", 128).unwrap().unwrap();
        let small = image::load_from_memory(&bytes).unwrap();
        assert!(small.width() <= 128 && small.height() <= 128);
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_slice_ends_at_the_end_of_the_file() {
        let dir = folder("slice");
        std::fs::write(dir.join("clip.mp4"), b"0123456789").unwrap();
        let gallery = FolderGallery::new(vec![dir.clone()], None);
        assert_eq!(gallery.read("0/clip.mp4", 7, 100).unwrap(), b"789");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn photos_with_one_time_are_all_reached_across_page_edges() {
        let dir = folder("page");
        let names = ["1.png", "2.png", "3.png", "4.png", "5.png"];
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for name in names {
            photo(&dir.join(name));
            std::fs::File::options()
                .write(true)
                .open(dir.join(name))
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        let gallery = FolderGallery::new(vec![dir.clone()], None);
        let mut seen = Vec::new();
        let mut after: Option<PageMark> = None;
        loop {
            let page = gallery.list(2, after.as_ref(), MediaFilter::Images);
            let Some(last) = page.last() else { break };
            after = Some(PageMark {
                taken_ms: last.taken_ms,
                id: last.id.clone(),
            });
            seen.extend(page.into_iter().map(|i| i.name));
        }
        assert_eq!(seen, names);
        assert!(gallery.list(10, None, MediaFilter::Videos).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
