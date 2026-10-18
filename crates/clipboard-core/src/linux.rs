//! Linux clipboard backend, via `arboard` (X11 / XWayland).
//!
//! ## Why X11 and not Wayland's `data-control`
//!
//! Reading someone else's clipboard is privileged on Wayland: an ordinary
//! client only sees the clipboard while it holds keyboard focus, which is
//! useless for a background history manager that has no window at all. The
//! `wlr-data-control` / `ext-data-control` protocol exists for exactly this
//! case — and it is the obvious choice on wlroots compositors (Sway, Hyprland).
//!
//! **GNOME does not implement it.** Verified on this project's target system
//! (GNOME Shell 49, Ubuntu, Wayland session): the compositor's registry
//! advertises only `wl_seat`, with neither `ext_data_control_manager_v1` nor
//! `zwlr_data_control_manager_v1` present, even though the protocol XML ships
//! in `/usr/share/wayland-protocols/staging/`. `wl-clipboard-rs` therefore
//! fails with `MissingProtocol`, and `wl-paste` only works because it is a
//! focus-dependent client — which a daemon cannot be.
//!
//! What *does* work is XWayland: GNOME bridges the X11 selection to the Wayland
//! clipboard **both ways**, including for native Wayland applications and
//! including images (verified by round-tripping a PNG between `wl-copy` and
//! `xclip`). So the X11 path is not a legacy fallback here — on GNOME it is the
//! only way a background process can do this job.
//!
//! `arboard` is used rather than raw X11 because the alternatives cannot write
//! several representations at once: `x11-clipboard` and `xclip` hold a single
//! `(target, value)` pair per selection, which would make every copy lose all
//! but one of its formats.
//!
//! ## Two consequences worth knowing
//!
//! **Images are transcoded, not passed through.** `arboard` decodes an image to
//! raw RGBA on read and re-encodes it on write. That is lossless for PNG, but
//! it means [`load`](Clipboard::load) returns *re-encoded* PNG bytes rather
//! than the source's original file, and a JPEG on the clipboard comes back as
//! PNG. Callers that store images must budget for PNG-encoded size, not RGBA:
//! a 4K screenshot is ~2 MB as PNG but ~33 MB as raw pixels.
//!
//! **Whoever writes must keep serving.** The X11 clipboard has no storage of
//! its own — content lives in the process that copied it, which hands it over
//! on each paste. A process that writes and exits leaves an empty clipboard, so
//! [`write`](Clipboard::write) hands ownership to a background thread that
//! stays alive to answer paste requests.

use std::io::Cursor;

use crate::{
    Clipboard, ClipboardChanged, ClipboardContent, ClipboardEntry, ClipboardError, ClipboardFormat,
    Selection, WatchHandle,
};

use arboard::{GetExtLinux, SetExtLinux};

/// How often the watcher re-reads the clipboard looking for a change.
///
/// Polling is not the first choice, but X11 offers no usable change
/// notification without owning the selection, and GNOME rules out
/// `data-control` (see the module docs). 400 ms is below the threshold where a
/// person notices lag between copying and the item appearing in history, while
/// staying cheap: a poll compares a small fingerprint, never full image bytes
/// (see [`watch`]).
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

impl From<Selection> for arboard::LinuxClipboardKind {
    fn from(selection: Selection) -> Self {
        match selection {
            Selection::Clipboard => arboard::LinuxClipboardKind::Clipboard,
            Selection::Primary => arboard::LinuxClipboardKind::Primary,
        }
    }
}

impl From<arboard::Error> for ClipboardError {
    fn from(e: arboard::Error) -> Self {
        match e {
            // "Nothing there in that format" is an ordinary state, not a fault:
            // it is what an empty clipboard, and a probe for a format the owner
            // doesn't offer, both look like.
            arboard::Error::ContentNotAvailable => ClipboardError::Empty,
            arboard::Error::ClipboardNotSupported => ClipboardError::Unsupported,
            arboard::Error::ClipboardOccupied => {
                ClipboardError::Backend("clipboard is locked by another process".to_string())
            }
            other => ClipboardError::Backend(other.to_string()),
        }
    }
}

