//! macOS clipboard via `NSPasteboard`.
//!
//! The Objective-C lives in `native/macos/pasteboard.m` and is compiled into
//! this crate; everything here is the translation between Apple's type
//! identifiers and the shared vocabulary.
//!
//! ## UTIs, not MIME
//!
//! macOS names representations with Uniform Type Identifiers — `public.png`,
//! not `image/png` — so the table below is the whole disagreement, in the same
//! spirit as the Windows backend's `CF_*` mapping. Anything unrecognised is
//! carried verbatim as [`ClipboardFormat::Other`] under its UTI, so two Macs
//! still round-trip an application's private type through a peer that has no
//! idea what it is.
//!
//! ## Nothing announces a copy
//!
//! macOS has no clipboard-change notification of any kind — no callback, no
//! notification centre name, nothing. `changeCount` is a counter that ticks
//! whenever anyone takes ownership, and polling it is both the documented
//! approach and what every clipboard manager on the platform does. The poll
//! costs one Objective-C message; the interval below is what keeps a copy from
//! feeling delayed without spinning.
//!
//! ## Reading is not lazy here, and cannot be
//!
//! The trait's contract is that listing formats is cheap and fetching bytes is
//! not. `NSPasteboard` offers no way to ask how large a representation is
//! without materialising it, so [`MacClipboard::read`] does load each one to
//! measure it and then throws the bytes away. The sizes reported are therefore
//! honest, and the cost is real — but a pasteboard holds one copy's worth of
//! data, and the alternative is lying about sizes.

