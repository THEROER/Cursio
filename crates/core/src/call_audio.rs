//! A phone's call voice on this computer: the Bluetooth hands-free service
//! started, its links filed in the calls hub, and its PCM played and recorded
//! on the same outputs and microphone everything else here uses.
//!
//! The voice plays through an ordinary Cursio playback node and the
//! microphone is read through the same tap that shares it with peers, so the
//! call follows the desktop's default devices (and whatever routing the user
//! applied to Cursio's nodes) instead of a Bluetooth headset appearing and
//! taking them over for the length of the call. `[calls] output`/`input` pick
//! other devices.
//!
//! ## Where the user is, not where the radio is
//!
//! The phone is paired with one computer, and that computer carries the
//! call; the user may be at another. With `[calls] voice = "follow"` the voice
//! plays where [`crate::presence::whereabouts`] says the user is — here, or
//! the peer this computer's mouse is driving — sent there as one of this
//! machine's application streams ([`crate::audio_relay`]) and answered with
//! that peer's microphone. The phone's music over A2DP follows the same way.
//!
//! The answer is looked at twice a second and must hold for [`SETTLE`] before
//! the sound moves: a cursor crossing to a peer and straight back would
//! otherwise tear a call's audio down and up twice. A machine left alone
//! ("away") says nothing about where the user went, so the sound stays put.
//!
//! Moving costs one gap: the new playback or stream opens on the thread that
//! carries the call (a few hundred milliseconds for a PipeWire node), and the
//! Opus hop to a peer adds its own latency — kept low by the audio module's
//! small frames and its place in the `Audio` priority class, ahead of video
//! and files.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cursio_audio::{Capture, Playback};
use cursio_bluetooth::resample::{Downsampler, MusicResampler, Upsampler};
use cursio_bluetooth::{
    CallAudio, CallAudioFactory, HandsFreeService, HfCommand, LinkEvent, MediaAudio,
    MediaAudioFactory, MediaSinkService,
};
use cursio_protocol::{AudioChunk, PeerId};

use crate::audio_relay::{self, RelayHandle};
use crate::calls_hub::{hub, AudioDevice, CallAction, CallSetup, HandsFreeControl, ServiceState};
use crate::config::CallVoice;
use crate::presence::Whereabouts;

/// Stream ids for call audio, clear of the ranges sessions allocate.
const CALL_STREAM_BASE: u32 = 0xCA11_0000;
/// The phone's music over Bluetooth.
const MUSIC_STREAM: u32 = 0xCA11_0010;

/// How often where the user is gets looked at while sound plays.
const LOOK_EVERY: Duration = Duration::from_millis(500);
/// How long a new answer must hold before the sound moves.
pub const SETTLE: Duration = Duration::from_secs(2);
/// How often paired phones and audio devices are read for the settings.
const SETUP_EVERY: Duration = Duration::from_secs(20);

/// A machine sound can play on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Here,
    Peer(PeerId),
}

/// Where the sound is, and where it is about to go.
#[derive(Debug)]
struct Follow {
    current: Target,
    candidate: Option<(Target, Instant)>,
}

impl Follow {
    fn new(current: Target) -> Self {
        Follow {
            current,
            candidate: None,
        }
    }

    /// What was seen now (`None`: nothing to go by); the target to move to,
    /// once a different answer has held for [`SETTLE`].
    fn observe(&mut self, seen: Option<Target>, now: Instant) -> Option<Target> {
        let Some(seen) = seen else {
            self.candidate = None;
            return None;
        };
        if seen == self.current {
            self.candidate = None;
            return None;
        }
        match self.candidate {
            Some((target, since)) if target == seen => {
                if now.duration_since(since) < SETTLE {
                    return None;
                }
                self.current = seen;
                self.candidate = None;
                Some(seen)
            }
            _ => {
                self.candidate = Some((seen, now));
                None
            }
        }
    }
}

