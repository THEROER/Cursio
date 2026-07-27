//! What clipboard content *is*: formats and the set of representations one
//! copy carries.
//!
//! This is the shared vocabulary every other part of the system speaks — the
//! platform backends that read and write the real clipboard, the history store
//! that persists entries, and (in Cursio) the code that ships content to
//! another machine. It deliberately knows nothing about any of them.
//!
//! The shape here follows from one fact: **a clipboard is not a string, it is a
//! set of representations of the same thing.** Copying from a word processor
//! puts HTML, RTF, and plain text on the clipboard at once; copying an image
//! offers PNG and often a raw bitmap too. Whoever pastes picks whichever they
//! understand best — a browser takes the HTML, a terminal takes the text.
//! Collapsing that set to a single "the content" at capture time silently
//! destroys formatting and can't be undone later, so a [`ClipboardContent`]
//! keeps every representation and lets the consumer choose (see
//! [`best_match`](ClipboardContent::best_match)).
//!
//! Both platforms' native APIs already work this way, so preserving the set is
//! also the least surprising thing to do.

use serde::{Deserialize, Serialize};

/// A concrete representation of clipboard content, i.e. one MIME type.
///
/// The clipboard is format-plural (see the module docs), so content is
/// described by *which* representations exist, not by a single "kind". We name
/// the common ones as variants — they need consistent cross-platform mapping,
/// since the two OSes disagree on how to identify formats (Windows uses numeric
/// `CF_*` constants, Wayland/X11 use MIME strings) and this enum is where that
/// disagreement is normalised — and keep [`Other`](ClipboardFormat::Other) as
/// the escape hatch so an app-specific type is still carried verbatim, and
/// round-trips between two applications that understand it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClipboardFormat {
    /// UTF-8 plain text (`text/plain;charset=utf-8`, Win32 `CF_UNICODETEXT`).
    /// The universal fallback: if a receiver understands nothing else, it
    /// understands this, so a source that has any textual representation should
    /// always offer it alongside the richer ones.
    Text,
    /// HTML markup (`text/html`, Win32 `CF_HTML`). Carries formatting when
    /// pasting between rich editors and browsers.
    ///
    /// Note that Win32 `CF_HTML` wraps the markup in a header with byte offsets
    /// while every other platform uses bare markup; the canonical form here is
    /// **bare markup**, and the Windows backend adds/strips that header. Doing
    /// it at the edge keeps the fixup in one place instead of leaking a
    /// Windows-ism into the shared vocabulary.
    Html,
    /// Rich Text Format (`text/rtf`). What word processors negotiate with when
    /// HTML isn't on offer.
    Rtf,
    /// PNG image data (`image/png`). The default image interchange format:
    /// lossless, universally understood, and far smaller than raw bitmaps.
    /// Screenshots and "copy image" from a browser land here.
    ImagePng,
    /// JPEG image data (`image/jpeg`). Already-compressed photos, kept in their
    /// original encoding rather than re-encoded to PNG (which would usually
    /// make them *larger* and lose nothing worth gaining).
    ImageJpeg,
    /// A list of files (`text/uri-list`, Win32 `CF_HDROP`), as URIs.
    ///
    /// Note the URIs name files on the machine that copied them. That is fine
    /// locally, but when content moves between machines those paths generally
    /// do not exist on the other side; transferring file *contents* is a
    /// separate feature. See the design doc.
    UriList,
    /// Any other type, by its MIME string — app-specific formats
    /// (`application/x-…`) carried verbatim without being interpreted.
    Other(String),
}

impl ClipboardFormat {
    /// The MIME type string for this format, as used by Wayland and X11.
    ///
    /// The Windows backend maps to/from `CF_*` constants separately; this is
    /// the canonical spelling everywhere else.
    pub fn mime(&self) -> &str {
        match self {
            ClipboardFormat::Text => "text/plain;charset=utf-8",
            ClipboardFormat::Html => "text/html",
            ClipboardFormat::Rtf => "text/rtf",
            ClipboardFormat::ImagePng => "image/png",
            ClipboardFormat::ImageJpeg => "image/jpeg",
            ClipboardFormat::UriList => "text/uri-list",
            ClipboardFormat::Other(mime) => mime,
        }
    }

