//! Windows cursor hiding via `SetSystemCursor`.
//!
//! When this machine hands control to the peer, its own cursor should vanish.
//! A background process has no reliable per-window cursor to hide (`ShowCursor`
//! only affects the calling thread's windows), so we replace **every system
//! cursor** with a fully transparent one via `SetSystemCursor`. That hides the
//! pointer globally regardless of which window it's over.
//!
//! ## Why we don't restore with `SPI_SETCURSORS`
//!
//! The obvious way back is `SystemParametersInfoW(SPI_SETCURSORS)`, which
//! reloads the real cursors from the registry. It works in isolation, but it is
//! **asynchronous**: it broadcasts `WM_SETTINGCHANGE` and the reload lands some
//! time later. Edge-crossing flips ownership every few hundred milliseconds, so
//! the next `hide()` raced the still-settling restore and the two fought — the
//! cursor came back invisible on roughly every second crossing.
//!
//! Instead we keep our own copies of the original cursors (taken with
//! `CopyImage` before the first swap) and put them back with `SetSystemCursor` —
//! the exact inverse of how we hid them, synchronous, no broadcast, no race.

use std::ptr;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CopyImage, CreateCursor, GetSystemMetrics, LoadImageW, SetSystemCursor, SystemParametersInfoW,
    IMAGE_CURSOR, LR_DEFAULTSIZE, LR_LOADFROMFILE, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_IBEAM,
    OCR_NORMAL, OCR_SIZEALL, OCR_WAIT, SM_CXCURSOR, SM_CYCURSOR, SPI_SETCURSORS, SYSTEM_CURSOR_ID,
};

use crate::{CursorControl, InputError};

/// The system cursors we overwrite when hiding, each paired with its value name
/// under `HKCU\Control Panel\Cursors` — the registry entry that points at the
/// cursor's actual file. Covers the ones a user actually sees during normal
/// desktop/app use; others fall back to these or appear only transiently.
///
/// We reload from those files rather than from whatever is currently installed,
/// because "currently installed" may be our own transparent cursor (see
/// [`load_cursor_from_registry`]).
const SYSTEM_CURSORS: &[(SYSTEM_CURSOR_ID, &str)] = &[
    (OCR_NORMAL, "Arrow"),
    (OCR_IBEAM, "IBeam"),
    (OCR_HAND, "Hand"),
    (OCR_CROSS, "Crosshair"),
    (OCR_WAIT, "Wait"),
    (OCR_APPSTARTING, "AppStarting"),
    (OCR_SIZEALL, "SizeAll"),
];

/// Hides the local cursor by swapping all system cursors for a transparent one,
/// holding copies of the originals so it can put them back exactly.
pub struct WinCursor {
    hidden: bool,
    /// Our own copies of the real cursors, one per entry in [`SYSTEM_CURSORS`],
    /// captured once at construction. `None` for any slot we couldn't copy (we
    /// then simply leave that slot alone rather than hide a cursor we can't
    /// restore).
    originals: Vec<(SYSTEM_CURSOR_ID, Option<HANDLE>)>,
}

impl WinCursor {
    /// Create the controller, snapshotting the current system cursors.
    ///
    /// The snapshot is taken now, while the cursors are known-good, so that
    /// `show()` can restore them directly instead of asking Windows to reload
    /// them asynchronously (see the module docs for why that raced).
    pub fn new() -> Result<Self, InputError> {
        let originals: Vec<_> = SYSTEM_CURSORS
            .iter()
            .map(|&(id, name)| (id, copy_system_cursor(id, name)))
            .collect();
        let copied = originals.iter().filter(|(_, h)| h.is_some()).count();
        // Worth logging: zero copies means `show()` falls back to the slower
        // registry reload, which is the difference between a crisp restore and a
        // racy one.
        tracing::info!(
            copied,
            total = originals.len(),
            "snapshotted system cursors"
        );
        Ok(Self {
            hidden: false,
            originals,
        })
    }
}

impl CursorControl for WinCursor {
    fn hide(&mut self) -> Result<(), InputError> {
        if self.hidden {
            return Ok(());
        }
        // Only touch slots we hold an original for — hiding a cursor we couldn't
        // copy would mean never being able to bring it back.
        for &(id, original) in &self.originals {
            if original.is_none() {
                continue;
            }
            // SetSystemCursor takes ownership of (and destroys) each HCURSOR
            // passed, so create a fresh transparent cursor per slot.
            let cursor = create_transparent_cursor()?;
            // SAFETY: `cursor` is a valid HCURSOR we own; SetSystemCursor
            // consumes it. `id` is a documented OCR_* system cursor id.
            let ok = unsafe { SetSystemCursor(cursor, id) };
            if ok == 0 {
                tracing::warn!(
                    id,
                    err = last_error(),
                    "SetSystemCursor failed while hiding"
                );
            }
        }
        self.hidden = true;
        tracing::info!("system cursor hidden");
        Ok(())
    }

