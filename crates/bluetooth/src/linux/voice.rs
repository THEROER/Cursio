//! A phone's hands-free link once it is up: the AT dialogue on RFCOMM, and the
//! call's voice over SCO, carried as CVSD or mSBC to and from this computer's
//! own devices.

use crate::hands_free::Codec;
use crate::hands_free::HandsFree;
use crate::hands_free::HfEvent;
use crate::hands_free::HfOutput;
use crate::linux::device_class;
use crate::linux::property;
use crate::linux::sbc;
use crate::linux::sco;
use crate::linux::takes_calls;
use crate::linux::Link;
use crate::linux::Shared;
use crate::HfCommand;
use crate::LinkEvent;
use std::io::Read;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use zbus::zvariant::OwnedObjectPath;

/// A wideband link that has given no readable frame in this long is taken to
/// be one the adapter cannot carry.
pub(super) const WIDEBAND_PROOF: std::time::Duration = std::time::Duration::from_secs(2);

/// What the link thread is doing with the call's voice.
pub(super) struct Voice {
    stop: Sender<()>,
}

/// What a voice thread shares with its link's thread.
#[derive(Clone)]
pub(super) struct VoiceLink {
    muted: Arc<AtomicBool>,
    /// The codec the phone last chose, read when the audio link arrives.
    codec: Arc<AtomicU8>,
    /// Whether mSBC is offered, so an incoming link must be set up in the
    /// air mode the phone chose before it is accepted.
    wideband: bool,
    /// Set by a voice thread whose wideband link carried nothing readable.
    unreadable: Arc<AtomicBool>,
}

impl VoiceLink {
    fn codec(&self) -> Codec {
        match self.codec.load(Ordering::Relaxed) {
            1 => Codec::Msbc,
            _ => Codec::Cvsd,
        }
    }

    fn set_codec(&self, codec: Codec) {
        self.codec
            .store(u8::from(codec == Codec::Msbc), Ordering::Relaxed);
    }
}

pub(super) fn run_link(
    shared: Shared,
    phone: String,
    adapter: String,
    device: OwnedObjectPath,
    fd: OwnedFd,
) {
    let name = property(&shared.bus, device.as_str(), "org.bluez.Device1", "Alias")
        .unwrap_or_else(|| phone.clone());
    let class = device_class(&shared.bus, device.as_str());
    if !takes_calls(class) {
        // Dropping the socket refuses the link; the device's own stack sees
        // the headset go away.
        tracing::info!(%phone, %name, class = ?class, "not a phone; refusing its hands-free link");
        drop(fd);
        return;
    }
    let local = property(&shared.bus, &adapter, "org.bluez.Adapter1", "Address")
        .and_then(|a| sco::bdaddr(&a).ok());
    let (commands, commands_rx) = mpsc::channel();
    shared.links().insert(phone.clone(), Link { commands });
    tracing::info!(%phone, %name, "phone connected as hands-free");

    let mut file = std::fs::File::from(fd);
    let wideband = sbc::msbc_available();
    let mut hf = HandsFree::with_wideband(wideband);
    let mut pending = String::new();
    let mut voice: Option<Voice> = None;
    let link = VoiceLink {
        muted: Arc::default(),
        codec: Arc::default(),
        wideband,
        unreadable: Arc::default(),
    };
    let mut want_here = true;
    let mut buf = [0u8; 1024];
    let mut announced = false;

    let mut outputs = hf.start();
    let result: std::io::Result<()> = (|| {
        let probe = OwnedFd::from(file.try_clone()?);
        loop {
            for output in outputs.drain(..) {
                match output {
                    HfOutput::Send(line) => {
                        tracing::trace!(%phone, line = line.trim_end(), "to phone");
                        file.write_all(line.as_bytes())?;
                    }
                    HfOutput::Event(HfEvent::Connected) => {
                        announced = true;
                        let _ = shared.events.send(LinkEvent::Connected {
                            phone: phone.clone(),
                            name: name.clone(),
                        });
                    }
                    HfOutput::Event(HfEvent::Calls(calls)) => {
                        let call_up = !calls.is_empty();
                        let _ = shared.events.send(LinkEvent::Calls {
                            phone: phone.clone(),
                            calls,
                        });
                        match (call_up, voice.is_some()) {
                            (true, false) if want_here => {
                                voice = local.and_then(|local| {
                                    start_voice(&shared, &phone, local, None, link.clone())
                                });
                            }
                            (false, true) => {
                                voice = None;
                                want_here = true;
                            }
                            _ => {}
                        }
                    }
                    HfOutput::Event(HfEvent::Codec(codec)) => {
                        tracing::debug!(%phone, ?codec, "the phone chose the call's codec");
                        link.set_codec(codec);
                    }
                    HfOutput::Event(HfEvent::Refused(reason)) => {
                        let _ = shared.events.send(LinkEvent::Refused {
                            phone: phone.clone(),
                            reason,
                        });
                    }
                }
            }
            while let Ok(command) = commands_rx.try_recv() {
                match command {
                    HfCommand::Answer => outputs.extend(hf.answer()),
                    HfCommand::HangUp => outputs.extend(hf.hang_up()),
                    HfCommand::Dial(number) => outputs.extend(hf.dial(&number)),
                    HfCommand::Mute(on) => link.muted.store(on, Ordering::Relaxed),
                    HfCommand::AudioHere(here) => {
                        want_here = here;
                        if !here {
                            // Closing our end of SCO is how a headset hands the
                            // voice back: the phone keeps the call on itself.
                            voice = None;
                        } else if voice.is_none() && !hf.calls().is_empty() {
                            // A phone that negotiates codecs opens the link
                            // itself once asked, in the codec it picks.
                            let remote = match hf.request_audio() {
                                Some(asked) => {
                                    outputs.extend(asked);
                                    None
                                }
                                None => sco::bdaddr(&phone).ok(),
                            };
                            voice = local.and_then(|local| {
                                start_voice(&shared, &phone, local, remote, link.clone())
                            });
                        }
                    }
                }
            }
            if link.unreadable.swap(false, Ordering::Relaxed) {
                tracing::warn!(%phone, "the wideband audio link carried nothing readable; calls go narrowband from the next one");
                outputs.extend(hf.drop_wideband());
                link.set_codec(Codec::Cvsd);
            }
            if !outputs.is_empty() {
                continue;
            }
            if sco::readable(&probe, 100)? {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                let text = String::from_utf8_lossy(&buf[..n]);
                for line in crate::at::split_lines(&mut pending, &text) {
                    tracing::trace!(%phone, %line, "from phone");
                    outputs.extend(hf.feed(&line));
                }
            }
        }
    })();

    drop(voice);
    shared.links().remove(&phone);
    match result {
        Ok(()) => tracing::info!(%phone, "hands-free link closed"),
        Err(e) => tracing::info!(%phone, error = %e, "hands-free link ended"),
    }
    if announced {
        let _ = shared.events.send(LinkEvent::Disconnected { phone });
    }
}

