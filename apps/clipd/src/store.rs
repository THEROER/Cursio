//! The history: what was copied, in what order, and where the bytes live.
//!
//! ## Where content is kept
//!
//! Text goes in SQLite; image bytes go in files next to it, with only a
//! reference in the row. This split is not premature optimisation — it follows
//! from what a clipboard history actually accumulates. A screenshot is a few
//! megabytes, and a day of copying produces dozens. Storing those as blobs
//! would mean the popup's "list the last 50 items" query drags megabytes
//! through SQLite just to render a list of labels, and the database file grows
//! to a size where a single stray copy of a huge image makes every later query
//! slower. With blobs on disk, listing touches only small rows, and dropping an
//! old item is one `unlink`.
//!
//! ## Deduplication
//!
//! Blobs are named by the SHA-256 of their content, so copying the same image
//! twice costs one file, not two. That is worth doing for a history
//! specifically: re-copying the same thing is one of the most common things a
//! person does with a clipboard. It also makes writes idempotent — a blob that
//! already exists needs no work at all.
//!
//! Because several rows can point at one blob, a blob is only deleted once no
//! row references it; see [`Store::prune`].
//!
//! ## Limits
//!
//! History is bounded two ways, because either alone fails: a count limit
//! ("last 200 items") lets 200 screenshots fill a disk, while a byte limit
//! alone would let thousands of tiny text snippets accumulate and make the
//! popup unusable. Both are enforced on every insert.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clipboard_core::{ClipboardContent, ClipboardEntry, ClipboardFormat};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// How many items to keep before evicting the oldest.
pub const DEFAULT_MAX_ITEMS: usize = 200;

/// How many bytes of blobs to keep before evicting the oldest.
pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Longest edge of a generated thumbnail, in pixels.
///
/// Big enough to stay sharp on a HiDPI screen, small enough that a row costs
/// tens of kilobytes instead of megabytes. The picker shows a list of these, so
/// decoding the full images instead would make opening the popup visibly slow.
const THUMB_MAX_EDGE: u32 = 256;

/// Text longer than this is truncated for the preview shown in the picker.
/// The full text is always kept intact; this only bounds what is rendered.
const PREVIEW_CHARS: usize = 120;

/// One remembered clipboard item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryItem {
    /// Row id, and the handle used to recall or pin this item.
    pub id: i64,
    /// Unix timestamp (seconds) when it was copied.
    pub copied_at: i64,
    /// A short human-readable label for the picker.
    pub preview: String,
    /// The richest format this item holds — what the picker shows an icon for.
    pub kind: ItemKind,
    /// Total bytes across every representation.
    pub size: u64,
    /// Pinned items survive eviction (see [`Store::prune`]).
    pub pinned: bool,
    /// Path to a small PNG preview, for image items. `None` for text, and for
    /// images whose thumbnail could not be generated.
    pub thumbnail: Option<PathBuf>,
}

/// What an item mostly *is*, for display and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Text,
    Image,
    Files,
}

impl ItemKind {
    /// The single-character marker the picker prefixes an entry with.
    pub fn marker(self) -> &'static str {
        match self {
            ItemKind::Text => "T",
            ItemKind::Image => "I",
            ItemKind::Files => "F",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ItemKind::Text => "text",
            ItemKind::Image => "image",
            ItemKind::Files => "files",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "image" => ItemKind::Image,
            "files" => ItemKind::Files,
            _ => ItemKind::Text,
        }
    }
}

/// The history database plus its blob directory.
pub struct Store {
    conn: Connection,
    blobs: PathBuf,
    max_items: usize,
    max_bytes: u64,
}