    fn show(&mut self) -> Result<(), InputError> {
        // Fallback: with no usable copies we can't restore slot-by-slot, so ask
        // Windows to reload the real cursors from the registry. It's the async
        // path we avoid normally (it races rapid hide/show — see the module
        // docs), but a slightly racy restore beats leaving the pointer invisible.
        if self.originals.iter().all(|(_, h)| h.is_none()) {
            // SAFETY: SPI_SETCURSORS takes no in/out params; null/0 are correct.
            let ok = unsafe { SystemParametersInfoW(SPI_SETCURSORS, 0, ptr::null_mut(), 0) };
            if ok == 0 {
                return Err(InputError::Unavailable(
                    "no cursor copies and SPI_SETCURSORS failed".into(),
                ));
            }
            self.hidden = false;
            tracing::info!("system cursor restored (registry reload fallback)");
            return Ok(());
        }

        // Always restore, never short-circuit on `self.hidden`: the cursors are
        // global OS state a previous (hard-killed) run may have left transparent,
        // and putting the real ones back is harmless when they're already there.
        for &(id, original) in &self.originals {
            let Some(handle) = original else { continue };
            // SetSystemCursor destroys the handle it's given, so hand it a fresh
            // copy each time and keep our pristine original for the next restore.
            // SAFETY: `handle` is a cursor we copied and still own; CopyImage
            // returns a new independent handle (null on failure, checked below).
            let dup = unsafe { CopyImage(handle, IMAGE_CURSOR, 0, 0, 0) };
            if dup.is_null() {
                // DIAGNOSTIC: a failure here means our saved original is no
                // longer a live handle — the interesting case for the
                // every-other-time invisibility.
                tracing::warn!(
                    id,
                    err = last_error(),
                    "CopyImage failed while restoring cursor"
                );
                continue;
            }
            // SAFETY: `dup` is a valid HCURSOR we own; SetSystemCursor consumes it.
            let ok = unsafe { SetSystemCursor(dup, id) };
            if ok == 0 {
                tracing::warn!(
                    id,
                    err = last_error(),
                    "SetSystemCursor failed while restoring"
                );
            }
        }
        self.hidden = false;
        tracing::info!("system cursor restored");
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "windows"
    }
}

impl Drop for WinCursor {
    fn drop(&mut self) {
        // Never leave the desktop with an invisible cursor if we die.
        let _ = self.show();
    }
}

/// Take our own copy of the **real** system cursor for `id`.
///
/// Subtlety that cost us a round of debugging: `LoadImageW(.., LR_SHARED)` hands
/// back the *live* system handle for that slot, not a snapshot. Once
/// `SetSystemCursor` has swapped in our transparent cursor, that same handle
/// resolves to the transparent one — so copying it later captures the blank
/// cursor and "restoring" it keeps the pointer invisible forever.
///
/// Loads it **from the file the registry points at**, not from whatever is
/// installed right now. That distinction is the whole fix: every handle-based
/// route (`LoadImageW` by OCR_* ordinal, with or without `LR_SHARED`) resolves to
/// the *currently installed* cursor, so if a previous run left a transparent one
/// in place — or our own `hide()` ran microseconds after startup — the "original"
/// we captured was itself blank, and restoring it kept the pointer invisible
/// forever. The registry path survives all of that.
///
/// Returns `None` if the slot has no file (an empty value means "built-in
/// default") or the load fails, in which case the caller leaves that cursor
/// alone rather than hiding one it can't bring back.
fn copy_system_cursor(id: SYSTEM_CURSOR_ID, reg_name: &str) -> Option<HANDLE> {
    let path = read_cursor_path(reg_name)?;
    // SAFETY: LR_LOADFROMFILE with a NUL-terminated wide path loads that cursor
    // file; the returned handle is ours (no LR_SHARED) and outlives this call.
    let handle = unsafe {
        LoadImageW(
            ptr::null_mut(),
            path.as_ptr(),
            IMAGE_CURSOR,
            0,
            0,
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
        )
    };
    if handle.is_null() {
        tracing::warn!(
            id,
            reg_name,
            err = last_error(),
            "loading cursor file failed"
        );
        return None;
    }
    Some(handle)
}