    /// Parse a MIME type into a format, recognising the well-known spellings.
    ///
    /// Tolerates the variations real toolkits emit — `text/plain` with or
    /// without a charset parameter, `image/jpg` for JPEG — so an offer from a
    /// slightly-off source still maps onto a known variant instead of falling
    /// into [`Other`](ClipboardFormat::Other), where it would likely fail to
    /// match another source's spelling of the very same thing.
    pub fn from_mime(mime: &str) -> Self {
        let normalised = mime.trim().to_ascii_lowercase();
        let base = normalised
            .split(';')
            .next()
            .unwrap_or(&normalised)
            .trim_end();
        match base {
            "text/plain" | "text" | "utf8_string" | "string" => ClipboardFormat::Text,
            "text/html" => ClipboardFormat::Html,
            "text/rtf" | "application/rtf" => ClipboardFormat::Rtf,
            "image/png" => ClipboardFormat::ImagePng,
            "image/jpeg" | "image/jpg" => ClipboardFormat::ImageJpeg,
            "text/uri-list" => ClipboardFormat::UriList,
            _ => ClipboardFormat::Other(mime.to_string()),
        }
    }

    /// Whether this format is textual (as opposed to binary like an image).
    ///
    /// Backends use this to decide whether a payload needs a text-encoding
    /// conversion on the way in or out — notably Windows, whose text clipboard
    /// formats are UTF-16 while the canonical form here is always UTF-8. The
    /// history store also uses it to decide what can be shown as a preview.
    pub fn is_text(&self) -> bool {
        matches!(
            self,
            ClipboardFormat::Text
                | ClipboardFormat::Html
                | ClipboardFormat::Rtf
                | ClipboardFormat::UriList
        )
    }

    /// Whether this format carries an image.
    pub fn is_image(&self) -> bool {
        matches!(self, ClipboardFormat::ImagePng | ClipboardFormat::ImageJpeg)
    }

    /// Rough preference score — higher is richer/more faithful.
    ///
    /// Used by [`ClipboardContent::best_match`] to pick among the formats a
    /// consumer accepts. The ordering encodes "lose as little as possible":
    /// rich text beats plain text, and an image beats a textual description of
    /// one. It is only a tiebreaker between formats the consumer already
    /// accepts, so a caller wanting different priorities can ignore it and
    /// choose from [`ClipboardContent::formats`] itself.
    pub fn richness(&self) -> u8 {
        match self {
            ClipboardFormat::ImagePng => 60,
            ClipboardFormat::ImageJpeg => 55,
            ClipboardFormat::Html => 50,
            ClipboardFormat::Rtf => 45,
            ClipboardFormat::UriList => 30,
            ClipboardFormat::Other(_) => 20,
            ClipboardFormat::Text => 10,
        }
    }
}

/// How big a representation is, and where its bytes are.
///
/// Reading every representation eagerly is the wrong default: a single copy can
/// offer the same image as PNG *and* as an uncompressed bitmap, and a clipboard
/// watcher that slurps all of it on every copy would burn hundreds of megabytes
/// of I/O for content the user may never paste. So an entry always knows its
/// `size` — cheap to learn — while `bytes` is filled in only once someone
/// actually wants that representation.
///
/// Both native APIs are lazy in exactly this way (Wayland hands over an fd to
/// read, Win32 supports delayed rendering), so this mirrors the OS rather than
/// fighting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardEntry {
    /// Which representation this is.
    pub format: ClipboardFormat,
    /// Size of the payload in bytes, known even when `bytes` is `None`.
    pub size: u64,
    /// The payload, once loaded. `None` means "not fetched yet", *not* "empty" —
    /// use [`is_loaded`](ClipboardEntry::is_loaded) rather than testing for an
    /// empty slice, since a genuinely empty payload is possible.
    pub bytes: Option<Vec<u8>>,
}

impl ClipboardEntry {
    /// An entry whose payload is already in hand.
    pub fn loaded(format: ClipboardFormat, bytes: Vec<u8>) -> Self {
        ClipboardEntry { format, size: bytes.len() as u64, bytes: Some(bytes) }
    }

    /// An entry that is known to exist at `size` bytes but not yet fetched.
    pub fn lazy(format: ClipboardFormat, size: u64) -> Self {
        ClipboardEntry { format, size, bytes: None }
    }

    /// Whether the payload has been fetched.
    pub fn is_loaded(&self) -> bool {
        self.bytes.is_some()
    }
}

