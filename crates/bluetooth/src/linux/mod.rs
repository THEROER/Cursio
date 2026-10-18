//! The hands-free unit on BlueZ.
//!
//! BlueZ owns the adapter and SDP; an external profile registered with
//! `ProfileManager1.RegisterProfile` is handed each connected phone's RFCOMM
//! socket in `Profile1.NewConnection`. From there everything is ours: one
//! thread per phone runs [`HandsFree`] over that socket, and while a call is
//! up an SCO thread carries its voice.
//!
//! Plain threads, not the node's runtime: zbus's blocking calls deadlock on a
//! runtime thread (see `cursio_core::session_bus`), and both sockets are
//! blocking file descriptors read under `poll`.

mod channel;
mod devices;
mod media;
mod sbc;
mod sco;

pub use channel::MeshChannel;
pub use devices::paired_phones;
pub use media::MediaSinkService;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use zbus::blocking::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::hands_free::{Codec, HandsFree, HfEvent, HfOutput};
use crate::{BluetoothError, CallAudioFactory, HfCommand, LinkEvent};

const PROFILE_PATH: &str = "/dev/cursio/handsfree";
/// Hands-Free unit, the role a headset plays.
const HFP_HF_UUID: &str = "0000111e-0000-1000-8000-00805f9b34fb";
/// HFP 1.7, the version whose `+CLCC` and indicator handling this speaks.
const HFP_VERSION: u16 = 0x0107;
/// The SDP record's features: CLI presentation (bit 2) and remote volume
/// (bit 4).
const SDP_FEATURES: u16 = (1 << 2) | (1 << 4);
/// Wideband speech, in the record only where libsbc can carry it; see
/// [`crate::hands_free`].
const SDP_WIDEBAND: u16 = 1 << 5;
/// A wideband link that has given no readable frame in this long is taken to
/// be one the adapter cannot carry.
const WIDEBAND_PROOF: std::time::Duration = std::time::Duration::from_secs(2);

struct Link {
    commands: Sender<HfCommand>,
}

#[derive(Clone)]
struct Shared {
    links: Arc<Mutex<HashMap<String, Link>>>,
    events: Sender<LinkEvent>,
    audio: Arc<dyn CallAudioFactory>,
    bus: Connection,
}

impl Shared {
    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<String, Link>> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// This machine as the hands-free unit of every paired phone that connects.
pub struct HandsFreeService {
    shared: Shared,
}

struct Profile {
    shared: Arc<Mutex<Option<Shared>>>,
}

#[zbus::interface(name = "org.bluez.Profile1")]
impl Profile {
    fn release(&self) {
        tracing::info!("bluez released the hands-free profile");
    }

    fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: zbus::zvariant::OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) {
        let shared = self
            .shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(shared) = shared else { return };
        let Some(phone) = address_from_path(device.as_str()) else {
            tracing::warn!(device = %device.as_str(), "hands-free connection from a path that names no device");
            return;
        };
        let fd: OwnedFd = fd.into();
        let adapter = adapter_from_path(device.as_str());
        let spawned = std::thread::Builder::new()
            .name(format!("hfp-{phone}"))
            .spawn(move || run_link(shared, phone, adapter, device, fd));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot start the hands-free link thread");
        }
    }

    fn request_disconnection(&self, device: OwnedObjectPath) {
        let shared = self
            .shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let (Some(shared), Some(phone)) = (shared, address_from_path(device.as_str())) {
            shared.links().remove(&phone);
        }
    }
}

impl HandsFreeService {
    /// Register with BlueZ and take every phone that connects from now on.
    pub fn start(
        audio: Arc<dyn CallAudioFactory>,
    ) -> Result<(Self, Receiver<LinkEvent>), BluetoothError> {
        let slot = Arc::new(Mutex::new(None));
        let bus = zbus::blocking::connection::Builder::system()
            .and_then(|b| {
                b.serve_at(
                    PROFILE_PATH,
                    Profile {
                        shared: slot.clone(),
                    },
                )
            })
            .and_then(|b| b.build())
            .map_err(|e| BluetoothError::Bus(e.to_string()))?;
        let (events, events_rx) = mpsc::channel();
        let shared = Shared {
            links: Arc::default(),
            events,
            audio,
            bus: bus.clone(),
        };
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(shared.clone());

        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("Name", Value::from("Cursio hands-free"));
        options.insert("Version", Value::U16(HFP_VERSION));
        let features = if sbc::msbc_available() {
            SDP_FEATURES | SDP_WIDEBAND
        } else {
            SDP_FEATURES
        };
        options.insert("Features", Value::U16(features));
        options.insert("RequireAuthentication", Value::Bool(true));
        options.insert("AutoConnect", Value::Bool(true));
        let path = ObjectPath::try_from(PROFILE_PATH).expect("constant path is valid");
        bus.call_method(
            Some("org.bluez"),
            "/org/bluez",
            Some("org.bluez.ProfileManager1"),
            "RegisterProfile",
            &(path, HFP_HF_UUID, options),
        )
        .map_err(|e| BluetoothError::Register(e.to_string()))?;
        tracing::info!("registered as the hands-free unit for paired phones");
        Ok((HandsFreeService { shared }, events_rx))
    }

