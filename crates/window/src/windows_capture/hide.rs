//! Making a transferred window invisible on the host, reversibly.
//!
//! The point of the feature is that dragging a window to the other machine
//! makes it *leave* this one — the two desktops should read as one, not as a
//! viewer showing a copy. So while a window is transferred it must be invisible
//! here, yet still rendering, since its pixels are what the peer is watching.
//!
//! Measured on Windows 11 build 26200 against a running Minecraft
//! (`tools/wgc-probe`), capture frame rate per state:
//!
//! | State | FPS |
//! |---|---|
//! | normal, visible | 30 |
//! | `WS_EX_LAYERED` + `alpha=0` | 30 |
//! | fully offscreen | 30 |
//! | **minimised** | **0** |
//!
//! Hence layered alpha, and hence the hard rule that this module never
//! minimises: minimising is both the most obvious way to "put a window away"
//! and the only one that breaks it — it stops frames *and* stops `PostMessage`,
//! which the input path depends on.
//!
//! [`HiddenWindow`] restores the original style on drop, so a panic or an
//! early return can't strand the user's window invisible.

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::UI::WindowsAndMessaging::{
    GetLayeredWindowAttributes, GetWindowLongW, SetLayeredWindowAttributes, SetWindowLongW,
    GWL_EXSTYLE, LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, WS_EX_LAYERED,
};

/// A window made invisible for the duration of a transfer.
///
/// Restoring on `Drop` is deliberate: leaving a user's window permanently
/// invisible would be the worst failure this feature could produce, so the
/// restore must not depend on the happy path being reached.
pub struct HiddenWindow {
    hwnd: HWND,
    /// The extended style before we touched it.
    prev_ex_style: i32,
    /// The previous layered attributes, when the window was *already* layered —
    /// some apps use layering for their own translucency, and clobbering it
    /// would visibly change the window after restore.
    prev_layered: Option<(COLORREF, u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS)>,
}

impl HiddenWindow {
    /// Make `hwnd` invisible while keeping it composited and driveable.
    pub fn hide(hwnd: HWND) -> Result<Self, String> {
        unsafe {
            let prev_ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
            let was_layered = (prev_ex_style & WS_EX_LAYERED.0 as i32) != 0;

            let prev_layered = if was_layered {
                let mut key = COLORREF(0);
                let mut alpha = 0u8;
                let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
                GetLayeredWindowAttributes(hwnd, Some(&mut key), Some(&mut alpha), Some(&mut flags))
                    .ok()
                    .map(|_| (key, alpha, flags))
            } else {
                None
            };

            if !was_layered && SetWindowLongW(hwnd, GWL_EXSTYLE, prev_ex_style | WS_EX_LAYERED.0 as i32) == 0
            {
                // `SetWindowLongW` returning 0 can be either the previous value
                // or a genuine failure; only the latter sets an error.
                let err = windows::core::Error::from_win32();
                if err.code().is_err() {
                    return Err(format!("set WS_EX_LAYERED: {err}"));
                }
            }

            SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA)
                .map_err(|e| format!("set alpha=0: {e}"))?;

            Ok(Self {
                hwnd,
                prev_ex_style,
                prev_layered,
            })
        }
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        unsafe {
            match self.prev_layered {
                // Was layered already: put its own attributes back.
                Some((key, alpha, flags)) => {
                    let _ = SetLayeredWindowAttributes(self.hwnd, key, alpha, flags);
                }
                // Wasn't layered: make it fully opaque, then drop the style bit.
                None => {
                    let _ = SetLayeredWindowAttributes(self.hwnd, COLORREF(0), 255, LWA_ALPHA);
                    let _ = SetWindowLongW(self.hwnd, GWL_EXSTYLE, self.prev_ex_style);
                }
            }
            tracing::info!("source window restored to its original visibility");
        }
    }
}