/// Where the user is, as a target sound can reach now.
fn user_target(mode: CallVoice) -> Option<Target> {
    if mode == CallVoice::Here {
        return Some(Target::Here);
    }
    match crate::presence::whereabouts() {
        Whereabouts::Here => Some(Target::Here),
        Whereabouts::Peer(peer) if audio_relay::reachable(peer).is_some() => {
            Some(Target::Peer(peer))
        }
        // Driving a peer that runs no audio module: nowhere else to go.
        Whereabouts::Peer(_) => Some(Target::Here),
        Whereabouts::Away => None,
    }
}

/// Following the user, looked at no more often than [`LOOK_EVERY`].
struct Placement {
    mode: CallVoice,
    follow: Follow,
    next_look: Instant,
}

impl Placement {
    fn start(mode: CallVoice) -> (Self, Target) {
        let first = user_target(mode).unwrap_or(Target::Here);
        let placement = Placement {
            mode,
            follow: Follow::new(first),
            next_look: Instant::now() + LOOK_EVERY,
        };
        (placement, first)
    }

    fn moved(&mut self) -> Option<Target> {
        let now = Instant::now();
        if now < self.next_look {
            return None;
        }
        self.next_look = now + LOOK_EVERY;
        self.follow.observe(user_target(self.mode), now)
    }
}

/// Where a call's voice is played and recorded now.
enum VoiceSink {
    Here { playback: Playback, mic: Capture },
    Peer { relay: RelayHandle },
}

struct Voice {
    address: String,
    phone: String,
    output: Option<String>,
    input: Option<String>,
    sink: Option<VoiceSink>,
    placement: Placement,
    up: Upsampler,
    down: Downsampler,
    seq: u32,
    /// Microphone PCM at the link's rate, waiting for the phone to take it.
    recorded: std::collections::VecDeque<i16>,
}

impl Voice {
    /// Put the voice on `target`, closing where it was first.
    fn go(&mut self, target: Target) -> Result<(), String> {
        self.sink = None;
        self.recorded.clear();
        match target {
            Target::Here => {
                let playback = Playback::open_to(
                    CALL_STREAM_BASE,
                    format!("Cursio call ({})", self.phone),
                    self.output.clone(),
                )
                .map_err(|e| format!("cannot play the call here: {e}"))?;
                let mic = Capture::mic_tap(CALL_STREAM_BASE + 1, self.input.clone())
                    .map_err(|e| format!("cannot record the microphone for the call: {e}"))?;
                self.sink = Some(VoiceSink::Here { playback, mic });
                hub().voice_sent_to(&self.address, None);
            }
            Target::Peer(peer) => {
                let name = audio_relay::reachable(peer).unwrap_or_else(|| peer.to_string());
                tracing::info!(phone = %self.phone, to = %name, "the call's voice follows the user");
                let relay = audio_relay::open(peer, &audio_relay::call_label(&self.phone), true);
                self.sink = Some(VoiceSink::Peer { relay });
                hub().voice_sent_to(&self.address, Some(name));
            }
        }
        Ok(())
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        hub().voice_sent_to(&self.address, None);
    }
}

impl CallAudio for Voice {
    fn play(&mut self, pcm: &[i16]) {
        if let Some(target) = self.placement.moved() {
            if let Err(e) = self.go(target) {
                tracing::warn!(error = %e, "the call's voice could not move; it plays nowhere until it can");
            }
        }
        let mut data = Vec::with_capacity(pcm.len() * 48);
        self.up.push(pcm, &mut data);
        self.seq = self.seq.wrapping_add(1);
        let chunk = AudioChunk {
            id: CALL_STREAM_BASE,
            seq: self.seq,
            timestamp: 0,
            data,
        };
        match &self.sink {
            Some(VoiceSink::Here { playback, .. }) => {
                if let Err(e) = playback.play(&chunk) {
                    tracing::debug!(error = %e, "call audio dropped");
                }
            }
            Some(VoiceSink::Peer { relay }) => relay.play(chunk),
            None => {}
        }
    }