/// Open the backend for this session.
pub fn open() -> Result<Box<dyn Clipboard>, ClipboardError> {
    // `arboard` picks X11 or Wayland itself; on GNOME this lands on XWayland.
    let inner = arboard::Clipboard::new()?;
    Ok(Box::new(ArboardClipboard { inner }))
}

struct ArboardClipboard {
    inner: arboard::Clipboard,
}

impl ArboardClipboard {
    /// Fetch one representation, mapping "absent" onto the right error.
    ///
    /// `arboard` reports both "clipboard empty" and "this format isn't offered"
    /// as `ContentNotAvailable`. When probing a *specific* format the latter is
    /// what is meant, so it is translated to
    /// [`FormatUnavailable`](ClipboardError::FormatUnavailable) — a caller
    /// asking for PNG needs to distinguish "no image here" from "clipboard is
    /// empty" to decide whether to try another format.
    fn fetch(&mut self, format: &ClipboardFormat) -> Result<Vec<u8>, ClipboardError> {
        let bytes = match format {
            ClipboardFormat::Text => self.inner.get_text().map(String::into_bytes),
            ClipboardFormat::Html => {
                // arboard can set HTML but only reads it back as text, so there
                // is no separate HTML read path to offer here.
                return Err(ClipboardError::FormatUnavailable(
                    "text/html (read unsupported by backend)".to_string(),
                ));
            }
            ClipboardFormat::ImagePng => {
                let image = self.inner.get_image()?;
                return encode_png(&image);
            }
            ClipboardFormat::UriList => self.inner.get().file_list().map(|paths| {
                paths
                    .iter()
                    .map(|p| format!("file://{}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\r\n")
                    .into_bytes()
            }),
            // JPEG and app-specific types aren't reachable through arboard's
            // typed API; an image always comes back as PNG (see module docs).
            other => {
                return Err(ClipboardError::FormatUnavailable(other.mime().to_string()));
            }
        };

        bytes.map_err(|e| match e {
            arboard::Error::ContentNotAvailable => {
                ClipboardError::FormatUnavailable(format.mime().to_string())
            }
            other => ClipboardError::from(other),
        })
    }
}

/// Re-encode raw RGBA pixels as PNG.
///
/// `arboard` hands images back as decoded pixels, but every consumer here wants
/// a portable, compact encoding — storing raw RGBA would cost ~16x more for a
/// typical screenshot, and no other application would understand the bytes.
fn encode_png(image: &arboard::ImageData<'_>) -> Result<Vec<u8>, ClipboardError> {
    let width = u32::try_from(image.width)
        .map_err(|_| ClipboardError::Backend("image width out of range".to_string()))?;
    let height = u32::try_from(image.height)
        .map_err(|_| ClipboardError::Backend("image height out of range".to_string()))?;

    let buffer =
        image::RgbaImage::from_raw(width, height, image.bytes.to_vec()).ok_or_else(|| {
            ClipboardError::Backend("clipboard image pixel buffer has the wrong length".to_string())
        })?;

    let mut png = Vec::new();
    buffer
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| ClipboardError::Backend(format!("encoding clipboard image as PNG: {e}")))?;
    Ok(png)
}

/// Decode PNG (or any supported encoding) back to the raw RGBA `arboard` wants.
fn decode_image(bytes: &[u8]) -> Result<arboard::ImageData<'static>, ClipboardError> {
    let decoded = image::load_from_memory(bytes)
        .map_err(|e| ClipboardError::Backend(format!("decoding clipboard image: {e}")))?
        .to_rgba8();
    Ok(arboard::ImageData {
        width: decoded.width() as usize,
        height: decoded.height() as usize,
        bytes: std::borrow::Cow::Owned(decoded.into_raw()),
    })
}

