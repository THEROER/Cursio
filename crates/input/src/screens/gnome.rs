//! A GNOME desktop's screens: where Mutter placed them, and their EDID.
//!
//! Mutter's `DisplayConfig` is the one source that speaks the coordinates the
//! InputCapture portal reports a crossing in, and it answers on X11 as well as
//! Wayland. It does not carry a panel's size, though, and names connectors
//! its own way (`HDMI-1` where the kernel says `HDMI-A-1`), so the size comes
//! from the kernel's copy of each EDID, matched by maker and serial — which
//! Mutter does report, read from the same bytes.

use std::collections::HashMap;

use cursio_protocol::Screen;
use zbus::zvariant::OwnedValue;

use super::edid::Edid;
use crate::InputError;

/// `(connector, vendor, product, serial)`.
type MonitorSpec = (String, String, String, String);
/// `(id, width, height, refresh, preferred scale, scales, properties)`.
type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type Monitor = (MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);
/// `(x, y, scale, transform, primary, monitors, properties)`.
type Logical = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, OwnedValue>,
);
type State = (u32, Vec<Monitor>, Vec<Logical>, HashMap<String, OwnedValue>);

/// `layout-mode` 2: logical monitors are placed in physical pixels, so a
/// scaled monitor is not shrunk by its scale.
const LAYOUT_PHYSICAL: u32 = 2;

/// Blocks on the session bus; call it off any async runtime's own threads.
pub(super) fn screens() -> Result<Vec<Screen>, InputError> {
    let unavailable = |e: zbus::Error| {
        InputError::Unavailable(format!(
            "cannot read the monitor layout from GNOME's DisplayConfig: {e}"
        ))
    };
    let connection = zbus::blocking::Connection::session().map_err(unavailable)?;
    let reply = connection
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .map_err(unavailable)?;
    let state: State = reply.body().deserialize().map_err(unavailable)?;
    Ok(from_state(&state, &kernel_edids()))
}

fn from_state(state: &State, edids: &[Edid]) -> Vec<Screen> {
    let (_, monitors, logical, properties) = state;
    let physical = properties
        .get("layout-mode")
        .and_then(|v| u32::try_from(v).ok())
        == Some(LAYOUT_PHYSICAL);
    logical
        .iter()
        .filter_map(|(x, y, scale, transform, _, specs, _)| {
            // Mirrored monitors share one logical monitor; the first stands
            // for them.
            let spec = specs.first()?;
            let (_, modes, props) = monitors.iter().find(|(s, _, _)| s == spec)?;
            let (_, mode_w, mode_h, ..) = modes.iter().find(|mode| is_current(&mode.6))?;
            let divide = if physical { 1.0 } else { *scale };
            let mut width = (f64::from(*mode_w) / divide).round() as u32;
            let mut height = (f64::from(*mode_h) / divide).round() as u32;
            let edid = matching_edid(spec, edids);
            let mut size_mm = edid.and_then(|e| e.size_mm);
            // Odd transforms turn the panel a quarter: both its pixels and
            // its millimetres swap.
            if transform % 2 == 1 {
                std::mem::swap(&mut width, &mut height);
                size_mm = size_mm.map(|(w, h)| (h, w));
            }
            let name = props
                .get("display-name")
                .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
                .unwrap_or_else(|| spec.0.clone());
            Some(Screen {
                id: edid.map(Edid::id).unwrap_or_else(|| spec.0.clone()),
                name,
                x: *x,
                y: *y,
                width,
                height,
                size_mm,
            })
        })
        .collect()
}

fn is_current(props: &HashMap<String, OwnedValue>) -> bool {
    props
        .get("is-current")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

/// The EDID Mutter's `(connector, vendor, product, serial)` was read from:
/// same maker and serial, else same maker and model, else the only panel
/// from that maker.
fn matching_edid<'a>(spec: &MonitorSpec, edids: &'a [Edid]) -> Option<&'a Edid> {
    let (_, vendor, product, serial) = spec;
    let same_maker: Vec<&Edid> = edids.iter().filter(|e| &e.maker == vendor).collect();
    same_maker
        .iter()
        .find(|e| e.serial.as_deref() == Some(serial.as_str()))
        .or_else(|| {
            same_maker
                .iter()
                .find(|e| e.model.as_deref() == Some(product.as_str()))
        })
        .copied()
        .or_else(|| (same_maker.len() == 1).then(|| same_maker[0]))
}

