//! A Mac's screens: CoreGraphics gives each display's bounds in the global
//! point space the event tap reports positions in, and its size in
//! millimetres directly, read from the panel's EDID by the system.

use cursio_protocol::Screen;

use crate::InputError;

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Rect {
    origin: Point,
    size: Size,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> Rect;
    fn CGDisplayScreenSize(display: u32) -> Size;
    fn CGDisplayIsBuiltin(display: u32) -> u32;
    fn CGDisplayVendorNumber(display: u32) -> u32;
    fn CGDisplayModelNumber(display: u32) -> u32;
    fn CGDisplaySerialNumber(display: u32) -> u32;
}

pub(super) fn screens() -> Result<Vec<Screen>, InputError> {
    let mut ids = [0_u32; 16];
    let mut count = 0_u32;
    // SAFETY: the buffer holds `ids.len()` entries; the count is a local.
    let status = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status != 0 {
        return Err(InputError::Unavailable(format!(
            "cannot list the displays: CoreGraphics error {status}"
        )));
    }
    Ok(ids[..count as usize]
        .iter()
        .map(|&display| {
            // SAFETY: `display` came from CGGetActiveDisplayList; each call
            // only reads it.
            let (bounds, size, builtin, vendor, model, serial) = unsafe {
                (
                    CGDisplayBounds(display),
                    CGDisplayScreenSize(display),
                    CGDisplayIsBuiltin(display) != 0,
                    CGDisplayVendorNumber(display),
                    CGDisplayModelNumber(display),
                    CGDisplaySerialNumber(display),
                )
            };
            let width = bounds.size.width.round() as u32;
            let height = bounds.size.height.round() as u32;
            let size_mm = (size.width >= 1.0 && size.height >= 1.0).then(|| {
                let mm = (size.width.round() as u32, size.height.round() as u32);
                super::oriented(mm, width, height)
            });
            Screen {
                id: format!("{vendor:04x}:{model:04x}:{serial:08x}"),
                name: if builtin {
                    "Built-in display".to_string()
                } else {
                    format!("Display {model:04x}")
                },
                x: bounds.origin.x.round() as i32,
                y: bounds.origin.y.round() as i32,
                width,
                height,
                size_mm,
            }
        })
        .collect())
}
