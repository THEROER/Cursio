//! What a SETUP and a TEARDOWN body ask for, read from the binary plists the
//! sender posts.

use crate::audio::{self, AudioStreamRequest};
use crate::media::MIRROR_STREAM_TYPE;
use plist::Value;
use std::io::Cursor;

/// Read `volume: <decibels>` out of a `SET_PARAMETER` body.
///
/// AirPlay states volume in decibels: 0 is full, negative is quieter, and
/// -144 means muted.
pub(super) fn parse_volume(body: &[u8]) -> Option<f32> {
    std::str::from_utf8(body)
        .ok()?
        .lines()
        .find_map(|line| line.trim().strip_prefix("volume:"))
        .and_then(|value| value.trim().parse::<f32>().ok())
}

/// The stream types a `TEARDOWN` names, or `None` when it names none and
/// therefore ends the session.
pub(super) fn teardown_stream_types(body: &[u8]) -> Option<Vec<u64>> {
    let value = Value::from_reader(Cursor::new(body)).ok()?;
    let streams = value.as_dictionary()?.get("streams")?.as_array()?;
    if streams.is_empty() {
        return None;
    }
    Some(
        streams
            .iter()
            .filter_map(Value::as_dictionary)
            .filter_map(|stream| stream.get("type"))
            .filter_map(plist_u64_bits)
            .collect(),
    )
}

pub(super) struct SetupKeys {
    pub(super) encrypted_key: Vec<u8>,
    /// The audio stream's CBC initialisation vector, reused for every packet.
    pub(super) iv: [u8; 16],
    pub(super) remote_timing_port: u16,
}

pub(super) struct SetupRequest {
    pub(super) keys: Option<SetupKeys>,
    pub(super) mirror_stream_id: Option<u64>,
    pub(super) audio: Option<AudioStreamRequest>,
}

impl SetupRequest {
    pub(super) fn parse(body: &[u8]) -> Result<Self, &'static str> {
        let value =
            Value::from_reader(Cursor::new(body)).map_err(|_| "invalid AirPlay SETUP plist")?;
        let root = value
            .as_dictionary()
            .ok_or("AirPlay SETUP root is not a dictionary")?;

        let encrypted_key = root.get("ekey").and_then(Value::as_data);
        let encrypted_iv = root.get("eiv").and_then(Value::as_data);
        let keys = match (encrypted_key, encrypted_iv) {
            (None, None) => None,
            (Some(key), Some(iv)) if !key.is_empty() && key.len() <= 1024 && iv.len() == 16 => {
                let timing_port = root
                    .get("timingPort")
                    .and_then(Value::as_unsigned_integer)
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port != 0)
                    .ok_or("AirPlay SETUP has an invalid timingPort")?;
                Some(SetupKeys {
                    encrypted_key: key.to_vec(),
                    iv: iv.try_into().expect("length checked above"),
                    remote_timing_port: timing_port,
                })
            }
            _ => return Err("AirPlay SETUP has invalid ekey/eiv fields"),
        };

        let mut mirror_stream_id = None;
        let mut audio = None;
        if let Some(streams) = root.get("streams") {
            for stream in streams
                .as_array()
                .ok_or("AirPlay SETUP streams is not an array")?
            {
                let stream = stream
                    .as_dictionary()
                    .ok_or("AirPlay SETUP stream is not a dictionary")?;
                if stream.get("type").and_then(Value::as_unsigned_integer)
                    == Some(MIRROR_STREAM_TYPE)
                {
                    if mirror_stream_id.is_some() {
                        return Err("AirPlay SETUP contains multiple mirror streams");
                    }
                    mirror_stream_id = Some(
                        stream
                            .get("streamConnectionID")
                            .and_then(plist_u64_bits)
                            .ok_or("mirror stream has no streamConnectionID")?,
                    );
                } else if stream.get("type").and_then(Value::as_unsigned_integer)
                    == Some(audio::AUDIO_STREAM_TYPE)
                {
                    if audio.is_some() {
                        return Err("AirPlay SETUP contains multiple audio streams");
                    }
                    audio = Some(AudioStreamRequest::parse(stream));
                }
            }
        }

        Ok(Self {
            keys,
            mirror_stream_id,
            audio,
        })
    }
}

/// Binary plists do not tag integers as signed or unsigned. Values with their
/// high bit set are decoded by some plist producers/readers as negative i64,
/// while AirPlay defines streamConnectionID as an opaque u64. Preserve the
/// original two's-complement bits in either representation.
pub(crate) fn plist_u64_bits(value: &Value) -> Option<u64> {
    value
        .as_unsigned_integer()
        .or_else(|| value.as_signed_integer().map(|value| value as u64))
}