/// One clipboard item: every representation of a single copy.
///
/// This is the unit the whole system passes around — what a backend returns
/// when it reads the clipboard, what the history stores as one row, and what
/// gets written back on paste. Keeping the representations together is the
/// point (see the module docs): the choice of which one to use belongs to
/// whoever consumes it, not to whoever captured it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClipboardContent {
    /// The available representations. Order is not significant; use
    /// [`best_match`](ClipboardContent::best_match) to choose.
    pub entries: Vec<ClipboardEntry>,
}

impl ClipboardContent {
    /// Content consisting of a single representation.
    pub fn single(format: ClipboardFormat, bytes: Vec<u8>) -> Self {
        ClipboardContent { entries: vec![ClipboardEntry::loaded(format, bytes)] }
    }

    /// Plain UTF-8 text — the common case, spelled directly.
    pub fn text(text: impl Into<String>) -> Self {
        Self::single(ClipboardFormat::Text, text.into().into_bytes())
    }

    /// Whether there are no representations at all (an empty clipboard).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The formats present, without their payloads.
    pub fn formats(&self) -> impl Iterator<Item = &ClipboardFormat> {
        self.entries.iter().map(|e| &e.format)
    }

    /// Whether a given format is present.
    pub fn has(&self, format: &ClipboardFormat) -> bool {
        self.entries.iter().any(|e| &e.format == format)
    }

    /// The entry for a format, if present.
    pub fn entry(&self, format: &ClipboardFormat) -> Option<&ClipboardEntry> {
        self.entries.iter().find(|e| &e.format == format)
    }

    /// The loaded payload for a format. `None` if absent *or* not yet fetched.
    pub fn bytes(&self, format: &ClipboardFormat) -> Option<&[u8]> {
        self.entry(format).and_then(|e| e.bytes.as_deref())
    }

    /// The plain-text representation as a string, if present, loaded, and valid
    /// UTF-8. The convenience path for previews, search, and "just give me the
    /// text".
    pub fn as_text(&self) -> Option<&str> {
        self.bytes(&ClipboardFormat::Text)
            .and_then(|b| std::str::from_utf8(b).ok())
    }

    /// The richest image representation present, if any.
    pub fn image(&self) -> Option<&ClipboardEntry> {
        self.entries
            .iter()
            .filter(|e| e.format.is_image())
            .max_by_key(|e| e.format.richness())
    }

    /// Total size across all representations.
    ///
    /// This — not the size of any one entry — is what a storage quota must be
    /// measured against, since a copy that offers PNG *and* a bitmap costs both.
    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    /// Pick the richest present format that the consumer also accepts.
    ///
    /// The negotiation half of the format-plural model: the source keeps
    /// everything, the consumer states what it can handle, and the best common
    /// representation wins — so pasting into a rich editor keeps formatting
    /// while pasting into a terminal still works. `None` means no overlap,
    /// which callers should treat as "nothing usable here", not an error.
    pub fn best_match(&self, accepted: &[ClipboardFormat]) -> Option<&ClipboardFormat> {
        self.entries
            .iter()
            .filter(|e| accepted.contains(&e.format))
            .max_by_key(|e| e.format.richness())
            .map(|e| &e.format)
    }

