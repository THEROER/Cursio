//! Windows clipboard backend via the Win32 clipboard API.
//!
//! Reads and writes the ordinary system clipboard with
//! `OpenClipboard`/`GetClipboardData`/`SetClipboardData`, and watches for
//! changes with `AddClipboardFormatListener` on a message-only window (the same
//! hidden-window + message-loop pattern `cursio-input`'s Windows capture uses).
//!
//! ## Formats
//!
//! Two representations are carried, covering what actually gets copied between
//! machines:
//!
//! - **Text** ↔ `CF_UNICODETEXT`. Windows stores UTF-16; we convert to/from the
//!   crate's UTF-8 [`ClipboardFormat::Text`].
//! - **PNG** ↔ a registered `"PNG"` clipboard format (what browsers and modern
//!   apps put alongside images) for a lossless round-trip, plus `CF_DIB` for
//!   interop with classic apps that only understand device-independent bitmaps.
//!   On write we offer both (PNG for fidelity, DIB decoded from it for reach);
//!   on read we prefer PNG and fall back to decoding `CF_DIB` into PNG.
//!
//! Unlike X11/Wayland, the Windows clipboard is *owned by the OS*, not served by
//! a live process: once `SetClipboardData` copies the data in, the writing
//! process can exit and the content survives. So there is no serving thread here
//! (contrast `linux.rs`).

use std::sync::mpsc as std_mpsc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    TranslateMessage, HWND_MESSAGE, MSG, WM_CLIPBOARDUPDATE, WNDCLASSW,
};

use crate::format::{ClipboardContent, ClipboardEntry, ClipboardFormat};
use crate::{Clipboard, ClipboardChanged, ClipboardError, Selection, WatchHandle};

/// Open the Windows clipboard backend.
pub fn open() -> Result<Box<dyn Clipboard>, ClipboardError> {
    Ok(Box::new(WinClipboard::new()))
}

/// A handle to the Windows clipboard. Stateless beyond the registered PNG format
/// id — every operation opens and closes the clipboard around itself, which is
/// the required Win32 discipline (the clipboard is a global, briefly-locked
/// resource).
struct WinClipboard {
    /// The registered id for the `"PNG"` clipboard format, resolved once.
    png_format: u32,
}

impl WinClipboard {
    fn new() -> Self {
        WinClipboard {
            png_format: register_format("PNG"),
        }
    }
}

/// RAII guard: opens the clipboard on construction, closes it on drop, so no
/// path can leave the clipboard locked (which would hang every other app).
struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Result<Self, ClipboardError> {
        // A null owner window is fine for a background tool. Retry briefly: the
        // clipboard is a single global lock and another app may hold it for a
        // moment right as we try.
        for _ in 0..10 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(ClipboardGuard);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Err(ClipboardError::Backend(
            "could not open the clipboard".into(),
        ))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

impl Clipboard for WinClipboard {
    fn read(&mut self, _selection: Selection) -> Result<ClipboardContent, ClipboardError> {
        // Windows has no "primary" selection; the selection arg is ignored.
        let _guard = ClipboardGuard::open()?;

        let mut content = ClipboardContent::default();

        // Text.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_ok() {
            content.insert(ClipboardEntry::lazy(ClipboardFormat::Text, 0));
        }
        // Image: PNG directly, or a DIB we can transcode to PNG.
        if unsafe { IsClipboardFormatAvailable(self.png_format) }.is_ok()
            || unsafe { IsClipboardFormatAvailable(CF_DIB.0 as u32) }.is_ok()
        {
            content.insert(ClipboardEntry::lazy(ClipboardFormat::ImagePng, 0));
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
        let _guard = ClipboardGuard::open()?;
        match format {
            ClipboardFormat::Text => read_text(),
            ClipboardFormat::ImagePng => self.read_png(),
            other => Err(ClipboardError::FormatUnavailable(other.mime().to_string())),
        }
    }

    fn write(
        &mut self,
        _selection: Selection,
        content: &ClipboardContent,
    ) -> Result<(), ClipboardError> {
        let _guard = ClipboardGuard::open()?;
        // Taking ownership: clear whatever is there, then offer our formats.
        unsafe { EmptyClipboard() }
            .map_err(|e| ClipboardError::Backend(format!("EmptyClipboard failed: {e}")))?;

        let mut wrote_any = false;

        if let Some(paths) = content.file_paths() {
            write_bytes(CF_HDROP, &drop_files(&paths))?;
            wrote_any = true;
        }

        if let Some(bytes) = content.bytes(&ClipboardFormat::Text) {
            let text = String::from_utf8_lossy(bytes);
            write_text(&text)?;
            wrote_any = true;
        }

        if let Some(png) = content.bytes(&ClipboardFormat::ImagePng) {
            // Offer the PNG verbatim (fidelity) and a decoded DIB (interop with
            // apps that only read CF_DIB). A failure to build the DIB isn't
            // fatal — the PNG format still went on.
            write_bytes(self.png_format, png)?;
            match png_to_dib(png) {
                Ok(dib) => {
                    let _ = write_bytes(CF_DIB.0 as u32, &dib);
                }
                Err(e) => tracing::debug!(error = %e, "could not build a DIB from the PNG"),
            }
            wrote_any = true;
        }

        if !wrote_any {
            return Err(ClipboardError::Empty);
        }
        Ok(())
    }
}

impl WinClipboard {
    /// Read image data as PNG: prefer the registered PNG format, else transcode
    /// a `CF_DIB`.
    fn read_png(&self) -> Result<Vec<u8>, ClipboardError> {
        if unsafe { IsClipboardFormatAvailable(self.png_format) }.is_ok() {
            return read_bytes(self.png_format);
        }
        if unsafe { IsClipboardFormatAvailable(CF_DIB.0 as u32) }.is_ok() {
            let dib = read_bytes(CF_DIB.0 as u32)?;
            return dib_to_png(&dib);
        }
        Err(ClipboardError::FormatUnavailable("image/png".into()))
    }
}

/// Register (or look up) a named clipboard format, returning its id.
fn register_format(name: &str) -> u32 {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) }
}