use std::ffi::{c_char, c_int, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::format::{ClipboardContent, ClipboardEntry, ClipboardFormat};
use crate::{Clipboard, ClipboardChanged, ClipboardError, Selection, WatchHandle};

/// How often the pasteboard's revision counter is read.
///
/// A copy has to be noticed before the user reaches for paste on the other
/// machine, and 200 ms is comfortably inside that. Ten times a second would
/// cost ten times as much for a difference nobody can perceive.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Open the macOS clipboard.
pub fn open() -> Result<Box<dyn Clipboard>, ClipboardError> {
    Ok(Box::new(MacClipboard))
}

/// Watch the pasteboard for copies.
pub fn watch(selection: Selection) -> Result<WatchHandle, ClipboardError> {
    // macOS has exactly one pasteboard in the sense this trait means. X11's
    // primary selection has no equivalent, so asking for it is answered
    // honestly rather than by silently watching the general one.
    if selection != Selection::Clipboard {
        return Err(ClipboardError::Unsupported);
    }

    let (changes_tx, changes) = tokio::sync::mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();

    std::thread::Builder::new()
        .name("cursio-clipboard-watch".into())
        .spawn(move || {
            // SAFETY: no arguments, no pointers.
            let mut seen = unsafe { cursio_clipboard_change_count() };
            while !thread_stop.load(Ordering::Relaxed) {
                std::thread::sleep(POLL_INTERVAL);
                // SAFETY: as above.
                let now = unsafe { cursio_clipboard_change_count() };
                if now == seen {
                    continue;
                }
                seen = now;
                if changes_tx
                    .send(ClipboardChanged {
                        selection: Selection::Clipboard,
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .map_err(|error| {
            ClipboardError::Backend(format!("could not start the clipboard watcher: {error}"))
        })?;

    Ok(WatchHandle {
        changes,
        _stop: Box::new(StopOnDrop(stop)),
    })
}

/// Stops the polling thread when the handle is dropped, which is the contract
/// [`WatchHandle`] promises.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub struct MacClipboard;

impl MacClipboard {
    /// Every type currently on the pasteboard, as UTI strings.
    fn types() -> Vec<String> {
        // Asked for the size first, because a pasteboard may carry more
        // representations than any fixed buffer would hold — an application
        // offering a dozen private types is unusual but not wrong.
        // SAFETY: a null buffer with zero capacity is the documented way to
        // ask the shim for the length it would need.
        let needed = unsafe { cursio_clipboard_types(std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Vec::new();
        }
        let mut buffer = vec![0_u8; needed as usize];
        // SAFETY: the buffer is exactly the length just reported.
        let written =
            unsafe { cursio_clipboard_types(buffer.as_mut_ptr().cast::<c_char>(), needed) };
        if written <= 0 {
            return Vec::new();
        }
        buffer.truncate(written as usize);
        buffer
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .filter_map(|part| std::str::from_utf8(part).ok().map(str::to_owned))
            .collect()
    }
}

impl Clipboard for MacClipboard {
    fn read(&mut self, selection: Selection) -> Result<ClipboardContent, ClipboardError> {
        if selection != Selection::Clipboard {
            return Err(ClipboardError::Unsupported);
        }
        let mut content = ClipboardContent::default();
        for uti in Self::types() {
            let Ok(c_uti) = CString::new(uti.as_str()) else {
                continue;
            };
            // SAFETY: `c_uti` is a valid NUL-terminated string that outlives
            // the call.
            let size = unsafe { cursio_clipboard_size(c_uti.as_ptr()) };
            if size < 0 {
                continue;
            }
            content.insert(ClipboardEntry::lazy(format_from_uti(&uti), size as u64));
        }
        if content.is_empty() {
            return Err(ClipboardError::Empty);
        }
        Ok(content)
    }

    fn load(
        &mut self,
        selection: Selection,
        format: &ClipboardFormat,
    ) -> Result<Vec<u8>, ClipboardError> {
        if selection != Selection::Clipboard {
            return Err(ClipboardError::Unsupported);
        }
        let uti = uti_for_format(format);
        let c_uti = CString::new(uti.as_str())
            .map_err(|_| ClipboardError::FormatUnavailable(format.mime().to_string()))?;

        // SAFETY: `c_uti` outlives the call.
        let size = unsafe { cursio_clipboard_size(c_uti.as_ptr()) };
        if size < 0 {
            return Err(ClipboardError::FormatUnavailable(uti));
        }
        let mut buffer = vec![0_u8; size as usize];
        // SAFETY: the buffer is exactly the length just reported; the shim
        // refuses to write past the capacity it is given.
        let read = unsafe { cursio_clipboard_read(c_uti.as_ptr(), buffer.as_mut_ptr(), size) };
        if read < 0 {
            // Another application replaced the pasteboard between the two
            // calls, which is ordinary rather than exceptional — the owner
            // really is gone.
            return Err(ClipboardError::OwnerGone);
        }
        buffer.truncate(read as usize);
        Ok(buffer)
    }

    fn write(
        &mut self,
        selection: Selection,
        content: &ClipboardContent,
    ) -> Result<(), ClipboardError> {
        if selection != Selection::Clipboard {
            return Err(ClipboardError::Unsupported);
        }
        // Clearing takes ownership, and must happen exactly once before the
        // representations are added: doing it per entry would leave only the
        // last one on the pasteboard.
        // SAFETY: no arguments.
        unsafe { cursio_clipboard_clear() };

        let mut written = 0;
        for entry in &content.entries {
            let Some(bytes) = entry.bytes.as_ref() else {
                continue;
            };
            let Ok(uti) = CString::new(uti_for_format(&entry.format)) else {
                continue;
            };
            // SAFETY: both pointers are valid for `bytes.len()` bytes and
            // outlive the call.
            let ok = unsafe {
                cursio_clipboard_write(uti.as_ptr(), bytes.as_ptr(), bytes.len() as isize)
            };
            if ok == 0 {
                tracing::debug!(format = %entry.format.mime(), "pasteboard refused a representation");
                continue;
            }
            written += 1;
        }
        if written == 0 {
            return Err(ClipboardError::Backend(
                "the pasteboard accepted none of the offered representations".into(),
            ));
        }
        Ok(())
    }
}

/// Apple's identifier for one of the shared formats.
fn uti_for_format(format: &ClipboardFormat) -> String {
    match format {
        ClipboardFormat::Text => UTI_TEXT.to_string(),
        ClipboardFormat::Html => UTI_HTML.to_string(),
        ClipboardFormat::Rtf => UTI_RTF.to_string(),
        ClipboardFormat::ImagePng => UTI_PNG.to_string(),
        ClipboardFormat::ImageJpeg => UTI_JPEG.to_string(),
        ClipboardFormat::UriList => UTI_FILE_URL.to_string(),
        // Already a UTI if it came from here, and a MIME type if it came from
        // a peer. Either way it is carried through untouched: a type nobody
        // recognises is still one two copies of the same application share.
        ClipboardFormat::Other(name) => name.clone(),
    }
}

/// The shared format one of Apple's identifiers means.
fn format_from_uti(uti: &str) -> ClipboardFormat {
    match uti {
        UTI_TEXT => ClipboardFormat::Text,
        UTI_HTML => ClipboardFormat::Html,
        UTI_RTF => ClipboardFormat::Rtf,
        UTI_PNG => ClipboardFormat::ImagePng,
        UTI_JPEG => ClipboardFormat::ImageJpeg,
        UTI_FILE_URL => ClipboardFormat::UriList,
        // Apple's legacy names for the same things, still put on the
        // pasteboard by older applications.
        "NSStringPboardType" => ClipboardFormat::Text,
        "NeXT Rich Text Format v1.0 pasteboard type" => ClipboardFormat::Rtf,
        other => ClipboardFormat::Other(other.to_string()),
    }
}

const UTI_TEXT: &str = "public.utf8-plain-text";
const UTI_HTML: &str = "public.html";
const UTI_RTF: &str = "public.rtf";
const UTI_PNG: &str = "public.png";
const UTI_JPEG: &str = "public.jpeg";
const UTI_FILE_URL: &str = "public.file-url";

extern "C" {
    fn cursio_clipboard_change_count() -> i64;
    fn cursio_clipboard_types(out: *mut c_char, cap: isize) -> isize;
    fn cursio_clipboard_size(uti: *const c_char) -> isize;
    fn cursio_clipboard_read(uti: *const c_char, out: *mut u8, cap: isize) -> isize;
    fn cursio_clipboard_clear() -> i64;
    fn cursio_clipboard_write(uti: *const c_char, bytes: *const u8, len: isize) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A format that survives the trip out and back is one a peer can actually
    /// paste; one that does not silently becomes an unrecognised blob.
    #[test]
    fn known_formats_round_trip_through_apples_identifiers() {
        for format in [
            ClipboardFormat::Text,
            ClipboardFormat::Html,
            ClipboardFormat::Rtf,
            ClipboardFormat::ImagePng,
            ClipboardFormat::ImageJpeg,
            ClipboardFormat::UriList,
        ] {
            let uti = uti_for_format(&format);
            assert_eq!(format_from_uti(&uti), format, "uti {uti}");
        }
    }

    /// An application's private type has to survive verbatim, or two copies of
    /// that application stop understanding each other across the mesh.
    #[test]
    fn an_unknown_type_is_carried_verbatim() {
        let private = ClipboardFormat::Other("com.example.thing".to_string());
        let uti = uti_for_format(&private);
        assert_eq!(uti, "com.example.thing");
        assert_eq!(format_from_uti(&uti), private);
    }

    /// Old applications still put these on the pasteboard, and treating them as
    /// unknown would lose plain text — the one representation everything can
    /// paste.
    #[test]
    fn apples_legacy_names_are_recognised_as_their_modern_formats() {
        assert_eq!(format_from_uti("NSStringPboardType"), ClipboardFormat::Text);
    }
}