/// Every connected panel's EDID, from `/sys/class/drm`. A connector with
/// nothing plugged in has an empty file.
fn kernel_edids() -> Vec<Edid> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path().join("edid")).ok())
        .filter_map(|bytes| Edid::parse(&bytes))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(connector: &str, vendor: &str, product: &str, serial: &str) -> MonitorSpec {
        (
            connector.into(),
            vendor.into(),
            product.into(),
            serial.into(),
        )
    }

    fn mode(width: i32, height: i32, current: bool) -> Mode {
        let mut props = HashMap::new();
        props.insert("is-current".into(), OwnedValue::from(current));
        ("m".into(), width, height, 60.0, 1.0, vec![1.0], props)
    }

    fn monitor(spec: MonitorSpec, name: &str) -> Monitor {
        let mut props = HashMap::new();
        props.insert(
            "display-name".into(),
            OwnedValue::try_from(zbus::zvariant::Value::from(name)).unwrap(),
        );
        (
            spec,
            vec![mode(1280, 720, false), mode(1920, 1080, true)],
            props,
        )
    }

    fn lg() -> Edid {
        Edid::parse(include_bytes!("testdata/lg-22mp.edid")).unwrap()
    }

    fn philips() -> Edid {
        Edid::parse(include_bytes!("testdata/philips-243v7.edid")).unwrap()
    }

    /// This desk: the LG on the left at 0,0, the Philips to its right.
    fn nocturne(layout_mode: u32, scale: f64) -> State {
        let lg = spec("HDMI-1", "GSM", "LG IPS FULLHD", "502NDWE0C929");
        let philips = spec("DVI-D-1", "PHL", "PHL 243V7", "UK01944014362");
        let mut props = HashMap::new();
        props.insert("layout-mode".into(), OwnedValue::from(layout_mode));
        (
            2,
            vec![
                monitor(philips.clone(), "Philips Consumer Electronics Company 24\""),
                monitor(lg.clone(), "LG Electronics 22\""),
            ],
            vec![
                (0, 0, scale, 0, false, vec![lg], HashMap::new()),
                (1920, 0, scale, 0, true, vec![philips], HashMap::new()),
            ],
            props,
        )
    }

    #[test]
    fn places_each_monitor_where_mutter_has_it_with_its_panel_size() {
        let screens = from_state(&nocturne(1, 1.0), &[lg(), philips()]);
        assert_eq!(screens.len(), 2);
        assert_eq!(screens[0].id, "GSM:502NDWE0C929");
        assert_eq!(screens[0].name, "LG Electronics 22\"");
        assert_eq!(
            (screens[0].x, screens[0].width, screens[0].height),
            (0, 1920, 1080)
        );
        assert_eq!(screens[0].size_mm, Some((480, 270)));
        assert_eq!(screens[1].x, 1920);
        assert_eq!(screens[1].size_mm, Some((527, 296)));
    }

    #[test]
    fn a_scaled_monitor_is_measured_in_logical_pixels_unless_the_layout_is_physical() {
        let logical = from_state(&nocturne(1, 2.0), &[]);
        assert_eq!((logical[0].width, logical[0].height), (960, 540));
        let physical = from_state(&nocturne(LAYOUT_PHYSICAL, 2.0), &[]);
        assert_eq!((physical[0].width, physical[0].height), (1920, 1080));
    }

    #[test]
    fn a_monitor_with_no_edid_keeps_its_connector_and_no_size() {
        let screens = from_state(&nocturne(1, 1.0), &[]);
        assert_eq!(screens[0].id, "HDMI-1");
        assert_eq!(screens[0].size_mm, None);
    }

    #[test]
    fn a_portrait_monitor_swaps_its_pixels_and_its_millimetres() {
        let mut state = nocturne(1, 1.0);
        state.2[0].3 = 1;
        let screens = from_state(&state, &[lg()]);
        assert_eq!((screens[0].width, screens[0].height), (1080, 1920));
        assert_eq!(screens[0].size_mm, Some((270, 480)));
    }
}