impl Clipboard for ArboardClipboard {
    fn read(&mut self, selection: Selection) -> Result<ClipboardContent, ClipboardError> {
        // `arboard`'s typed API has no "list what's on offer" call, so the
        // representations are discovered by probing the ones it can actually
        // return. Probing text first is deliberate: it is by far the common
        // case and the cheapest, so the usual copy costs one call.
        let mut content = ClipboardContent::default();

        match self.inner.get().clipboard(selection.into()).text() {
            Ok(text) => content.insert(ClipboardEntry::loaded(
                ClipboardFormat::Text,
                text.into_bytes(),
            )),
            Err(arboard::Error::ContentNotAvailable) => {}
            Err(e) => return Err(ClipboardError::from(e)),
        }

        // Only look for an image when there is no text. Copies are
        // overwhelmingly textual, and probing for an image forces the owner to
        // render and transfer the full bitmap — far too expensive to do on
        // every poll of the watcher just to find nothing.
        if content.is_empty() {
            match self.inner.get().clipboard(selection.into()).image() {
                Ok(image) => {
                    let png = encode_png(&image)?;
                    content.insert(ClipboardEntry::loaded(ClipboardFormat::ImagePng, png));
                }
                Err(arboard::Error::ContentNotAvailable) => {}
                Err(e) => return Err(ClipboardError::from(e)),
            }
        }

        if content.is_empty() {
            return Err(ClipboardError::Empty);
        }
        Ok(content)
    }

    fn load(
        &mut self,
        _selection: Selection,
        format: &ClipboardFormat,
    ) -> Result<Vec<u8>, ClipboardError> {
        self.fetch(format)
    }

    fn write(
        &mut self,
        selection: Selection,
        content: &ClipboardContent,
    ) -> Result<(), ClipboardError> {
        // Files first: a file list is what was copied when there is one.
        // arboard offers it as `text/uri-list`, which file managers paste as
        // files and chat windows attach.
        if let Some(paths) = content.file_paths() {
            return set_serving(selection, move |set| set.file_list(&paths));
        }

        // Prefer an image if one is present, else the richest text. arboard
        // sets one representation at a time, so unlike a data-control backend
        // this cannot offer text and HTML simultaneously.
        if let Some(entry) = content.image() {
            if let Some(bytes) = &entry.bytes {
                let image = decode_image(bytes)?;
                return set_serving(selection, move |set| set.image(image));
            }
        }

        if let Some(html) = content.bytes(&ClipboardFormat::Html) {
            let html = String::from_utf8_lossy(html).into_owned();
            // Pass the plain-text representation as the fallback alternative so
            // a target that can't take HTML still pastes something sensible.
            let alt = content.as_text().map(str::to_owned);
            return set_serving(selection, move |set| set.html(html, alt));
        }

        if let Some(text) = content.as_text() {
            let text = text.to_owned();
            return set_serving(selection, move |set| set.text(text));
        }

        Err(ClipboardError::Empty)
    }
}