impl Store {
    /// Open (creating if needed) the history under `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating history directory {}", dir.display()))?;
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs)
            .with_context(|| format!("creating blob directory {}", blobs.display()))?;

        let conn = Connection::open(dir.join("history.db"))
            .with_context(|| format!("opening history database in {}", dir.display()))?;

        // WAL keeps the watcher's writes from blocking the picker's reads —
        // they are separate processes hitting the same file.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        let store = Store { conn, blobs, max_items: DEFAULT_MAX_ITEMS, max_bytes: DEFAULT_MAX_BYTES };
        store.migrate()?;
        Ok(store)
    }

    /// Override the eviction limits.
    pub fn with_limits(mut self, max_items: usize, max_bytes: u64) -> Self {
        self.max_items = max_items;
        self.max_bytes = max_bytes;
        self
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS items (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                copied_at INTEGER NOT NULL,
                preview   TEXT    NOT NULL,
                kind      TEXT    NOT NULL,
                size      INTEGER NOT NULL,
                pinned    INTEGER NOT NULL DEFAULT 0,
                -- Monotonic recency counter. Separate from `id` because an
                -- item's identity must not change when it is re-copied, while
                -- its position in the list must.
                seq       INTEGER NOT NULL DEFAULT 0,
                -- Identifies the content, so a repeat copy updates the existing
                -- row's timestamp instead of adding a duplicate.
                digest    TEXT    NOT NULL,
                -- Blob name of a small PNG preview, for image items. Generated
                -- once on insert: the picker renders a list of these, and
                -- decoding full images per row would make it visibly slow.
                thumb     TEXT
            );

            -- One row per representation of an item. Text is stored inline;
            -- anything binary lives in a blob file named by its digest.
            CREATE TABLE IF NOT EXISTS reps (
                item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                mime    TEXT    NOT NULL,
                size    INTEGER NOT NULL,
                inline  BLOB,
                blob    TEXT,
                PRIMARY KEY (item_id, mime)
            );

            CREATE INDEX IF NOT EXISTS items_recent ON items(seq DESC);
            CREATE UNIQUE INDEX IF NOT EXISTS items_digest ON items(digest);
            CREATE INDEX IF NOT EXISTS reps_blob ON reps(blob);
            "#,
        )?;

        // `CREATE TABLE IF NOT EXISTS` leaves an existing table untouched, so a
        // database created by an older version keeps its old shape and every
        // query naming a newer column fails. Columns added after the first
        // release therefore have to be applied separately, here.
        self.add_column_if_missing("items", "thumb", "TEXT")?;
        Ok(())
    }

    /// Add a column when it isn't there yet, so old databases catch up.
    ///
    /// SQLite has no `ADD COLUMN IF NOT EXISTS`, hence the explicit check
    /// against `PRAGMA table_info` rather than running the statement and
    /// swallowing the error — which would also hide real failures.
    fn add_column_if_missing(&self, table: &str, column: &str, ty: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<_, _>>()?;

        if !existing.iter().any(|name| name == column) {
            tracing::info!(table, column, "migrating: adding column");
            self.conn
                .execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"), [])?;
        }
        Ok(())
    }

    /// Record a clipboard item, or bump an identical one to the top.
    ///
    /// Ordering everywhere is by `seq`, not by `copied_at`: the timestamp has
    /// one-second resolution, and several copies within one second are common
    /// (a burst of copying, or a script). With ties left unbroken SQLite may
    /// return them in any order — which made eviction keep the *oldest* items
    /// instead of the newest. `seq` is a strictly increasing counter, so
    /// recency is always unambiguous.
    ///
    /// Returns the row id. Re-copying something already in history moves it to
    /// the front rather than adding a second entry — which is what makes the
    /// picker's recency order useful instead of a wall of repeats.
    pub fn insert(&mut self, content: &ClipboardContent) -> Result<i64> {
        let digest = content_digest(content);
        let now = unix_now();

        if let Some(id) = self.find_by_digest(&digest)? {
            // Bump the timestamp *and* the sequence, so this lands ahead of
            // everything already recorded. Touching only `copied_at` would
            // leave it behind items copied in the same second, since ties are
            // broken by `seq` (see the ordering note above).
            let next = self.next_seq()?;
            self.conn.execute(
                "UPDATE items SET copied_at = ?1, seq = ?2 WHERE id = ?3",
                params![now, next, id],
            )?;
            return Ok(id);
        }

        let kind = classify(content);
        let preview = build_preview(content, kind);
        let size = content.total_size() as i64;

        let tx = self.conn.transaction()?;
        // Generated here, once, rather than on every listing.
        let thumb = match content.image().and_then(|e| e.bytes.as_deref()) {
            Some(bytes) => match make_thumbnail(bytes) {
                Some(png) => Some(write_blob(&self.blobs, &png)?),
                None => {
                    tracing::debug!("could not render a thumbnail for this image");
                    None
                }
            },
            None => None,
        };

        let seq = next_seq_in(&tx)?;
        tx.execute(
            "INSERT INTO items (copied_at, preview, kind, size, pinned, digest, seq, thumb)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
            params![now, preview, kind.as_str(), size, digest, seq, thumb],
        )?;
        let id = tx.last_insert_rowid();

        for entry in &content.entries {
            let Some(bytes) = &entry.bytes else { continue };

            // Text is small and wanted on every list query, so it is stored
            // inline; binary goes to a file (see module docs).
            if entry.format.is_text() {
                tx.execute(
                    "INSERT OR REPLACE INTO reps (item_id, mime, size, inline, blob)
                     VALUES (?1, ?2, ?3, ?4, NULL)",
                    params![id, entry.format.mime(), bytes.len() as i64, bytes],
                )?;
            } else {
                let name = write_blob(&self.blobs, bytes)?;
                tx.execute(
                    "INSERT OR REPLACE INTO reps (item_id, mime, size, inline, blob)
                     VALUES (?1, ?2, ?3, NULL, ?4)",
                    params![id, entry.format.mime(), bytes.len() as i64, name],
                )?;
            }
        }
        tx.commit()?;

        self.prune()?;
        Ok(id)
    }

    /// The next recency counter value.
    fn next_seq(&self) -> Result<i64> {
        next_seq_in(&self.conn)
    }

    fn find_by_digest(&self, digest: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM items WHERE digest = ?1", params![digest], |r| r.get(0))
            .optional()?)
    }

    /// The most recent items, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, copied_at, preview, kind, size, pinned, thumb
             FROM items ORDER BY pinned DESC, seq DESC LIMIT ?1",
        )?;
        let blobs = self.blobs.clone();
        let rows = stmt.query_map(params![limit as i64], move |row| {
            row_to_item(row, &blobs)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Items whose preview contains `needle`, case-insensitively.
    pub fn search(&self, needle: &str, limit: usize) -> Result<Vec<HistoryItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, copied_at, preview, kind, size, pinned, thumb
             FROM items WHERE preview LIKE ?1 ESCAPE '\\'
             ORDER BY pinned DESC, seq DESC LIMIT ?2",
        )?;
        let pattern = format!("%{}%", escape_like(needle));
        let blobs = self.blobs.clone();
        let rows = stmt.query_map(params![pattern, limit as i64], move |row| {
            row_to_item(row, &blobs)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Rebuild an item's full content, loading blobs back from disk.
    ///
    /// This is what "paste this history entry" needs: every representation
    /// restored, so putting it back on the clipboard is lossless.
    pub fn load(&self, id: i64) -> Result<ClipboardContent> {
        let mut stmt = self
            .conn
            .prepare("SELECT mime, size, inline, blob FROM reps WHERE item_id = ?1")?;

        let rows = stmt.query_map(params![id], |row| {
            let mime: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            let inline: Option<Vec<u8>> = row.get(2)?;
            let blob: Option<String> = row.get(3)?;
            Ok((mime, size, inline, blob))
        })?;

        let mut content = ClipboardContent::default();
        for row in rows {
            let (mime, size, inline, blob) = row?;
            let format = ClipboardFormat::from_mime(&mime);
            match (inline, blob) {
                (Some(bytes), _) => content.insert(ClipboardEntry::loaded(format, bytes)),
                (None, Some(name)) => {
                    let path = self.blobs.join(&name);
                    match std::fs::read(&path) {
                        Ok(bytes) => content.insert(ClipboardEntry::loaded(format, bytes)),
                        // A blob deleted from under us shouldn't sink the whole
                        // paste — the remaining representations are still good.
                        Err(e) => tracing::warn!(
                            blob = %name, error = %e,
                            "history blob is missing; skipping that representation"
                        ),
                    }
                }
                (None, None) => {
                    // Size known but no payload: keep it visible as a lazy entry
                    // rather than pretending the representation never existed.
                    content.insert(ClipboardEntry::lazy(format, size as u64));
                }
            }
        }

        if content.is_empty() {
            anyhow::bail!("history item {id} has no readable content");
        }
        Ok(content)
    }

    /// Pin or unpin an item, exempting it from eviction.
    pub fn set_pinned(&self, id: i64, pinned: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET pinned = ?1 WHERE id = ?2",
            params![pinned as i64, id],
        )?;
        Ok(())
    }

    /// Delete one item.
    pub fn remove(&mut self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM reps WHERE item_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM items WHERE id = ?1", params![id])?;
        self.collect_orphan_blobs()
    }

    /// Delete everything except pinned items.
    pub fn clear(&mut self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM reps WHERE item_id IN (SELECT id FROM items WHERE pinned = 0)",
            [],
        )?;
        self.conn.execute("DELETE FROM items WHERE pinned = 0", [])?;
        self.collect_orphan_blobs()
    }

    /// Enforce the count and byte limits, dropping the oldest unpinned items.
    ///
    /// Both limits matter: see the module docs on why either alone is
    /// insufficient. Pinned items are never evicted, which does mean a user who
    /// pins more than the limit keeps them all — deliberate, since pinning is an
    /// explicit "keep this" instruction.
    pub fn prune(&mut self) -> Result<()> {
        // Count limit.
        self.conn.execute(
            "DELETE FROM items WHERE pinned = 0 AND id NOT IN (
                 SELECT id FROM items WHERE pinned = 0
                 ORDER BY seq DESC LIMIT ?1
             )",
            params![self.max_items as i64],
        )?;

        // Byte limit: drop oldest unpinned until the total fits.
        loop {
            let total: i64 = self
                .conn
                .query_row("SELECT COALESCE(SUM(size), 0) FROM items", [], |r| r.get(0))?;
            if (total as u64) <= self.max_bytes {
                break;
            }
            let oldest: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM items WHERE pinned = 0 ORDER BY seq ASC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            match oldest {
                Some(id) => {
                    self.conn.execute("DELETE FROM items WHERE id = ?1", params![id])?;
                }
                // Only pinned items left: they are exempt, so stop rather than
                // loop forever.
                None => break,
            }
        }

        // Rows in `reps` for deleted items go too (no FK enforcement by default).
        self.conn.execute(
            "DELETE FROM reps WHERE item_id NOT IN (SELECT id FROM items)",
            [],
        )?;
        self.collect_orphan_blobs()
    }

    /// Delete blob files no row references any more.
    ///
    /// Blobs are shared by digest, so a file may outlive the row that created
    /// it; only a file with no referrers at all is safe to remove.
    fn collect_orphan_blobs(&self) -> Result<()> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT blob FROM reps WHERE blob IS NOT NULL")?;
        let mut referenced: std::collections::HashSet<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;

        // Thumbnails live in the same directory and are referenced from `items`,
        // not `reps` — missing them here would delete every preview.
        let mut thumbs = self.conn.prepare("SELECT DISTINCT thumb FROM items WHERE thumb IS NOT NULL")?;
        for name in thumbs.query_map([], |r| r.get::<_, String>(0))? {
            referenced.insert(name?);
        }

        for entry in std::fs::read_dir(&self.blobs)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !referenced.contains(&name) {
                if let Err(e) = std::fs::remove_file(entry.path()) {
                    tracing::warn!(blob = %name, error = %e, "could not delete orphaned blob");
                }
            }
        }
        Ok(())
    }

    /// Total number of items and bytes held.
    pub fn stats(&self) -> Result<(usize, u64)> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?;
        let bytes: i64 = self
            .conn
            .query_row("SELECT COALESCE(SUM(size), 0) FROM items", [], |r| r.get(0))?;
        Ok((count as usize, bytes as u64))
    }
}

