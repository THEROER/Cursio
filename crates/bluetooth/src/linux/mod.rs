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

mod sco;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use zbus::blocking::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::hands_free::{HandsFree, HfEvent, HfOutput};
use crate::{BluetoothError, CallAudioFactory, HfCommand, LinkEvent};

const PROFILE_PATH: &str = "/dev/cursio/handsfree";
/// Hands-Free unit, the role a headset plays.
const HFP_HF_UUID: &str = "0000111e-0000-1000-8000-00805f9b34fb";
/// HFP 1.7, the version whose `+CLCC` and indicator handling this speaks.
const HFP_VERSION: u16 = 0x0107;
/// The SDP record's features: CLI presentation (bit 2) and remote volume
/// (bit 4). Wideband (bit 5) is left off with codec negotiation; see
/// [`crate::hands_free`].
const SDP_FEATURES: u16 = (1 << 2) | (1 << 4);
/// CVSD's rate on the SCO socket.
const CVSD_RATE: u32 = 8_000;

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
        options.insert("Features", Value::U16(SDP_FEATURES));
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

fn run_link(shared: Shared, phone: String, adapter: String, device: OwnedObjectPath, fd: OwnedFd) {
    let name = property(&shared.bus, device.as_str(), "org.bluez.Device1", "Alias")
        .unwrap_or_else(|| phone.clone());
    let local = property(&shared.bus, &adapter, "org.bluez.Adapter1", "Address")
        .and_then(|a| sco::bdaddr(&a).ok());
    let (commands, commands_rx) = mpsc::channel();
    shared.links().insert(phone.clone(), Link { commands });
    tracing::info!(%phone, %name, "phone connected as hands-free");

    let mut file = std::fs::File::from(fd);
    let mut hf = HandsFree::new();
    let mut pending = String::new();
    let mut voice: Option<Voice> = None;
    let muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
                                    start_voice(&shared, &phone, local, None, muted.clone())
                                });
                            }
                            (false, true) => {
                                voice = None;
                                want_here = true;
                            }
                            _ => {}
                        }
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
                    HfCommand::Mute(on) => muted.store(on, std::sync::atomic::Ordering::Relaxed),
                    HfCommand::AudioHere(here) => {
                        want_here = here;
                        if !here {
                            // Closing our end of SCO is how a headset hands the
                            // voice back: the phone keeps the call on itself.
                            voice = None;
                        } else if voice.is_none() && !hf.calls().is_empty() {
                            let remote = sco::bdaddr(&phone).ok();
                            voice = local.and_then(|local| {
                                start_voice(&shared, &phone, local, remote, muted.clone())
                            });
                        }
                    }
                }
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
    muted: Arc<std::sync::atomic::AtomicBool>,
) -> Option<Voice> {
    let (stop, stop_rx) = mpsc::channel();
    let shared = shared.clone();
    let phone = phone.to_string();
    let spawned = std::thread::Builder::new()
        .name(format!("sco-{phone}"))
        .spawn(move || {
            let socket = match remote {
                Some(remote) => sco::connect(local, remote).map(Some),
                None => wait_for_sco(local, &phone, &stop_rx),
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
            if let Err(e) = carry_voice(&shared, &phone, socket, &stop_rx, &muted) {
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
) -> std::io::Result<Option<OwnedFd>> {
    let listener = sco::listen(local)?;
    loop {
        if stopped(stop) {
            return Ok(None);
        }
        match sco::accept(&listener, 200)? {
            Some((socket, from)) if from == phone => return Ok(Some(socket)),
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
    muted: &std::sync::atomic::AtomicBool,
) -> std::io::Result<()> {
    let mtu = sco::mtu(&socket);
    let mut audio = shared
        .audio
        .open(phone, CVSD_RATE)
        .map_err(std::io::Error::other)?;
    tracing::info!(%phone, mtu, "call audio flowing through this computer");
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
        if muted.load(std::sync::atomic::Ordering::Relaxed) {
            mic.iter_mut().for_each(|s| *s = 0);
        }
        out.clear();
        out.extend(mic.iter().flat_map(|s| s.to_le_bytes()));
        out.resize(n - n % 2, 0);
        file.write_all(&out)?;
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
}
