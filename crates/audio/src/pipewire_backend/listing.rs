//! What this machine has to offer: its output and input devices, and the
//! applications playing sound now.

use crate::AudioError;
use std::time::Duration;

/// List PipeWire sink (output) nodes, for mirroring to the peer.
///
/// Walks the registry's initial dump — every global with
/// `media.class = Audio/Sink` is an output device — and returns its node name
/// plus a human description. Cursio's own mirror sinks are skipped so we never
/// mirror a mirror (which would loop audio between the machines).
///
/// Runs a short-lived PipeWire loop inline (the registry dump arrives within a
/// few iterations), rather than holding a connection open: enumeration is a
/// one-shot question, not a stream.
pub fn list_output_devices() -> Result<Vec<crate::OutputDevice>, AudioError> {
    list_devices_of_class("Audio/Sink")
}

/// Enumerate the machine's capture devices (microphones, line-ins).
///
/// The mirror image of [`list_output_devices`]: in PipeWire a microphone is just
/// a node whose `media.class` is `Audio/Source` instead of `Audio/Sink`, so both
/// share one implementation rather than two near-identical registry walks.
pub fn list_input_devices() -> Result<Vec<crate::OutputDevice>, AudioError> {
    list_devices_of_class("Audio/Source")
}

/// Walk the PipeWire registry for nodes of one `media.class`.
///
/// Our own mirrors are skipped (see [`crate::MIRROR_MARKER`]) so a mirrored
/// device is never mirrored back — that would loop audio between the machines.
pub(super) fn list_devices_of_class(
    media_class: &str,
) -> Result<Vec<crate::OutputDevice>, AudioError> {
    use std::cell::RefCell;
    use std::rc::Rc;

    let media_class = media_class.to_string();

    let (main_loop, core) = super::connect().map_err(AudioError::Backend)?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| AudioError::Backend(format!("registry: {e}")))?;

    let found: Rc<RefCell<Vec<crate::OutputDevice>>> = Rc::new(RefCell::new(Vec::new()));
    let found_cb = found.clone();

    let _listener = registry
        .add_listener_local()
        .global(move |obj| {
            let Some(props) = obj.props else { return };
            if props.get("media.class") != Some(media_class.as_str()) {
                return;
            }
            let Some(name) = props.get("node.name") else {
                return;
            };
            let description = props.get("node.description").unwrap_or(name).to_string();
            // Skip anything of ours, checking BOTH the node name and the
            // description. Our virtual microphones are named by their label
            // ("Cursio(peer): Mic"), so a name-only check can miss one — and
            // capturing our own virtual mic feeds the peer's audio straight back
            // to it, which the user heard as the microphone playing into their
            // headphones.
            if name.to_lowercase().contains(crate::MIRROR_MARKER)
                || description.to_lowercase().contains(crate::MIRROR_MARKER)
            {
                return;
            }
            found_cb.borrow_mut().push(crate::OutputDevice {
                id: name.to_string(),
                description,
            });
        })
        .register();

    // Pump the loop briefly so the registry's initial dump is delivered.
    let loop_ref = main_loop.loop_();
    for _ in 0..20 {
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(25)));
    }

    let devices = found.borrow().clone();
    Ok(devices)
}

/// List currently-playing application streams, for per-app sharing.
///
/// PipeWire models every playing app as a node with
/// `media.class = Stream/Output/Audio`, so this is the same registry walk as
/// [`list_output_devices`] with a different class filter. The returned `id` is
/// the node's **numeric id** (as a string), not the app name: one application
/// can have several concurrent streams (two Firefox tabs are two nodes), and
/// only the node id distinguishes them.
///
/// Cursio's own streams are skipped so we never capture our own playback.
pub fn list_app_streams() -> Result<Vec<crate::AppStream>, AudioError> {
    use std::cell::RefCell;
    use std::rc::Rc;

    let (main_loop, core) = super::connect().map_err(AudioError::Backend)?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| AudioError::Backend(format!("registry: {e}")))?;

    let found: Rc<RefCell<Vec<crate::AppStream>>> = Rc::new(RefCell::new(Vec::new()));
    let found_cb = found.clone();

    let _listener = registry
        .add_listener_local()
        .global(move |obj| {
            let Some(props) = obj.props else { return };
            if props.get("media.class") != Some("Stream/Output/Audio") {
                return;
            }
            // Skip voice/telephony streams. A Bluetooth headset mic under the
            // HFP/HSP profile registers as `Stream/Output/Audio` — the SAME class
            // as a media player — but with `media.role = "Communication"`. Without
            // this guard we tapped the headset MIC as if it were an app playing
            // audio: PipeWire then routed that "playback" into the default sink
            // (the user's Ubuntu speakers, which mirror to Windows), so speaking
            // into the headset came back through Windows and the user heard
            // themselves. Real players (Spotify, Firefox) carry role = "Music",
            // so filtering on the communication role is precise.
            if props.get("media.role") == Some("Communication") {
                return;
            }
            // Prefer the app name; fall back to the node name (some streams,
            // e.g. plain `java`, carry no application.name).
            let name = props
                .get("application.name")
                .or_else(|| props.get("node.name"))
                .unwrap_or("unknown");
            if name.to_lowercase().contains(crate::MIRROR_MARKER) {
                return; // our own playback
            }
            found_cb.borrow_mut().push(crate::AppStream {
                id: obj.id.to_string(),
                app_name: name.to_string(),
            });
        })
        .register();

    let loop_ref = main_loop.loop_();
    for _ in 0..20 {
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(25)));
    }

    let streams = found.borrow().clone();
    Ok(streams)
}