    fn record(&mut self, samples: usize, out: &mut Vec<i16>) {
        let mut link = Vec::new();
        match &mut self.sink {
            Some(VoiceSink::Here { mic, .. }) => {
                while let Ok(chunk) = mic.chunks().try_recv() {
                    self.down.push(&chunk.data, &mut link);
                }
            }
            Some(VoiceSink::Peer { relay }) => {
                while let Ok(chunk) = relay.mic().try_recv() {
                    self.down.push(&chunk.data, &mut link);
                }
            }
            None => {}
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
        let settings = hub().settings();
        let (placement, first) = Placement::start(settings.voice);
        let mut voice = Voice {
            address: phone.to_string(),
            phone: hub()
                .hands_free_name(phone)
                .unwrap_or_else(|| phone.to_string()),
            output: settings.output,
            input: settings.input,
            sink: None,
            placement,
            up: Upsampler::new(link_rate),
            down: Downsampler::new(link_rate),
            seq: 0,
            recorded: Default::default(),
        };
        voice.go(first)?;
        Ok(Box::new(voice))
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

/// Where a phone's music plays now.
enum MusicSink {
    Here(Playback),
    Peer(RelayHandle),
}

/// A phone's music, played like any peer stream here or on the machine the
/// user is at.
struct Music {
    name: String,
    output: Option<String>,
    sink: Option<MusicSink>,
    placement: Placement,
    resampler: MusicResampler,
    rate: u32,
    seq: u32,
}

impl Music {
    fn go(&mut self, target: Target) -> Result<(), String> {
        self.sink = None;
        self.sink = Some(match target {
            Target::Here => MusicSink::Here(
                Playback::open_to(
                    MUSIC_STREAM,
                    format!("Cursio — {}", self.name),
                    self.output.clone(),
                )
                .map_err(|e| format!("cannot play the phone's sound here: {e}"))?,
            ),
            Target::Peer(peer) => MusicSink::Peer(audio_relay::open(
                peer,
                &audio_relay::phone_sound_label(&self.name),
                false,
            )),
        });
        Ok(())
    }
}

impl MediaAudio for Music {
    fn play(&mut self, pcm: &[i16], rate: u32, channels: u8) {
        if let Some(target) = self.placement.moved() {
            if let Err(e) = self.go(target) {
                tracing::warn!(error = %e, "the phone's sound could not move");
            }
        }
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
        match &self.sink {
            Some(MusicSink::Here(playback)) => {
                if let Err(e) = playback.play(&chunk) {
                    tracing::debug!(error = %e, "phone music dropped");
                }
            }
            Some(MusicSink::Peer(relay)) => relay.play(chunk),
            None => {}
        }
    }
}

struct MusicFactory;

impl MediaAudioFactory for MusicFactory {
    fn open(&self, _phone: &str, name: &str) -> Result<Box<dyn MediaAudio>, String> {
        let settings = hub().settings();
        let (placement, first) = Placement::start(settings.voice);
        let mut music = Music {
            name: name.to_string(),
            output: settings.output,
            sink: None,
            placement,
            resampler: MusicResampler::new(48_000),
            rate: 48_000,
            seq: 0,
        };
        music.go(first)?;
        Ok(Box::new(music))
    }
}

/// What the settings show about this computer as a phone's headset, kept
/// fresh by the setup thread.
fn setup() -> &'static Mutex<CallSetup> {
    static SETUP: OnceLock<Mutex<CallSetup>> = OnceLock::new();
    SETUP.get_or_init(Default::default)
}

fn update_setup(change: impl FnOnce(&mut CallSetup)) {
    let mut setup = setup().lock().unwrap_or_else(|p| p.into_inner());
    change(&mut setup);
    hub().set_setup(setup.clone());
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
    update_setup(|_| {});
    if let Err(e) = std::thread::Builder::new()
        .name("bluetooth-setup".into())
        .spawn(register)
    {
        tracing::warn!(error = %e, "cannot start Bluetooth setup; calls' voice stays on the phone");
    }
    if let Err(e) = std::thread::Builder::new()
        .name("call-setup".into())
        .spawn(watch_setup)
    {
        tracing::warn!(error = %e, "cannot read paired phones for the settings");
    }
}

/// Paired phones and audio devices, read again every [`SETUP_EVERY`]: none of
/// them announces a change this side listens to, and the settings page is the
/// only reader.
fn watch_setup() {
    loop {
        let phones = cursio_bluetooth::paired_phones().unwrap_or_else(|e| {
            tracing::debug!(error = %e, "cannot list paired phones");
            Vec::new()
        });
        let devices = |list: Result<Vec<cursio_audio::OutputDevice>, _>| {
            list.map(|devices| {
                devices
                    .into_iter()
                    .map(|d| AudioDevice {
                        id: d.id,
                        name: d.description,
                    })
                    .collect()
            })
            .unwrap_or_default()
        };
        let outputs = devices(cursio_audio::list_output_devices());
        let inputs = devices(cursio_audio::list_input_devices());
        #[cfg(target_os = "linux")]
        let roles_taken = crate::bluetooth_setup::installed();
        #[cfg(not(target_os = "linux"))]
        let roles_taken = false;
        update_setup(|s| {
            s.roles_taken = roles_taken;
            s.phones = phones
                .into_iter()
                .map(|p| crate::calls_hub::PairedPhone {
                    name: p.name,
                    address: p.address,
                    connected: p.connected,
                })
                .collect();
            s.outputs = outputs;
            s.inputs = inputs;
        });
        std::thread::sleep(SETUP_EVERY);
    }
}

fn register() {
    match MediaSinkService::start(Arc::new(MusicFactory)) {
        // Lives as long as the process: dropping it unregisters the speaker.
        Ok(service) => {
            std::mem::forget(service);
            update_setup(|s| s.speaker = ServiceState::Running);
        }
        Err(e) => {
            tracing::warn!(error = %e, "phones cannot pick this computer as a Bluetooth speaker");
            update_setup(|s| {
                s.speaker = ServiceState::Failed {
                    reason: e.to_string(),
                }
            });
        }
    }
    let (service, events) = match HandsFreeService::start(Arc::new(Factory)) {
        Ok(started) => {
            update_setup(|s| s.hands_free = ServiceState::Running);
            started
        }
        Err(e) => {
            tracing::warn!(error = %e, "calls' voice stays on the phone");
            update_setup(|s| {
                s.hands_free = ServiceState::Failed {
                    reason: e.to_string(),
                }
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sound_moves_only_once_a_new_place_has_held() {
        let peer = Target::Peer(PeerId::from_u128(7));
        let mut follow = Follow::new(Target::Here);
        let t0 = Instant::now();
        assert_eq!(follow.observe(Some(peer), t0), None);
        assert_eq!(follow.observe(Some(peer), t0 + SETTLE / 2), None);
        assert_eq!(follow.observe(Some(peer), t0 + SETTLE), Some(peer));
        assert_eq!(follow.observe(Some(peer), t0 + SETTLE * 2), None);
    }

    #[test]
    fn a_cursor_that_crosses_and_comes_straight_back_moves_nothing() {
        let peer = Target::Peer(PeerId::from_u128(7));
        let mut follow = Follow::new(Target::Here);
        let t0 = Instant::now();
        follow.observe(Some(peer), t0);
        follow.observe(Some(Target::Here), t0 + SETTLE / 2);
        assert_eq!(follow.observe(Some(peer), t0 + SETTLE), None);
        assert_eq!(follow.observe(Some(peer), t0 + SETTLE * 2), Some(peer));
    }

    #[test]
    fn a_user_who_left_leaves_the_sound_where_it_was() {
        let peer = Target::Peer(PeerId::from_u128(7));
        let mut follow = Follow::new(peer);
        let t0 = Instant::now();
        assert_eq!(follow.observe(None, t0), None);
        assert_eq!(follow.observe(None, t0 + SETTLE * 3), None);
        assert_eq!(follow.current, peer);
    }

    #[test]
    fn with_the_voice_kept_here_where_the_user_is_does_not_matter() {
        assert_eq!(user_target(CallVoice::Here), Some(Target::Here));
    }
}