/// Carry the call's voice until the returned handle is dropped.
///
/// With `remote` set this side opens the SCO link (bringing the voice back
/// from the phone); otherwise it waits for the phone to open it, which is how
/// a call's audio normally starts.
pub(super) fn start_voice(
    shared: &Shared,
    phone: &str,
    local: [u8; 6],
    remote: Option<[u8; 6]>,
    link: VoiceLink,
) -> Option<Voice> {
    let (stop, stop_rx) = mpsc::channel();
    let shared = shared.clone();
    let phone = phone.to_string();
    let spawned = std::thread::Builder::new()
        .name(format!("sco-{phone}"))
        .spawn(move || {
            let socket = match remote {
                Some(remote) => sco::connect(local, remote, link.codec() == Codec::Msbc).map(Some),
                None => wait_for_sco(local, &phone, &stop_rx, &link),
            };
            let socket = match socket {
                Ok(Some(socket)) => socket,
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!(%phone, error = %e, "the call's audio link did not open");
                    let _ = shared.events.send(LinkEvent::Refused {
                        phone,
                        reason: format!("the call's audio did not reach this computer: {e}"),
                    });
                    return;
                }
            };
            let _ = shared.events.send(LinkEvent::Audio {
                phone: phone.clone(),
                here: true,
            });
            if let Err(e) = carry_voice(&shared, &phone, socket, &stop_rx, &link) {
                tracing::info!(%phone, error = %e, "the call's audio link ended");
            }
            let _ = shared.events.send(LinkEvent::Audio { phone, here: false });
        });
    match spawned {
        Ok(_) => Some(Voice { stop }),
        Err(e) => {
            tracing::warn!(error = %e, "cannot start the call audio thread");
            None
        }
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

pub(super) fn stopped(stop: &Receiver<()>) -> bool {
    !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty))
}

pub(super) fn wait_for_sco(
    local: [u8; 6],
    phone: &str,
    stop: &Receiver<()>,
    link: &VoiceLink,
) -> std::io::Result<Option<OwnedFd>> {
    let listener = sco::listen(local, link.wideband)?;
    loop {
        if stopped(stop) {
            return Ok(None);
        }
        match sco::accept(&listener, 200)? {
            Some((socket, from)) if from == phone => {
                if link.wideband {
                    // Deferred: the air mode is the codec the phone chose
                    // with `+BCS`, which by now has arrived.
                    sco::set_transparent(&socket, link.codec() == Codec::Msbc)?;
                    sco::authorize(&socket)?;
                }
                return Ok(Some(socket));
            }
            Some((_, from)) => {
                // Taken from PipeWire's wildcard listener because ours is
                // more specific; it is a headset of this computer's, and
                // nothing here can hand it back. Said, not swallowed.
                tracing::warn!(%from, "refused an audio link from a device other than the phone in a call");
            }
            None => {}
        }
    }
}