/// Put content on the clipboard and keep serving paste requests for it.
///
/// The X11 clipboard is served by its owner, so the writer must outlive the
/// call (see module docs). `arboard`'s `wait()` provides that, but it does not
/// return once the content is set — it blocks *inside* the set call until the
/// clipboard is replaced, which may be hours later. So the work runs on a
/// detached thread with its own connection and the caller returns as soon as
/// ownership has been taken.
///
/// That timing is the subtle part. Waiting on the set call's `Result` would
/// deadlock (it doesn't arrive until the clipboard is replaced), so the thread
/// instead signals *just before* blocking, and this function waits only for
/// that signal. The trade-off is deliberate: failures raised after ownership is
/// taken are logged rather than returned, since the alternative is a `write`
/// that never returns.
fn set_serving<F>(selection: Selection, apply: F) -> Result<(), ClipboardError>
where
    F: FnOnce(arboard::Set<'_>) -> Result<(), arboard::Error> + Send + 'static,
{
    // Carries either an early failure (no connection) or "ownership taken".
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(ClipboardError::from(e)));
                return;
            }
        };

        // `wait()` keeps this thread — and thus the X11 connection serving the
        // content — alive until something else takes the clipboard.
        let set = clipboard.set().clipboard(selection.into()).wait();

        // Signal before the blocking call, or the caller would wait for a
        // result that only arrives once the clipboard is replaced.
        let _ = tx.send(Ok(()));

        match apply(set) {
            Ok(()) => tracing::debug!("clipboard ownership released"),
            Err(e) => tracing::warn!(error = %e, "serving the clipboard failed"),
        }
    });

    // Bounded so a wedged X11 connection surfaces as an error rather than
    // hanging the caller forever.
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(ClipboardError::Backend(
            "timed out taking clipboard ownership".to_string(),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(ClipboardError::Backend(
            "clipboard thread died before taking ownership".to_string(),
        )),
    }
}

/// Watch the clipboard for changes.
///
/// Polls (see [`POLL_INTERVAL`]) because neither viable notification mechanism
/// is available here: X11 has none without owning the selection, and GNOME does
/// not implement `data-control`.
///
/// What counts as "changed" is the awkward part. Comparing the text is cheap
/// and catches most copies, but it cannot see one image replacing another:
/// neither state has any text, so nothing appears to change and the copy is
/// silently missed (observed in practice). Comparing full image bytes instead
/// would force the owning application to render and transfer an entire bitmap
/// several times a second — far too expensive to do while merely watching.
///
/// So each tick compares a cheap *fingerprint*. For text that is the text
/// itself. For an image it must **not** involve decoding: `get_image()` decodes
/// the whole PNG to RGBA, which for a 1920x1080 screenshot is ~8 MB of work,
/// twice a second, forever — enough to peg a core and visibly stall the UI
/// while an image sits on the clipboard (measured: 45% of a core, versus 0%
/// with text). Instead the X11 selection's *ownership timestamp* is read, a
/// tiny property the owner updates on every new copy, which changes whenever
/// the content does without touching the content at all.
///
/// Notifications still carry no content, keeping the laziness contract.
pub fn watch(selection: Selection) -> Result<WatchHandle, ClipboardError> {
    let mut clipboard = arboard::Clipboard::new()?;
    let (tx, changes) = tokio::sync::mpsc::unbounded_channel();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_stop = stop.clone();

    std::thread::spawn(move || {
        let mut previous = fingerprint(&mut clipboard, selection);

        while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(POLL_INTERVAL);

            let current = fingerprint(&mut clipboard, selection);
            if current != previous {
                previous = current;
                if tx.send(ClipboardChanged { selection }).is_err() {
                    break; // receiver dropped
                }
            }
        }
        tracing::debug!("clipboard watch ended");
    });

    Ok(WatchHandle {
        changes,
        _stop: Box::new(StopGuard(stop)),
    })
}

/// A cheap value that changes whenever the clipboard's contents change.
///
/// Text is used verbatim when present. Otherwise an image is described by its
/// dimensions and byte length — enough to tell two images apart without
/// hashing megabytes on every tick (see [`watch`]). `None` means the clipboard
/// holds neither.
#[derive(PartialEq)]
enum Fingerprint {
    /// Textual content, compared verbatim.
    Text(String),
    /// Non-textual content, identified without reading it (see
    /// [`selection_timestamp`]).
    Opaque(String),
    /// Nothing on the clipboard.
    Empty,
}

