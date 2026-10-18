//! A phone's call voice on this computer: the Bluetooth hands-free service
//! started, its links filed in the calls hub, and its PCM played and recorded
//! on the same outputs and microphone everything else here uses.
//!
//! The voice plays through an ordinary Cursio playback node and the
//! microphone is read through the same tap that shares it with peers, so the
//! call follows the desktop's default devices (and whatever routing the user
//! applied to Cursio's nodes) instead of a Bluetooth headset appearing and
//! taking them over for the length of the call.

use std::sync::Arc;

use cursio_audio::{Capture, Playback};
use cursio_bluetooth::resample::{Downsampler, MusicResampler, Upsampler};
use cursio_bluetooth::{
    CallAudio, CallAudioFactory, HandsFreeService, HfCommand, LinkEvent, MediaAudio,
    MediaAudioFactory, MediaSinkService,
};
use cursio_protocol::AudioChunk;

use crate::calls_hub::{hub, CallAction, HandsFreeControl};

/// Stream ids for call audio, clear of the ranges sessions allocate.
const CALL_STREAM_BASE: u32 = 0xCA11_0000;
/// The phone's music over Bluetooth.
const MUSIC_STREAM: u32 = 0xCA11_0010;

struct Voice {
    playback: Playback,
    mic: Capture,
    up: Upsampler,
    down: Downsampler,
    seq: u32,
    /// Microphone PCM at the link's rate, waiting for the phone to take it.
    recorded: std::collections::VecDeque<i16>,
}

impl CallAudio for Voice {
    fn play(&mut self, pcm: &[i16]) {
        let mut data = Vec::with_capacity(pcm.len() * 48);
        self.up.push(pcm, &mut data);
        self.seq = self.seq.wrapping_add(1);
        let chunk = AudioChunk {
            id: CALL_STREAM_BASE,
            seq: self.seq,
            timestamp: 0,
            data,
        };
        if let Err(e) = self.playback.play(&chunk) {
            tracing::debug!(error = %e, "call audio dropped");
        }
    }

    fn record(&mut self, samples: usize, out: &mut Vec<i16>) {
        let mut link = Vec::new();
        while let Ok(chunk) = self.mic.chunks().try_recv() {
            self.down.push(&chunk.data, &mut link);
        }
        self.recorded.extend(link);
        // Never let the microphone run ahead of the call: what the phone has
        // not taken within a fifth of a second is delay, not sound.
        let cap = samples * 25;
        if self.recorded.len() > cap {
            let excess = self.recorded.len() - cap;
            self.recorded.drain(..excess);
        }
        let take = samples.min(self.recorded.len());
        out.extend(self.recorded.drain(..take));
        out.resize(samples, 0);
    }
}

struct Factory;

impl CallAudioFactory for Factory {
    fn open(&self, phone: &str, link_rate: u32) -> Result<Box<dyn CallAudio>, String> {
        let playback = Playback::open(CALL_STREAM_BASE, format!("Cursio call ({phone})"))
            .map_err(|e| format!("cannot play the call here: {e}"))?;
        let mic = Capture::mic_tap(CALL_STREAM_BASE + 1, None)
            .map_err(|e| format!("cannot record the microphone for the call: {e}"))?;
        Ok(Box::new(Voice {
            playback,
            mic,
            up: Upsampler::new(link_rate),
            down: Downsampler::new(link_rate),
            seq: 0,
            recorded: Default::default(),
        }))
    }
}

struct Control(HandsFreeService);

impl HandsFreeControl for Control {
    fn act(&self, phone: &str, action: &CallAction) -> Result<(), String> {
        let command = match action {
            CallAction::Answer => HfCommand::Answer,
            CallAction::Decline | CallAction::HangUp => HfCommand::HangUp,
            CallAction::Dial { number } => HfCommand::Dial(number.clone()),
            CallAction::Mute { muted } => HfCommand::Mute(*muted),
            CallAction::AudioHere { here } => HfCommand::AudioHere(*here),
        };
        self.0.command(phone, command).map_err(|e| e.to_string())
    }
}

/// A phone's music, played like any peer stream here.
struct Music {
    playback: Playback,
    resampler: MusicResampler,
    rate: u32,
    seq: u32,
}

impl MediaAudio for Music {
    fn play(&mut self, pcm: &[i16], rate: u32, channels: u8) {
        if rate != self.rate {
            self.rate = rate;
            self.resampler = MusicResampler::new(rate);
        }
        let mut data = Vec::with_capacity(pcm.len() * 8);
        self.resampler.push(pcm, channels, &mut data);
        self.seq = self.seq.wrapping_add(1);
        let chunk = AudioChunk {
            id: MUSIC_STREAM,
            seq: self.seq,
            timestamp: 0,
            data,
        };
        if let Err(e) = self.playback.play(&chunk) {
            tracing::debug!(error = %e, "phone music dropped");
        }
    }
}

struct MusicFactory;

impl MediaAudioFactory for MusicFactory {
    fn open(&self, _phone: &str, name: &str) -> Result<Box<dyn MediaAudio>, String> {
        let playback = Playback::open(MUSIC_STREAM, format!("Cursio — {name}"))
            .map_err(|e| format!("cannot play the phone's sound here: {e}"))?;
        Ok(Box::new(Music {
            playback,
            resampler: MusicResampler::new(48_000),
            rate: 48_000,
            seq: 0,
        }))
    }
}

/// Be the speaker and hands-free unit of every paired phone, for the life of
/// the process.
///
/// Failing is not fatal: calls still work over the mesh, with the voice on
/// the phone, and the phone's sound can still come by playback capture. The
/// reason is logged once, with what to do about it.
///
/// The registration runs on a thread of its own and is not waited for. Both
/// services talk to BlueZ through zbus's blocking API, which runs its future
/// on the calling thread; called from the node's startup, which is inside the
/// Tokio runtime, that panicked ("cannot start a runtime from within a
/// runtime") on every Linux machine with Bluetooth, and the update that
/// carried it rolled back. See `session_bus` for the same trap.
pub fn start() {
    if let Err(e) = std::thread::Builder::new()
        .name("bluetooth-setup".into())
        .spawn(register)
    {
        tracing::warn!(error = %e, "cannot start Bluetooth setup; calls' voice stays on the phone");
    }
}

fn register() {
    match MediaSinkService::start(Arc::new(MusicFactory)) {
        // Lives as long as the process: dropping it unregisters the speaker.
        Ok(service) => std::mem::forget(service),
        Err(e) => {
            tracing::warn!(error = %e, "phones cannot pick this computer as a Bluetooth speaker")
        }
    }
    let (service, events) = match HandsFreeService::start(Arc::new(Factory)) {
        Ok(started) => started,
        Err(e) => {
            tracing::warn!(error = %e, "calls' voice stays on the phone");
            return;
        }
    };
    hub().set_hands_free_control(Arc::new(Control(service)));
    let spawned = std::thread::Builder::new()
        .name("hfp-events".into())
        .spawn(move || {
            for event in events {
                match event {
                    LinkEvent::Connected { phone, name } => {
                        hub().hands_free_connected(&phone, &name)
                    }
                    LinkEvent::Disconnected { phone } => hub().hands_free_gone(&phone),
                    LinkEvent::Calls { phone, calls } => hub().hands_free_calls(&phone, calls),
                    LinkEvent::Audio { phone, here } => hub().hands_free_audio(&phone, here),
                    LinkEvent::Refused { phone, reason } => {
                        tracing::warn!(%phone, %reason, "a call request failed");
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot follow hands-free links");
    }
}