/// Read a packet, play it, write as much microphone back: the phone's clock
/// paces both directions, so neither drifts against the other.
pub(super) fn carry_voice(
    shared: &Shared,
    phone: &str,
    socket: OwnedFd,
    stop: &Receiver<()>,
    link: &VoiceLink,
) -> std::io::Result<()> {
    let codec = link.codec();
    let mtu = sco::mtu(&socket);
    let audio = shared
        .audio
        .open(phone, codec.rate())
        .map_err(std::io::Error::other)?;
    tracing::info!(%phone, mtu, ?codec, "call audio flowing through this computer");
    match codec {
        Codec::Cvsd => carry_cvsd(audio, socket, mtu, stop, &link.muted),
        Codec::Msbc => carry_msbc(audio, socket, mtu, stop, link),
    }
}

pub(super) fn carry_cvsd(
    mut audio: Box<dyn crate::CallAudio>,
    socket: OwnedFd,
    mtu: usize,
    stop: &Receiver<()>,
    muted: &AtomicBool,
) -> std::io::Result<()> {
    let mut file = std::fs::File::from(socket);
    let probe = OwnedFd::from(file.try_clone()?);
    let mut packet = vec![0u8; mtu.max(48)];
    let mut pcm = Vec::new();
    let mut mic = Vec::new();
    let mut out = Vec::new();
    loop {
        if stopped(stop) {
            return Ok(());
        }
        if !sco::readable(&probe, 100)? {
            continue;
        }
        let n = file.read(&mut packet)?;
        if n == 0 {
            return Ok(());
        }
        pcm.clear();
        pcm.extend(
            packet[..n]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b)),
        );
        audio.play(&pcm);
        mic.clear();
        audio.record(pcm.len(), &mut mic);
        if muted.load(Ordering::Relaxed) {
            mic.iter_mut().for_each(|s| *s = 0);
        }
        out.clear();
        out.extend(mic.iter().flat_map(|s| s.to_le_bytes()));
        out.resize(n - n % 2, 0);
        file.write_all(&out)?;
    }
}

/// The wideband counterpart: whole frames are found in what arrives, each
/// decoded frame is answered with one encoded frame of microphone, and the
/// answer leaves in writes of the size the phone sends, which keeps both
/// directions on the phone's clock as in CVSD.
///
/// A frame that does not decode is played as silence of its length rather
/// than skipped, so the call's timing holds across a lost packet.
pub(super) fn carry_msbc(
    mut audio: Box<dyn crate::CallAudio>,
    socket: OwnedFd,
    mtu: usize,
    stop: &Receiver<()>,
    link: &VoiceLink,
) -> std::io::Result<()> {
    use crate::msbc::{self, FRAME_SAMPLES};

    let mut codec = sbc::Msbc::new().map_err(std::io::Error::other)?;
    let mut file = std::fs::File::from(socket);
    let probe = OwnedFd::from(file.try_clone()?);
    let mut packet = vec![0u8; mtu.max(msbc::PACKET_LEN)];
    let mut frames = msbc::Reassembler::default();
    let mut outgoing = std::collections::VecDeque::<u8>::new();
    let mut pcm = Vec::with_capacity(FRAME_SAMPLES);
    let mut mic = Vec::with_capacity(FRAME_SAMPLES);
    let mut sent = 0usize;
    let mut readable = 0usize;
    let started = std::time::Instant::now();
    loop {
        if stopped(stop) {
            return Ok(());
        }
        if !sco::readable(&probe, 100)? {
            continue;
        }
        let n = file.read(&mut packet)?;
        if n == 0 {
            return Ok(());
        }
        frames.push(&packet[..n]);
        while let Some(frame) = frames.next_frame() {
            pcm.clear();
            if codec.decode(&frame, &mut pcm) {
                readable += 1;
            } else {
                pcm.resize(FRAME_SAMPLES, 0);
            }
            audio.play(&pcm);
            mic.clear();
            audio.record(FRAME_SAMPLES, &mut mic);
            if link.muted.load(Ordering::Relaxed) {
                mic.iter_mut().for_each(|s| *s = 0);
            }
            if let Some(encoded) = codec.encode(&mic) {
                outgoing.extend(msbc::packet(sent, &encoded));
                sent += 1;
            }
        }
        if readable == 0 && started.elapsed() > WIDEBAND_PROOF {
            link.unreadable.store(true, Ordering::Relaxed);
            return Err(std::io::Error::other(
                "no mSBC frame could be read from the audio link",
            ));
        }
        if outgoing.len() >= n {
            let out: Vec<u8> = outgoing.drain(..n).collect();
            file.write_all(&out)?;
        }
    }
}
