//! Linux audio backend via PipeWire.
//!
//! Implements both the source (capture, [`PwSource`]) and sink (playback,
//! [`PwSink`]) sides of [`crate`] on Linux. PipeWire runs its own thread-local
//! main loop that can't share a tokio task, so each stream owns a dedicated OS
//! thread running a [`pipewire::main_loop::MainLoopRc`]; it talks to the async
//! world only through channels — captured samples out, inbound samples in. This
//! mirrors how `cursio-input`'s evdev capture bridges a blocking source to tokio
//! channels.
//!
//! This first cut is **whole-device, RawPcm f32**: capture one sink's monitor,
//! and play one inbound stream into a virtual output through a small jitter
//! buffer. Per-app nodes (advertising each PipeWire stream by `application.name`)
//! and Opus come in later milestones; the trait shape already accommodates them.

use cursio_protocol::{AudioFormat, SampleKind};
use pipewire::spa::{
    param::audio::{AudioFormat as SpaAudioFormat, AudioInfoRaw},
    pod::{serialize::PodSerializer, Object, Value},
    sys::{
        SPA_PARAM_EnumFormat, SPA_TYPE_OBJECT_Format, SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR,
        SPA_AUDIO_MAX_CHANNELS,
    },
};

mod app_links;
mod capture;
mod devices;
mod intent_sink;
mod listing;
mod playback;

pub use capture::{CaptureMode, PwSource};
pub(crate) use devices::spawn_device_monitor;
pub use intent_sink::{IntentSinkEvent, PwIntentSink};
pub use listing::{list_app_streams, list_input_devices, list_output_devices};
pub use playback::PwSink;

/// Fixed capture/playback format for the first cut: 48 kHz stereo f32.
///
/// f32, not s16: PipeWire monitors are natively float, and requesting f32 avoids
/// a format conversion in the graph (and the earlier bug where the wire claimed
/// s16 while the buffers were actually float32). The Opus milestone will convert
/// to whatever the encoder wants; for RawPcm we carry the native float.
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
/// Bytes per interleaved frame (all channels) for f32 (4 bytes/sample).
const BYTES_PER_FRAME: usize = CHANNELS as usize * 4;

/// The neutral [`AudioFormat`] this backend produces/consumes.
fn wire_format() -> AudioFormat {
    AudioFormat {
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
        sample_kind: SampleKind::F32,
    }
}

/// Build the SPA `EnumFormat` POD describing our fixed f32 stereo 48k format,
/// which the capture stream negotiates against.
///
/// The channel **positions** matter as much as the count. `AudioInfoRaw::new()`
/// starts with the `UNPOSITIONED` flag set, and `set_channels` alone does not
/// clear it — only `set_position` with a non-zero first entry does. An
/// unpositioned stereo stream gets ports with an empty `audio.channel`, and then
/// nothing downstream can tell left from right:
///
/// - `relink_app` pairs app ports to ours by matching `audio.channel`, so with
///   empty labels every port fell through to positional pairing. When an app
///   exposed its ports in any other order — or exposed only one at the moment we
///   linked — both of its outputs could land on the *same* input port, so one
///   ear carried everything and the other stayed silent.
/// - The session manager cannot up/downmix a stream it has no layout for, so a
///   mono source is never spread across both ears.
///
/// Declaring `[FL, FR]` explicitly is what the null-sink already does (see
/// `audio.position` where it is created); this brings the streams in line.
fn format_param_pod() -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(SpaAudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);

    // Front-left / front-right, then zeroes for the unused slots. Setting this
    // also clears the UNPOSITIONED flag, which is the point.
    let mut position = [0u32; SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = SPA_AUDIO_CHANNEL_FL;
    position[1] = SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);

    pod_bytes(
        Object {
            type_: SPA_TYPE_OBJECT_Format,
            id: SPA_PARAM_EnumFormat,
            properties: info.into(),
        },
        "audio format",
    )
}

/// A main loop and a connection to the PipeWire server through it: where every
/// stream, listing and monitor here starts. The core keeps its context alive,
/// so the context is not handed back.
pub(super) fn connect() -> Result<(pipewire::main_loop::MainLoopRc, pipewire::core::CoreRc), String>
{
    let main_loop =
        pipewire::main_loop::MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context =
        pipewire::context::ContextRc::new(&main_loop, None).map_err(|e| format!("context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect: {e}"))?;
    Ok((main_loop, core))
}

/// Serialize `obj`, a param built here from fixed fields, which cannot fail
/// to serialize; `what` names it if that ever stops being true.
pub(super) fn pod_bytes(obj: Object, what: &str) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .unwrap_or_else(|e| panic!("serializing a {what} pod cannot fail: {e:?}"))
        .0
        .into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipewire::spa::pod::ValueArray;
    use pipewire::spa::{pod::deserialize::PodDeserializer, sys::SPA_FORMAT_AUDIO_position};

    /// Our streams must declare FL/FR positions, not just "2 channels".
    ///
    /// `AudioInfoRaw::new()` starts UNPOSITIONED and `set_channels` does not
    /// clear that flag, so a stereo format is easy to build that carries no
    /// layout. Ports then come up with an empty `audio.channel`, `relink_app`
    /// cannot match FL→FL, and the graph has no basis to keep left and right
    /// apart — heard as everything arriving in one ear.
    #[test]
    fn format_pod_declares_channel_positions() {
        let pod = format_param_pod();
        let parsed = PodDeserializer::deserialize_any_from(&pod)
            .expect("pod parses")
            .1;

        let Value::Object(obj) = parsed else {
            panic!("format pod should be an object");
        };
        let position = obj
            .properties
            .iter()
            .find(|p| p.key == SPA_FORMAT_AUDIO_position)
            .expect("format pod must carry a channel position array");

        let Value::ValueArray(ValueArray::Id(ref ids)) = position.value else {
            panic!(
                "position should be an array of ids, got {:?}",
                position.value
            );
        };
        assert_eq!(
            ids.iter().map(|id| id.0).collect::<Vec<_>>(),
            vec![SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR],
            "stereo must be declared as front-left + front-right"
        );
    }
}