/// Copy the bytes of an arbitrary clipboard format out of the global handle.
///
/// The clipboard must already be open (held by a [`ClipboardGuard`]).
fn read_bytes(format: u32) -> Result<Vec<u8>, ClipboardError> {
    unsafe {
        let handle: HANDLE = GetClipboardData(format).map_err(|_| ClipboardError::OwnerGone)?;
        if handle.0.is_null() {
            return Err(ClipboardError::OwnerGone);
        }
        let hglobal = HGLOBAL(handle.0);
        let size = GlobalSize(hglobal);
        if size == 0 {
            return Err(ClipboardError::Empty);
        }
        let ptr = GlobalLock(hglobal) as *const u8;
        if ptr.is_null() {
            return Err(ClipboardError::Backend("GlobalLock failed".into()));
        }
        let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
        let _ = GlobalUnlock(hglobal);
        Ok(bytes)
    }
}

/// Read `CF_UNICODETEXT` and convert the UTF-16 to UTF-8.
fn read_text() -> Result<Vec<u8>, ClipboardError> {
    let raw = read_bytes(CF_UNICODETEXT.0 as u32)?;
    // The buffer is UTF-16 code units, NUL-terminated. Drop a trailing NUL.
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&u| u != 0)
        .collect();
    let text = String::from_utf16_lossy(&units);
    Ok(text.into_bytes())
}

/// Put arbitrary bytes on the clipboard under `format` via a moveable HGLOBAL.
///
/// The clipboard must be open and already emptied. On success the OS owns the
/// HGLOBAL, so we must NOT free it.
fn write_bytes(format: u32, bytes: &[u8]) -> Result<(), ClipboardError> {
    unsafe {
        let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
            .map_err(|e| ClipboardError::Backend(format!("GlobalAlloc failed: {e}")))?;
        let ptr = GlobalLock(hglobal) as *mut u8;
        if ptr.is_null() {
            return Err(ClipboardError::Backend("GlobalLock failed".into()));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(hglobal);

        SetClipboardData(format, Some(HANDLE(hglobal.0)))
            .map_err(|e| ClipboardError::Backend(format!("SetClipboardData failed: {e}")))?;
        Ok(())
    }
}

/// The standard format id of a file list, `CF_HDROP`.
const CF_HDROP: u32 = 15;

/// A `DROPFILES` block: the 20-byte header (`pFiles`, a zero point, `fNC`,
/// `fWide`) then each path as NUL-terminated UTF-16, and one more NUL to end
/// the list. Explorer and every file dialog read pasted files from this.
fn drop_files(paths: &[std::path::PathBuf]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&20u32.to_le_bytes()); // pFiles: offset of the list
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    bytes.extend_from_slice(&0u32.to_le_bytes()); // fNC
    bytes.extend_from_slice(&1u32.to_le_bytes()); // fWide: UTF-16 paths
    for path in paths {
        for unit in path.as_os_str().to_string_lossy().encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

/// Write text as `CF_UNICODETEXT` (UTF-16, NUL-terminated).
fn write_text(text: &str) -> Result<(), ClipboardError> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0); // NUL terminator
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    write_bytes(CF_UNICODETEXT.0 as u32, &bytes)
}

/// Decode a PNG and re-encode it as a BMP, then strip the 14-byte BMP file
/// header to leave the `CF_DIB` payload (BITMAPINFOHEADER + pixels), which is
/// exactly what the clipboard expects for `CF_DIB`.
fn png_to_dib(png: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| ClipboardError::Backend(format!("PNG decode failed: {e}")))?;
    let mut bmp = std::io::Cursor::new(Vec::new());
    img.write_to(&mut bmp, image::ImageFormat::Bmp)
        .map_err(|e| ClipboardError::Backend(format!("BMP encode failed: {e}")))?;
    let bmp = bmp.into_inner();
    // A BMP file starts with a 14-byte BITMAPFILEHEADER; CF_DIB omits it.
    if bmp.len() <= 14 {
        return Err(ClipboardError::Backend("BMP too small".into()));
    }
    Ok(bmp[14..].to_vec())
}