/// Read `HKCU\Control Panel\Cursors\<name>` and return it as a NUL-terminated
/// wide string, with any `%SystemRoot%`-style variables expanded.
///
/// This is the source of truth for what the user's cursors actually *are*,
/// independent of what happens to be installed right now — which is the whole
/// point (see [`load_cursor_from_registry`]). An empty value means "use the
/// built-in default", which we can't load from a file, so we skip that slot.
fn read_cursor_path(name: &str) -> Option<Vec<u16>> {
    let mut key: HKEY = ptr::null_mut();
    let subkey = wide("Control Panel\\Cursors");
    // SAFETY: standard registry open; `key` is written only on success.
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut key) };
    if rc != 0 {
        tracing::warn!(rc, "cannot open HKCU\\Control Panel\\Cursors");
        return None;
    }

    let value = wide(name);
    let mut buf = [0u16; 512];
    let mut len = (buf.len() * 2) as u32; // bytes, per the API
                                          // SAFETY: `buf`/`len` describe the same buffer; the API writes at most `len`
                                          // bytes and updates `len` to what it wrote.
    let rc = unsafe {
        RegQueryValueExW(
            key,
            value.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            buf.as_mut_ptr() as *mut u8,
            &mut len,
        )
    };
    // SAFETY: `key` came from RegOpenKeyExW and hasn't been closed.
    unsafe { RegCloseKey(key) };
    if rc != 0 {
        tracing::debug!(name, rc, "no cursor path in registry for this slot");
        return None;
    }

    // Trim at the NUL the API included (len is in bytes).
    let chars = (len as usize / 2).saturating_sub(1);
    let raw: Vec<u16> = buf[..chars].to_vec();
    if raw.is_empty() {
        return None; // empty => system default, nothing to load from disk
    }

    // Expand %SystemRoot% and friends, then NUL-terminate for LoadImageW.
    let raw_z: Vec<u16> = raw.iter().copied().chain(std::iter::once(0)).collect();
    let mut expanded = [0u16; 512];
    // SAFETY: both buffers are valid; the call writes at most `expanded.len()`.
    let n = unsafe {
        ExpandEnvironmentStringsW(raw_z.as_ptr(), expanded.as_mut_ptr(), expanded.len() as u32)
    };
    if n == 0 || n as usize > expanded.len() {
        return Some(raw_z); // expansion failed; try the raw path
    }
    Some(expanded[..n as usize].to_vec())
}

/// Wrap a Rust string as a NUL-terminated UTF-16 buffer for the `*W` APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The OS last-error code, for logging failed Win32 calls.
fn last_error() -> u32 {
    // SAFETY: GetLastError has no preconditions.
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

/// Create a fully transparent cursor sized to the system cursor dimensions.
///
/// A monochrome cursor is defined by an AND mask and an XOR mask. `AND = 0xFF`
/// (all ones) with `XOR = 0x00` (all zeros) yields a fully transparent pixel
/// everywhere — the standard "invisible cursor" bitmasks.
fn create_transparent_cursor(
) -> Result<windows_sys::Win32::UI::WindowsAndMessaging::HCURSOR, InputError> {
    // SAFETY: GetSystemMetrics has no preconditions.
    let (w, h) = unsafe {
        (
            GetSystemMetrics(SM_CXCURSOR).max(1),
            GetSystemMetrics(SM_CYCURSOR).max(1),
        )
    };
    // Masks are 1 bit per pixel, padded to whole bytes per row. `div_ceil` is
    // stable for usize (unlike i32 on this toolchain), so widen first.
    let bytes_per_row = (w as usize).div_ceil(8);
    let mask_len = bytes_per_row * h as usize;
    let and_mask = vec![0xFFu8; mask_len]; // transparent everywhere
    let xor_mask = vec![0x00u8; mask_len]; // no inversion

    // SAFETY: masks are `mask_len` bytes each, matching the w×h 1bpp geometry
    // CreateCursor reads; hotspot 0,0 is valid; null hinst is allowed.
    let cursor = unsafe {
        CreateCursor(
            ptr::null_mut(),
            0,
            0,
            w,
            h,
            and_mask.as_ptr() as *const core::ffi::c_void,
            xor_mask.as_ptr() as *const core::ffi::c_void,
        )
    };
    if cursor.is_null() {
        return Err(InputError::Unavailable("CreateCursor failed".into()));
    }
    Ok(cursor)
}
