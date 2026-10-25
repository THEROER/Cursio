//! What the engine's tests share: an engine to send through, and readings of
//! what it sent.

use crate::audio_gate::MicGate;
use crate::audio_session::send::record_sent;
use crate::audio_session::AudioEngine;
use crate::mixer::LocalMixer;
pub(super) use crate::test_support::RecordingTx;
use cursio_protocol::{AudioMsg, ModuleId, PeerId};

/// The `(id, listening)` pairs of every demand report sent, in order.
pub(super) fn demands(tx: &RecordingTx) -> Vec<(u32, bool)> {
    tx.decoded::<AudioMsg>(ModuleId::AUDIO)
        .into_iter()
        .filter_map(|msg| match msg {
            AudioMsg::StreamDemand { id, listening } => Some((id, listening)),
            _ => None,
        })
        .collect()
}

/// Build a receiver holding `n` chunks, seq 0..n, for the backlog tests.
pub(super) fn queued(n: u32) -> tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::AudioChunk> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for seq in 0..n {
        tx.send(cursio_protocol::AudioChunk {
            id: 1,
            seq,
            timestamp: seq as u64,
            data: vec![0u8; 4],
        })
        .unwrap();
    }
    rx
}

/// Send chunks `seqs` on stream `id`, then run one rate-control evaluation
/// as though its interval had elapsed.
pub(super) fn send_and_adapt(e: &mut AudioEngine, id: u32, seqs: std::ops::Range<u32>) {
    for seq in seqs {
        record_sent(&mut e.unacked, &mut e.sent_since_adapt, id, seq);
    }
    e.last_adapt.clear();
    e.adapt_bitrate();
}

pub(super) fn ack(e: &mut AudioEngine, id: u32, acked_seq: u32) {
    e.on_ack(&cursio_protocol::AudioFrameAck { id, acked_seq });
}

pub(super) fn engine() -> AudioEngine {
    AudioEngine::new(
        PeerId::new_v4(),
        None,
        crate::live::Shared::new(MicGate::Auto),
        LocalMixer::default(),
        "peer".into(),
    )
}

pub(super) fn route(id: u32, device: Option<&str>) -> cursio_protocol::RouteEntry {
    cursio_protocol::RouteEntry {
        id,
        app_name: Some("app".into()),
        device: device.map(|d| (d.to_string(), d.to_string())),
    }
}

pub(super) fn advertised(tx: &RecordingTx) -> Vec<cursio_protocol::AudioStreamMeta> {
    tx.sent
        .iter()
        .filter_map(|m| match m.decode_module::<AudioMsg>(ModuleId::AUDIO) {
            Ok(Some(AudioMsg::StreamAdvertise(meta))) => Some(meta),
            _ => None,
        })
        .collect()
}

pub(super) fn closed(tx: &RecordingTx) -> Vec<u32> {
    tx.sent
        .iter()
        .filter_map(|m| match m.decode_module::<AudioMsg>(ModuleId::AUDIO) {
            Ok(Some(AudioMsg::StreamClose { id, .. })) => Some(id),
            _ => None,
        })
        .collect()
}

pub(super) fn capture_meta(device: Option<(&str, &str)>) -> cursio_protocol::AudioStreamMeta {
    cursio_protocol::AudioStreamMeta {
        id: 901800,
        role: cursio_protocol::StreamRole::Capture,
        app_name: None,
        device: device.map(|(id, desc)| (id.to_string(), desc.to_string())),
        format: cursio_protocol::AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_kind: cursio_protocol::SampleKind::F32,
        },
        codec: cursio_protocol::AudioCodec::Opus,
    }
}