/// Rebuild a BMP file (add the 14-byte file header back) from a `CF_DIB`
/// payload, then decode it and re-encode as PNG.
fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    if dib.len() < std::mem::size_of::<BITMAPINFOHEADER>() {
        return Err(ClipboardError::Backend("DIB too small".into()));
    }
    // Reconstruct a BMP file: 14-byte BITMAPFILEHEADER + the DIB.
    let file_size = 14 + dib.len();
    // Pixel data offset = 14 + info header + palette. We can't cheaply know the
    // palette size here, but for the common 24/32-bit true-colour DIBs there is
    // no palette, so offset = 14 + biSize. Read biSize from the header.
    let bi_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    let pixel_offset = 14 + bi_size;

    let mut bmp = Vec::with_capacity(file_size);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
    bmp.extend_from_slice(&0u16.to_le_bytes()); // reserved
    bmp.extend_from_slice(&0u16.to_le_bytes()); // reserved
    bmp.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);

    let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp)
        .map_err(|e| ClipboardError::Backend(format!("DIB decode failed: {e}")))?;
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| ClipboardError::Backend(format!("PNG encode failed: {e}")))?;
    Ok(png.into_inner())
}

// ---------------------------------------------------------------------------
// Watcher: a message-only window listening for WM_CLIPBOARDUPDATE.
// ---------------------------------------------------------------------------

/// Start watching for clipboard changes.
///
/// Spawns a thread that owns a hidden message-only window registered as a
/// clipboard-format listener; each `WM_CLIPBOARDUPDATE` becomes a
/// [`ClipboardChanged`] on the returned channel. Dropping the [`WatchHandle`]
/// ends the thread (its window is destroyed when the process's message loop
/// stops — here, when the sender end closes and the thread notices).
pub fn watch(_selection: Selection) -> Result<WatchHandle, ClipboardError> {
    let (changes_tx, changes_rx) = tokio::sync::mpsc::unbounded_channel();
    // Confirm the window came up (or report why not) before returning.
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();

    let handle = std::thread::Builder::new()
        .name("clipboard-watch".into())
        .spawn(move || watch_thread(changes_tx, ready_tx))
        .map_err(|e| ClipboardError::Backend(format!("spawn watcher thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(WatchHandle {
            changes: changes_rx,
            _stop: Box::new(WatchStop { _handle: handle }),
        }),
        Ok(Err(e)) => Err(ClipboardError::Backend(e)),
        Err(_) => Err(ClipboardError::Backend(
            "watcher thread died at startup".into(),
        )),
    }
}

/// Owns the watcher thread's join handle; dropping it lets the thread be
/// detached. (The thread exits when its message loop breaks; a full clean
/// shutdown signal can be added if watchers are ever created and destroyed
/// repeatedly, which they are not today.)
struct WatchStop {
    _handle: std::thread::JoinHandle<()>,
}

// The sender the window proc uses to publish changes. Set once, per thread,
// before the message loop runs — the proc has no other way to reach it.
thread_local! {
    static WATCH_TX: std::cell::RefCell<Option<tokio::sync::mpsc::UnboundedSender<ClipboardChanged>>> =
        const { std::cell::RefCell::new(None) };
}

fn watch_thread(
    changes_tx: tokio::sync::mpsc::UnboundedSender<ClipboardChanged>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
) {
    WATCH_TX.with(|t| *t.borrow_mut() = Some(changes_tx));

    let class_name: Vec<u16> = "CursioClipboardWatch\0".encode_utf16().collect();
    unsafe {
        let hinstance =
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(watch_wndproc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            Default::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            Default::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE), // message-only window
            None,
            Some(hinstance.into()),
            None,
        );
        let hwnd = match hwnd {
            Ok(h) if !h.0.is_null() => h,
            _ => {
                let _ = ready_tx.send(Err("CreateWindowExW failed".into()));
                return;
            }
        };

        if AddClipboardFormatListener(hwnd).is_err() {
            let _ = ready_tx.send(Err("AddClipboardFormatListener failed".into()));
            return;
        }

        let _ = ready_tx.send(Ok(()));

        // Pump messages until the window is destroyed.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Window procedure: turn each clipboard-update message into a channel send.
unsafe extern "system" fn watch_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        WATCH_TX.with(|t| {
            if let Some(tx) = t.borrow().as_ref() {
                // If the receiver is gone, nothing more to do — the session
                // ended; the loop will wind down with the thread.
                let _ = tx.send(ClipboardChanged {
                    selection: Selection::Clipboard,
                });
            }
        });
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_list_is_a_wide_dropfiles_block_ending_in_two_nuls() {
        let block = drop_files(&[std::path::PathBuf::from("C:\\a.jpg")]);
        assert_eq!(&block[..4], &20u32.to_le_bytes());
        assert_eq!(&block[16..20], &1u32.to_le_bytes());
        let units: Vec<u16> = block[20..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let mut expected: Vec<u16> = "C:\\a.jpg".encode_utf16().collect();
        expected.extend([0, 0]);
        assert_eq!(units, expected);
    }
}