    /// Add or replace a representation.
    pub fn insert(&mut self, entry: ClipboardEntry) {
        match self.entries.iter_mut().find(|e| e.format == entry.format) {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_roundtrips_for_known_formats() {
        for format in [
            ClipboardFormat::Text,
            ClipboardFormat::Html,
            ClipboardFormat::Rtf,
            ClipboardFormat::ImagePng,
            ClipboardFormat::ImageJpeg,
            ClipboardFormat::UriList,
        ] {
            assert_eq!(ClipboardFormat::from_mime(format.mime()), format);
        }
    }

    #[test]
    fn from_mime_tolerates_real_world_spellings() {
        assert_eq!(ClipboardFormat::from_mime("text/plain"), ClipboardFormat::Text);
        assert_eq!(
            ClipboardFormat::from_mime("TEXT/PLAIN;charset=utf-8"),
            ClipboardFormat::Text
        );
        assert_eq!(
            ClipboardFormat::from_mime("image/jpg"),
            ClipboardFormat::ImageJpeg
        );
    }

    #[test]
    fn unknown_mime_becomes_other_preserving_original() {
        let format = ClipboardFormat::from_mime("application/x-kde-cutselection");
        assert_eq!(
            format,
            ClipboardFormat::Other("application/x-kde-cutselection".to_string())
        );
        // `Other` must keep the exact string so it maps back onto another
        // application's spelling of the same app-specific type.
        assert_eq!(format.mime(), "application/x-kde-cutselection");
    }

    #[test]
    fn best_match_prefers_richer_format() {
        let content = ClipboardContent {
            entries: vec![
                ClipboardEntry::loaded(ClipboardFormat::Text, b"hello".to_vec()),
                ClipboardEntry::loaded(ClipboardFormat::Html, b"<b>hello</b>".to_vec()),
            ],
        };
        // A consumer that takes both should get the formatted version.
        assert_eq!(
            content.best_match(&[ClipboardFormat::Text, ClipboardFormat::Html]),
            Some(&ClipboardFormat::Html)
        );
        // One that only takes plain text still gets something pasteable.
        assert_eq!(
            content.best_match(&[ClipboardFormat::Text]),
            Some(&ClipboardFormat::Text)
        );
    }

    #[test]
    fn best_match_is_none_without_overlap() {
        let content = ClipboardContent::single(ClipboardFormat::ImagePng, vec![0x89, b'P']);
        assert_eq!(content.best_match(&[ClipboardFormat::Text]), None);
    }

    #[test]
    fn lazy_entry_knows_size_without_payload() {
        // The whole point of laziness: a 48 MiB image is describable without
        // being read, and "not fetched" is distinguishable from "empty".
        let content = ClipboardContent {
            entries: vec![ClipboardEntry::lazy(ClipboardFormat::ImagePng, 48 * 1024 * 1024)],
        };
        let entry = content.entry(&ClipboardFormat::ImagePng).unwrap();
        assert_eq!(entry.size, 48 * 1024 * 1024);
        assert!(!entry.is_loaded());
        assert_eq!(content.bytes(&ClipboardFormat::ImagePng), None);
    }

    #[test]
    fn empty_payload_is_loaded_not_missing() {
        let content = ClipboardContent::single(ClipboardFormat::Text, Vec::new());
        let entry = content.entry(&ClipboardFormat::Text).unwrap();
        assert!(entry.is_loaded());
        assert_eq!(content.bytes(&ClipboardFormat::Text), Some(&[][..]));
    }

    #[test]
    fn total_size_counts_every_representation() {
        // A quota must charge for all representations, not just the richest:
        // one copy offering PNG and a bitmap costs both.
        let content = ClipboardContent {
            entries: vec![
                ClipboardEntry::lazy(ClipboardFormat::ImagePng, 1_000),
                ClipboardEntry::lazy(ClipboardFormat::Other("image/bmp".into()), 9_000),
            ],
        };
        assert_eq!(content.total_size(), 10_000);
    }

    #[test]
    fn insert_replaces_existing_format() {
        let mut content = ClipboardContent::text("old");
        content.insert(ClipboardEntry::loaded(ClipboardFormat::Text, b"new".to_vec()));
        assert_eq!(content.entries.len(), 1);
        assert_eq!(content.as_text(), Some("new"));
    }

    #[test]
    fn as_text_ignores_non_utf8_and_absent_text() {
        assert_eq!(ClipboardContent::text("hi").as_text(), Some("hi"));
        // Invalid UTF-8 must not panic or produce garbage.
        let bad = ClipboardContent::single(ClipboardFormat::Text, vec![0xff, 0xfe]);
        assert_eq!(bad.as_text(), None);
        // No text representation at all.
        let png = ClipboardContent::single(ClipboardFormat::ImagePng, vec![0x89]);
        assert_eq!(png.as_text(), None);
    }

    #[test]
    fn image_picks_richest_of_several() {
        let content = ClipboardContent {
            entries: vec![
                ClipboardEntry::lazy(ClipboardFormat::ImageJpeg, 10),
                ClipboardEntry::lazy(ClipboardFormat::ImagePng, 20),
                ClipboardEntry::loaded(ClipboardFormat::Text, b"caption".to_vec()),
            ],
        };
        assert_eq!(content.image().unwrap().format, ClipboardFormat::ImagePng);
        assert!(ClipboardContent::text("no image").image().is_none());
    }

    #[test]
    fn text_and_image_classification() {
        assert!(ClipboardFormat::Html.is_text());
        assert!(!ClipboardFormat::Html.is_image());
        assert!(ClipboardFormat::ImagePng.is_image());
        assert!(!ClipboardFormat::ImagePng.is_text());
    }
}