    pub fn command(&self, phone: &str, command: HfCommand) -> Result<(), BluetoothError> {
        let links = self.shared.links();
        let link = links
            .get(phone)
            .ok_or_else(|| BluetoothError::NotConnected(phone.to_string()))?;
        link.commands
            .send(command)
            .map_err(|_| BluetoothError::NotConnected(phone.to_string()))
    }

    /// Ask BlueZ to bring up the hands-free link to a paired phone that is in
    /// range but not connected.
    pub fn connect(&self, phone: &str) -> Result<(), BluetoothError> {
        let path = format!("/org/bluez/hci0/dev_{}", phone.replace(':', "_"));
        self.shared
            .bus
            .call_method(
                Some("org.bluez"),
                path.as_str(),
                Some("org.bluez.Device1"),
                "ConnectProfile",
                &(HFP_HF_UUID,),
            )
            .map(|_| ())
            .map_err(|e| BluetoothError::Bus(e.to_string()))
    }
}

/// `/org/bluez/hci0/dev_9C_9E_D5_78_FE_C4` → `9C:9E:D5:78:FE:C4`.
fn address_from_path(path: &str) -> Option<String> {
    let dev = path.rsplit('/').next()?.strip_prefix("dev_")?;
    let address = dev.replace('_', ":");
    (address.len() == 17).then_some(address)
}

fn adapter_from_path(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(adapter, _)| adapter.to_string())
        .unwrap_or_else(|| "/org/bluez/hci0".into())
}

fn property(bus: &Connection, path: &str, interface: &str, name: &str) -> Option<String> {
    let reply = bus
        .call_method(
            Some("org.bluez"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(interface, name),
        )
        .ok()?;
    let value: OwnedValue = reply.body().deserialize().ok()?;
    String::try_from(value).ok()
}

/// What the link thread is doing with the call's voice.
struct Voice {
    stop: Sender<()>,
}

/// The major device class of a Class of Device: bits 8–12.
const MAJOR_CLASS_SHIFT: u32 = 8;
const MAJOR_CLASS_MASK: u32 = 0x1f;
const MAJOR_CLASS_PHONE: u32 = 0x02;

/// Whether a device of Bluetooth Class of Device `class` may be taken as a
/// phone. A laptop can play the gateway side of hands-free too — Windows does,
/// for its own Phone Link — and taken as a phone it showed up among the
/// calls with nothing to call. A device that gives no class is let through:
/// refusing it would refuse every phone whose stack leaves the field out.
fn takes_calls(class: Option<u32>) -> bool {
    class.is_none_or(|c| (c >> MAJOR_CLASS_SHIFT) & MAJOR_CLASS_MASK == MAJOR_CLASS_PHONE)
}

fn device_class(bus: &Connection, path: &str) -> Option<u32> {
    let reply = bus
        .call_method(
            Some("org.bluez"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.bluez.Device1", "Class"),
        )
        .ok()?;
    let value: OwnedValue = reply.body().deserialize().ok()?;
    u32::try_from(value).ok()
}

/// What a voice thread shares with its link's thread.
#[derive(Clone)]
struct VoiceLink {
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

fn run_link(shared: Shared, phone: String, adapter: String, device: OwnedObjectPath, fd: OwnedFd) {
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
fn start_voice(
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

fn stopped(stop: &Receiver<()>) -> bool {
    !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty))
}

fn wait_for_sco(
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
fn carry_voice(
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

fn carry_cvsd(
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
fn carry_msbc(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_path_names_its_address_and_adapter() {
        let path = "/org/bluez/hci0/dev_9C_9E_D5_78_FE_C4";
        assert_eq!(
            address_from_path(path).as_deref(),
            Some("9C:9E:D5:78:FE:C4")
        );
        assert_eq!(adapter_from_path(path), "/org/bluez/hci0");
        assert_eq!(address_from_path("/org/bluez/hci0"), None);
    }

    #[test]
    fn only_a_phone_class_device_is_taken_as_a_phone() {
        // A smartphone (major class Phone) and a laptop (major class Computer)
        // as BlueZ reports them.
        assert!(takes_calls(Some(0x5a020c)));
        assert!(!takes_calls(Some(0x2a010c)));
        assert!(takes_calls(None));
    }
}
