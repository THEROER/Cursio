//! This machine's screens: where each stands on the desktop, in the pixels
//! capture measures a crossing in, and how big its picture is.
//!
//! The layout every machine shares is drawn from these (`docs/input-design.md`,
//! "Screens on one desk"). Each platform has one source that speaks capture's
//! own coordinates: GNOME's `DisplayConfig` (Wayland and X11 alike), the
//! virtual screen on Windows, CoreGraphics on a Mac. A desktop with none of
//! those says so, and its machine keeps the grid's whole-edge crossing.

#[cfg(any(target_os = "linux", target_os = "windows", test))]
mod edid;
#[cfg(target_os = "linux")]
mod gnome;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use cursio_protocol::Screen;

use crate::InputError;

/// The screens as the desktop has them now. Blocks briefly (a D-Bus call on
/// Linux), so call it from a blocking thread, never an async runtime's own.
pub fn screens() -> Result<Vec<Screen>, InputError> {
    #[cfg(target_os = "linux")]
    {
        gnome::screens()
    }
    #[cfg(target_os = "windows")]
    {
        windows::screens()
    }
    #[cfg(target_os = "macos")]
    {
        macos::screens()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Err(InputError::Unavailable(
            "no monitor layout on this platform".to_string(),
        ))
    }
}

/// A panel's millimetres turned the way its pixels are: a monitor stood on
/// its side reports the size it has lying down.
#[cfg_attr(
    not(any(target_os = "windows", target_os = "macos", test)),
    allow(dead_code)
)]
fn oriented((w, h): (u32, u32), width: u32, height: u32) -> (u32, u32) {
    if (w > h) == (width > height) {
        (w, h)
    } else {
        (h, w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_portrait_panel_reports_its_size_standing_up() {
        assert_eq!(oriented((527, 296), 1080, 1920), (296, 527));
        assert_eq!(oriented((527, 296), 1920, 1080), (527, 296));
    }
}