/// The next value of the recency counter.
///
/// Derived from the current maximum rather than kept in a separate row, so it
/// stays correct after eviction and needs no extra bookkeeping.
fn next_seq_in(conn: &Connection) -> Result<i64> {
    let max: i64 = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM items", [], |r| r.get(0))?;
    Ok(max + 1)
}

fn row_to_item(row: &rusqlite::Row<'_>, blobs: &Path) -> rusqlite::Result<HistoryItem> {
    let thumb: Option<String> = row.get(6)?;
    Ok(HistoryItem {
        id: row.get(0)?,
        copied_at: row.get(1)?,
        preview: row.get(2)?,
        kind: ItemKind::from_str(&row.get::<_, String>(3)?),
        size: row.get::<_, i64>(4)? as u64,
        pinned: row.get::<_, i64>(5)? != 0,
        thumbnail: thumb.map(|name| blobs.join(name)),
    })
}

/// Render a small PNG preview of an image, preserving aspect ratio.
///
/// Returns `None` rather than failing when the bytes don't decode: a thumbnail
/// is a nicety, and an item with no preview is far better than a copy that
/// couldn't be recorded at all.
fn make_thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?;
    let thumb = image.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE);
    let mut png = Vec::new();
    thumb
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// Write `bytes` to a content-addressed file, returning its name.
///
/// Idempotent: an existing file with the same digest is the same content, so
/// nothing is rewritten.
fn write_blob(dir: &Path, bytes: &[u8]) -> Result<String> {
    let name = hex_digest(bytes);
    let path = dir.join(&name);
    if !path.exists() {
        std::fs::write(&path, bytes)
            .with_context(|| format!("writing blob {}", path.display()))?;
    }
    Ok(name)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// A digest over the whole item, used to detect a repeat copy.
///
/// Formats are folded in sorted order so the same content hashes identically
/// regardless of the order representations happened to be read in.
fn content_digest(content: &ClipboardContent) -> String {
    let mut parts: Vec<(&str, &[u8])> = content
        .entries
        .iter()
        .filter_map(|e| e.bytes.as_deref().map(|b| (e.format.mime(), b)))
        .collect();
    parts.sort_by_key(|(mime, _)| *mime);

    let mut hasher = Sha256::new();
    for (mime, bytes) in parts {
        hasher.update(mime.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn classify(content: &ClipboardContent) -> ItemKind {
    if content.image().is_some() {
        ItemKind::Image
    } else if content.has(&ClipboardFormat::UriList) {
        ItemKind::Files
    } else {
        ItemKind::Text
    }
}

/// A one-line label for the picker.
///
/// Newlines and tabs become spaces because the picker renders one item per
/// line — a multi-line copy would otherwise break the list apart.
fn build_preview(content: &ClipboardContent, kind: ItemKind) -> String {
    match kind {
        ItemKind::Image => {
            let size = content.image().map(|e| e.size).unwrap_or(0);
            format!("[image, {}]", human_bytes(size))
        }
        ItemKind::Files => content
            .bytes(&ClipboardFormat::UriList)
            .map(|b| {
                let text = String::from_utf8_lossy(b);
                let count = text.lines().filter(|l| !l.trim().is_empty()).count();
                let first = text.lines().next().unwrap_or("").trim().to_string();
                if count > 1 {
                    format!("[{count} files] {first}")
                } else {
                    format!("[file] {first}")
                }
            })
            .unwrap_or_else(|| "[files]".to_string()),
        ItemKind::Text => {
            let text = content.as_text().unwrap_or("");
            let flat: String = text
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            let trimmed = flat.trim();
            let mut preview: String = trimmed.chars().take(PREVIEW_CHARS).collect();
            if trimmed.chars().count() > PREVIEW_CHARS {
                preview.push('…');
            }
            if preview.is_empty() {
                "[empty]".to_string()
            } else {
                preview
            }
        }
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Escape LIKE wildcards so a search for "50%" doesn't match everything.
fn escape_like(needle: &str) -> String {
    needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (store, dir)
    }

    fn png(size: usize) -> ClipboardContent {
        ClipboardContent::single(ClipboardFormat::ImagePng, vec![7u8; size])
    }

    /// A real, decodable PNG of the given dimensions.
    fn real_png(width: u32, height: u32) -> ClipboardContent {
        let buffer = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 120, 220, 255]));
        let mut bytes = Vec::new();
        buffer
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        ClipboardContent::single(ClipboardFormat::ImagePng, bytes)
    }

    #[test]
    fn text_roundtrips_through_history() {
        let (mut s, _d) = store();
        let id = s.insert(&ClipboardContent::text("hello")).unwrap();
        assert_eq!(s.load(id).unwrap().as_text(), Some("hello"));
    }

    #[test]
    fn image_bytes_live_on_disk_not_in_the_row() {
        // The core storage decision: a listing must not drag image bytes
        // through SQLite (see module docs).
        let (mut s, dir) = store();
        s.insert(&png(50_000)).unwrap();

        let blobs: Vec<_> = std::fs::read_dir(dir.path().join("blobs")).unwrap().collect();
        assert_eq!(blobs.len(), 1, "image should be stored as one blob file");

        let db_len = std::fs::metadata(dir.path().join("history.db")).unwrap().len();
        assert!(db_len < 50_000, "database grew to {db_len}; image leaked into it");
    }

    #[test]
    fn image_roundtrips_byte_for_byte() {
        let (mut s, _d) = store();
        let original = png(1234);
        let id = s.insert(&original).unwrap();
        let back = s.load(id).unwrap();
        assert_eq!(
            back.bytes(&ClipboardFormat::ImagePng),
            original.bytes(&ClipboardFormat::ImagePng)
        );
    }

    #[test]
    fn recopying_bumps_instead_of_duplicating() {
        // Re-copying the same thing is extremely common; a history that
        // recorded each one would be a wall of repeats.
        let (mut s, _d) = store();
        let first = s.insert(&ClipboardContent::text("same")).unwrap();
        s.insert(&ClipboardContent::text("other")).unwrap();
        let again = s.insert(&ClipboardContent::text("same")).unwrap();

        assert_eq!(first, again, "should reuse the existing row");
        assert_eq!(s.stats().unwrap().0, 2);
        assert_eq!(s.recent(10).unwrap()[0].id, first, "should be back on top");
    }

    #[test]
    fn identical_images_share_one_blob() {
        let (mut s, dir) = store();
        s.insert(&png(4096)).unwrap();
        s.insert(&ClipboardContent::text("something else")).unwrap();
        s.insert(&png(4096)).unwrap();

        let blobs = std::fs::read_dir(dir.path().join("blobs")).unwrap().count();
        assert_eq!(blobs, 1, "identical content must be stored once");
    }

    #[test]
    fn count_limit_evicts_oldest() {
        let (mut s, _d) = store();
        let mut s = std::mem::replace(&mut s, Store::open(_d.path()).unwrap()).with_limits(3, u64::MAX);
        for i in 0..5 {
            s.insert(&ClipboardContent::text(format!("item {i}"))).unwrap();
        }
        let items = s.recent(10).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].preview, "item 4", "newest first");
    }

    #[test]
    fn byte_limit_evicts_and_frees_blobs() {
        // A byte limit is what stops a handful of screenshots filling a disk.
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap().with_limits(usize::MAX, 20_000);
        for i in 0..5 {
            // Distinct content so each gets its own blob.
            let mut bytes = vec![0u8; 10_000];
            bytes[0] = i;
            s.insert(&ClipboardContent::single(ClipboardFormat::ImagePng, bytes)).unwrap();
        }
        let (count, total) = s.stats().unwrap();
        assert!(total <= 20_000, "total {total} exceeds the limit");
        assert!(count < 5, "should have evicted something");

        let blobs = std::fs::read_dir(dir.path().join("blobs")).unwrap().count();
        assert_eq!(blobs, count, "evicted blobs must be deleted from disk");
    }

    #[test]
    fn pinned_items_survive_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap().with_limits(2, u64::MAX);
        let keep = s.insert(&ClipboardContent::text("important")).unwrap();
        s.set_pinned(keep, true).unwrap();

        for i in 0..5 {
            s.insert(&ClipboardContent::text(format!("noise {i}"))).unwrap();
        }
        assert!(s.load(keep).is_ok(), "pinned item was evicted");
    }

    #[test]
    fn clear_keeps_pinned() {
        let (mut s, _d) = store();
        let pinned = s.insert(&ClipboardContent::text("keep")).unwrap();
        s.set_pinned(pinned, true).unwrap();
        s.insert(&ClipboardContent::text("drop")).unwrap();

        s.clear().unwrap();
        let items = s.recent(10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, pinned);
    }

    #[test]
    fn search_matches_substring_case_insensitively() {
        let (mut s, _d) = store();
        s.insert(&ClipboardContent::text("Hello World")).unwrap();
        s.insert(&ClipboardContent::text("unrelated")).unwrap();

        assert_eq!(s.search("world", 10).unwrap().len(), 1);
        assert_eq!(s.search("HELLO", 10).unwrap().len(), 1);
        assert_eq!(s.search("nothing", 10).unwrap().len(), 0);
    }

    #[test]
    fn search_treats_wildcards_literally() {
        // Unescaped, "%" would match every row.
        let (mut s, _d) = store();
        s.insert(&ClipboardContent::text("50% off")).unwrap();
        s.insert(&ClipboardContent::text("nothing special")).unwrap();

        assert_eq!(s.search("%", 10).unwrap().len(), 1);
        assert_eq!(s.search("50%", 10).unwrap().len(), 1);
    }

    #[test]
    fn multiline_preview_is_flattened_to_one_line() {
        // The picker renders one item per line, so a preview must not contain
        // newlines or the list breaks apart.
        let (mut s, _d) = store();
        let id = s.insert(&ClipboardContent::text("first\nsecond\tthird")).unwrap();
        let item = s.recent(1).unwrap().into_iter().next().unwrap();
        assert_eq!(item.id, id);
        assert!(!item.preview.contains('\n') && !item.preview.contains('\t'));
        assert!(item.preview.contains("first") && item.preview.contains("second"));
    }

    #[test]
    fn long_text_preview_is_truncated_but_content_is_not() {
        let (mut s, _d) = store();
        let long = "x".repeat(500);
        let id = s.insert(&ClipboardContent::text(long.clone())).unwrap();

        let item = s.recent(1).unwrap().into_iter().next().unwrap();
        assert!(item.preview.chars().count() <= PREVIEW_CHARS + 1);
        assert_eq!(s.load(id).unwrap().as_text(), Some(long.as_str()), "full text must survive");
    }

    #[test]
    fn image_preview_reports_size() {
        let (mut s, _d) = store();
        s.insert(&png(2048)).unwrap();
        let item = s.recent(1).unwrap().into_iter().next().unwrap();
        assert_eq!(item.kind, ItemKind::Image);
        assert!(item.preview.contains("2.0 KiB"), "got {:?}", item.preview);
    }

    #[test]
    fn missing_blob_degrades_instead_of_failing_the_load() {
        // A blob deleted underneath us shouldn't make the whole paste fail
        // when other representations are still intact.
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let mut content = ClipboardContent::text("caption");
        content.insert(ClipboardEntry::loaded(ClipboardFormat::ImagePng, vec![1, 2, 3]));
        let id = s.insert(&content).unwrap();

        for blob in std::fs::read_dir(dir.path().join("blobs")).unwrap() {
            std::fs::remove_file(blob.unwrap().path()).unwrap();
        }

        let back = s.load(id).unwrap();
        assert_eq!(back.as_text(), Some("caption"), "text should still load");
    }

    #[test]
    fn multi_format_item_keeps_every_representation() {
        let (mut s, _d) = store();
        let mut content = ClipboardContent::text("plain");
        content.insert(ClipboardEntry::loaded(ClipboardFormat::Html, b"<b>rich</b>".to_vec()));
        let id = s.insert(&content).unwrap();

        let back = s.load(id).unwrap();
        assert_eq!(back.as_text(), Some("plain"));
        assert_eq!(back.bytes(&ClipboardFormat::Html), Some(&b"<b>rich</b>"[..]));
    }

    #[test]
    fn digest_is_order_independent() {
        // Representations may be read in any order; the same item must not be
        // recorded twice because of it.
        let mut a = ClipboardContent::text("body");
        a.insert(ClipboardEntry::loaded(ClipboardFormat::Html, b"<p>body</p>".to_vec()));

        let mut b = ClipboardContent::single(ClipboardFormat::Html, b"<p>body</p>".to_vec());
        b.insert(ClipboardEntry::loaded(ClipboardFormat::Text, b"body".to_vec()));

        assert_eq!(content_digest(&a), content_digest(&b));
    }

    #[test]
    fn rapid_copies_within_one_second_keep_their_order() {
        // Regression: `copied_at` has one-second resolution, so a burst of
        // copies all share a timestamp. Ordering by it alone let SQLite return
        // ties arbitrarily, and eviction kept the *oldest* items.
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap().with_limits(3, u64::MAX);
        for i in 0..6 {
            s.insert(&ClipboardContent::text(format!("item {i}"))).unwrap();
        }

        let items = s.recent(10).unwrap();
        assert_eq!(items.len(), 3);
        let previews: Vec<_> = items.iter().map(|i| i.preview.as_str()).collect();
        assert_eq!(previews, vec!["item 5", "item 4", "item 3"]);
    }

    #[test]
    fn recopy_moves_to_the_front_even_within_one_second() {
        // The same tie-breaking problem, seen through the bump path: touching
        // only the timestamp left a re-copied item behind its same-second peers.
        let (mut s, _d) = store();
        let first = s.insert(&ClipboardContent::text("alpha")).unwrap();
        s.insert(&ClipboardContent::text("beta")).unwrap();
        s.insert(&ClipboardContent::text("gamma")).unwrap();
        s.insert(&ClipboardContent::text("alpha")).unwrap();

        assert_eq!(s.recent(1).unwrap()[0].id, first);
    }

    #[test]
    fn images_get_a_thumbnail() {
        let (mut s, _d) = store();
        s.insert(&real_png(800, 600)).unwrap();

        let item = s.recent(1).unwrap().into_iter().next().unwrap();
        let thumb = item.thumbnail.expect("image item should have a thumbnail");
        assert!(thumb.exists(), "thumbnail file should be on disk");

        // It must actually be a smaller image, or it saves the picker nothing.
        // Loaded from memory, not by path: blobs are named by digest and have
        // no extension for `image::open` to sniff.
        let decoded = image::load_from_memory(&std::fs::read(&thumb).unwrap()).unwrap();
        assert!(decoded.width() <= THUMB_MAX_EDGE && decoded.height() <= THUMB_MAX_EDGE);
        assert_eq!(decoded.width() * 3, decoded.height() * 4, "aspect ratio should hold");
    }

    #[test]
    fn thumbnail_is_far_smaller_than_the_original() {
        // The whole reason thumbnails exist: opening the popup must not decode
        // megabytes per row.
        let (mut s, _d) = store();
        let original = real_png(1920, 1080);
        let original_len = original.image().unwrap().size;
        s.insert(&original).unwrap();

        let item = s.recent(1).unwrap().into_iter().next().unwrap();
        let thumb_len = std::fs::metadata(item.thumbnail.unwrap()).unwrap().len();
        assert!(thumb_len < original_len, "thumb {thumb_len} vs original {original_len}");
    }

    #[test]
    fn text_items_have_no_thumbnail() {
        let (mut s, _d) = store();
        s.insert(&ClipboardContent::text("just words")).unwrap();
        assert!(s.recent(1).unwrap()[0].thumbnail.is_none());
    }

    #[test]
    fn undecodable_image_is_still_recorded_without_a_thumbnail() {
        // A thumbnail is a nicety; failing to make one must not lose the copy.
        let (mut s, _d) = store();
        let id = s.insert(&png(64)).unwrap(); // not valid image data
        let item = s.recent(1).unwrap().into_iter().next().unwrap();
        assert_eq!(item.id, id);
        assert!(item.thumbnail.is_none());
        assert!(s.load(id).is_ok(), "content must still be retrievable");
    }

    #[test]
    fn pruning_keeps_thumbnails_of_surviving_items() {
        // Regression: thumbnails are referenced from `items`, not `reps`, so a
        // garbage collector that only consulted `reps` deleted every preview.
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap().with_limits(3, u64::MAX);
        for i in 0..5 {
            s.insert(&real_png(40 + i, 30)).unwrap();
        }
        s.prune().unwrap();

        for item in s.recent(10).unwrap() {
            let thumb = item.thumbnail.expect("surviving image should keep its thumbnail");
            assert!(thumb.exists(), "thumbnail {} was garbage-collected", thumb.display());
        }
    }

    #[test]
    fn an_older_database_gains_new_columns() {
        // Regression: `CREATE TABLE IF NOT EXISTS` leaves an existing table
        // alone, so a database made before `thumb` existed kept its old shape
        // and every listing query failed with "no such column".
        let dir = tempfile::tempdir().unwrap();

        // Build a database the way the previous version would have.
        {
            let conn = Connection::open(dir.path().join("history.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE items (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     copied_at INTEGER NOT NULL,
                     preview TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     size INTEGER NOT NULL,
                     pinned INTEGER NOT NULL DEFAULT 0,
                     seq INTEGER NOT NULL DEFAULT 0,
                     digest TEXT NOT NULL
                 );
                 INSERT INTO items (copied_at, preview, kind, size, digest, seq)
                 VALUES (1, 'from the old version', 'text', 5, 'abc', 1);",
            )
            .unwrap();
        }

        // Opening it must migrate rather than fail, and keep what was there.
        let mut store = Store::open(dir.path()).unwrap();
        let items = store.recent(10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].preview, "from the old version");
        assert!(items[0].thumbnail.is_none());

        // And the migrated database must still accept new items.
        store.insert(&ClipboardContent::text("after upgrade")).unwrap();
        assert_eq!(store.recent(10).unwrap()[0].preview, "after upgrade");
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path()).unwrap();
        // Re-opening must not try to add the column a second time.
        let store = Store::open(dir.path()).unwrap();
        assert!(store.recent(1).is_ok());
    }

    #[test]
    fn history_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut s = Store::open(dir.path()).unwrap();
            s.insert(&ClipboardContent::text("persisted")).unwrap();
        }
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.recent(10).unwrap()[0].preview, "persisted");
    }
}