fn fingerprint(clipboard: &mut arboard::Clipboard, selection: Selection) -> Fingerprint {
    if let Ok(text) = clipboard.get().clipboard(selection.into()).text() {
        return Fingerprint::Text(text);
    }
    // No text: something non-textual (usually an image) is on the clipboard.
    // Identify it from the *encoded* bytes, which is cheap, rather than from
    // decoded pixels — see the note in [`watch`] on why decoding here is
    // ruinous.
    match encoded_fingerprint(selection) {
        Some(stamp) => Fingerprint::Opaque(stamp),
        None => Fingerprint::Empty,
    }
}

/// A cheap identity for non-textual clipboard content.
///
/// Reads the PNG the owner offers **without decoding it** and summarises it as
/// "length:first bytes:last bytes". That is enough to tell one image from
/// another — two different screenshots essentially never agree on all three —
/// while costing about 10 ms, against the ~8 MB of RGBA decoding that
/// `get_image()` would do on every single tick.
///
/// Uses `xclip` rather than the backend's typed API precisely because the typed
/// API always decodes; here the encoded bytes are what is wanted.
///
/// Returns `None` when the clipboard holds no image, which the caller treats as
/// empty.
fn encoded_fingerprint(selection: Selection) -> Option<String> {
    let which = match selection {
        Selection::Clipboard => "clipboard",
        Selection::Primary => "primary",
    };
    let output = std::process::Command::new("xclip")
        .args(["-selection", which, "-t", "image/png", "-o"])
        .output()
        .ok()?;

    let bytes = output.stdout;
    if !output.status.success() || bytes.is_empty() {
        return None;
    }

    // Ends as well as length: an edit that preserves the size (a re-save, a
    // one-pixel change) still moves the tail.
    let head = &bytes[..bytes.len().min(64)];
    let tail = &bytes[bytes.len().saturating_sub(64)..];
    Some(format!(
        "{}:{:x?}:{:x?}",
        bytes.len(),
        head.iter()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(*b as u64)),
        tail.iter()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(*b as u64)),
    ))
}

/// Signals the polling thread to exit when the handle is dropped.
struct StopGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny solid-colour RGBA image.
    fn rgba(width: usize, height: usize) -> arboard::ImageData<'static> {
        arboard::ImageData {
            width,
            height,
            bytes: std::borrow::Cow::Owned(
                std::iter::repeat_n([0u8, 128, 255, 255], width * height)
                    .flatten()
                    .collect(),
            ),
        }
    }

    #[test]
    fn image_survives_encode_decode_roundtrip() {
        // PNG is lossless, so pixels must come back bit-identical — otherwise
        // pasting an image would silently degrade it.
        let original = rgba(4, 3);
        let png = encode_png(&original).unwrap();
        assert_eq!(&png[..4], b"\x89PNG", "should be PNG-encoded");

        let decoded = decode_image(&png).unwrap();
        assert_eq!(decoded.width, 4);
        assert_eq!(decoded.height, 3);
        assert_eq!(decoded.bytes.as_ref(), original.bytes.as_ref());
    }

    #[test]
    fn png_encoding_is_far_smaller_than_raw_pixels() {
        // The reason images are stored encoded rather than as RGBA: a large
        // flat image compresses enormously, and history quotas depend on it.
        let image = rgba(200, 200);
        let raw_len = image.bytes.len();
        let png = encode_png(&image).unwrap();
        assert!(
            png.len() < raw_len / 10,
            "PNG {} should be far below raw {raw_len}",
            png.len()
        );
    }

    #[test]
    fn malformed_pixel_buffer_is_rejected() {
        // Dimensions that don't match the buffer must fail rather than panic
        // or silently produce a corrupt image.
        let bad = arboard::ImageData {
            width: 10,
            height: 10,
            bytes: std::borrow::Cow::Owned(vec![0; 12]),
        };
        assert!(matches!(encode_png(&bad), Err(ClipboardError::Backend(_))));
    }

    #[test]
    fn decoding_non_image_bytes_fails_cleanly() {
        assert!(matches!(
            decode_image(b"this is not an image"),
            Err(ClipboardError::Backend(_))
        ));
    }
}
