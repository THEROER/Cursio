//! The A2DP sink endpoint on BlueZ: this computer in a phone's list of
//! speakers.
//!
//! BlueZ asks a registered endpoint to pick a configuration
//! (`SelectConfiguration`), tells it which transport carries the stream
//! (`SetConfiguration`), and flips that transport to `pending` when the phone
//! starts playing. The sink then acquires the transport — an L2CAP socket of
//! RTP packets — and decodes it. The transport's `Volume` is the phone's own
//! volume slider (AVRCP absolute volume), applied here to the samples, so the
//! phone's buttons keep working.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::sbc::Decoder;
use crate::a2dp;
use crate::{BluetoothError, MediaAudioFactory};

const ENDPOINT_PATH: &str = "/dev/cursio/a2dp_sink";
const A2DP_SINK_UUID: &str = "0000110b-0000-1000-8000-00805f9b34fb";

struct Endpoint {
    bus: Arc<std::sync::Mutex<Option<Connection>>>,
    audio: Arc<dyn MediaAudioFactory>,
}

#[zbus::interface(name = "org.bluez.MediaEndpoint1")]
impl Endpoint {
    fn select_configuration(&self, capabilities: Vec<u8>) -> zbus::fdo::Result<Vec<u8>> {
        a2dp::select_sbc(&capabilities)
            .map(|c| c.to_vec())
            .ok_or_else(|| {
                tracing::warn!(
                    ?capabilities,
                    "the phone offers no SBC configuration this sink plays"
                );
                zbus::fdo::Error::InvalidArgs("no common SBC configuration".into())
            })
    }

    fn set_configuration(
        &self,
        transport: OwnedObjectPath,
        properties: HashMap<String, OwnedValue>,
    ) {
        let config: Vec<u8> = properties
            .get("Configuration")
            .and_then(|v| Vec::<u8>::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default();
        let device = properties
            .get("Device")
            .and_then(|v| OwnedObjectPath::try_from(v.try_clone().ok()?).ok());
        let Some(bus) = self.bus.lock().unwrap_or_else(|p| p.into_inner()).clone() else {
            return;
        };
        let audio = self.audio.clone();
        let phone = device
            .as_ref()
            .and_then(|d| super::address_from_path(d.as_str()))
            .unwrap_or_default();
        let name = device
            .as_ref()
            .and_then(|d| super::property(&bus, d.as_str(), "org.bluez.Device1", "Alias"))
            .unwrap_or_else(|| phone.clone());
        tracing::info!(%phone, %name, config = ?config, "phone chose this computer as its speaker");
        let spawned = std::thread::Builder::new()
            .name(format!("a2dp-{phone}"))
            .spawn(move || follow_transport(bus, transport, config, phone, name, audio));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot follow the phone's media transport");
        }
    }

    fn clear_configuration(&self, transport: OwnedObjectPath) {
        tracing::info!(transport = %transport.as_str(), "the phone stopped using this computer as its speaker");
    }

    fn release(&self) {
        tracing::info!("bluez released the speaker endpoint");
    }
}

/// This computer as the speaker of every paired phone that picks it.
pub struct MediaSinkService {
    _bus: Connection,
}

impl MediaSinkService {
    pub fn start(audio: Arc<dyn MediaAudioFactory>) -> Result<Self, BluetoothError> {
        let slot = Arc::new(std::sync::Mutex::new(None));
        let bus = zbus::blocking::connection::Builder::system()
            .and_then(|b| {
                b.serve_at(
                    ENDPOINT_PATH,
                    Endpoint {
                        bus: slot.clone(),
                        audio,
                    },
                )
            })
            .and_then(|b| b.build())
            .map_err(|e| BluetoothError::Bus(e.to_string()))?;
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(bus.clone());
        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("UUID", Value::from(A2DP_SINK_UUID));
        options.insert("Codec", Value::U8(a2dp::SBC));
        options.insert("Capabilities", Value::from(a2dp::SBC_CAPABILITIES.to_vec()));
        let path = ObjectPath::try_from(ENDPOINT_PATH).expect("constant path is valid");
        bus.call_method(
            Some("org.bluez"),
            "/org/bluez/hci0",
            Some("org.bluez.Media1"),
            "RegisterEndpoint",
            &(path, options),
        )
        .map_err(|e| BluetoothError::Register(e.to_string()))?;
        tracing::info!("registered as a Bluetooth speaker for paired phones");
        Ok(MediaSinkService { _bus: bus })
    }
}

fn transport_property(bus: &Connection, transport: &str, name: &str) -> Option<OwnedValue> {
    let reply = bus
        .call_method(
            Some("org.bluez"),
            transport,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.bluez.MediaTransport1", name),
        )
        .ok()?;
    reply.body().deserialize().ok()
}

/// Wait for the phone to start playing, play it, and wait again, until the
/// transport goes away.
fn follow_transport(
    bus: Connection,
    transport: OwnedObjectPath,
    config: Vec<u8>,
    phone: String,
    name: String,
    audio: Arc<dyn MediaAudioFactory>,
) {
    let path = transport.as_str();
    loop {
        let Some(state) =
            transport_property(&bus, path, "State").and_then(|v| String::try_from(v).ok())
        else {
            tracing::debug!(%phone, "media transport gone");
            return;
        };
        if state == "pending" || state == "active" {
            if let Err(e) = play(&bus, path, &config, &phone, &name, &audio) {
                tracing::info!(%phone, error = %e, "the phone's sound stopped");
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn play(
    bus: &Connection,
    path: &str,
    config: &[u8],
    phone: &str,
    name: &str,
    audio: &Arc<dyn MediaAudioFactory>,
) -> std::io::Result<()> {
    let reply = bus
        .call_method(
            Some("org.bluez"),
            path,
            Some("org.bluez.MediaTransport1"),
            "TryAcquire",
            &(),
        )
        .or_else(|_| {
            bus.call_method(
                Some("org.bluez"),
                path,
                Some("org.bluez.MediaTransport1"),
                "Acquire",
                &(),
            )
        })
        .map_err(|e| std::io::Error::other(format!("acquire: {e}")))?;
    let (fd, read_mtu, _write_mtu): (zbus::zvariant::OwnedFd, u16, u16) = reply
        .body()
        .deserialize()
        .map_err(|e| std::io::Error::other(format!("acquire reply: {e}")))?;
    let fd: OwnedFd = fd.into();
    let rate = a2dp::sbc_rate(config).unwrap_or(44_100);
    let channels: u8 = if config
        .first()
        .is_some_and(|b| b & 0x08 != 0 && b & 0x07 == 0)
    {
        1
    } else {
        2
    };
    let mut decoder = Decoder::new(config).map_err(std::io::Error::other)?;
    let mut sink = audio.open(phone, name).map_err(std::io::Error::other)?;
    tracing::info!(%phone, rate, channels, "playing the phone's sound");

    let probe = fd.try_clone()?;
    let mut file = std::fs::File::from(fd);
    let mut packet = vec![0u8; usize::from(read_mtu).max(1024)];
    let mut pcm = Vec::new();
    let mut volume = 1.0f32;
    let mut volume_checked = std::time::Instant::now();
    loop {
        if !super::sco::readable(&probe, 1000)? {
            // A second without a packet: the phone paused. Hand the
            // transport back so it can sleep, and wait for it to resume.
            let _ = bus.call_method(
                Some("org.bluez"),
                path,
                Some("org.bluez.MediaTransport1"),
                "Release",
                &(),
            );
            return Ok(());
        }
        let n = file.read(&mut packet)?;
        if n == 0 {
            return Ok(());
        }
        if volume_checked.elapsed() > Duration::from_millis(500) {
            volume_checked = std::time::Instant::now();
            if let Some(v) =
                transport_property(bus, path, "Volume").and_then(|v| u16::try_from(v).ok())
            {
                volume = f32::from(v.min(127)) / 127.0;
            }
        }
        let Some(frames) = a2dp::sbc_payload(&packet[..n]) else {
            continue;
        };
        pcm.clear();
        decoder.decode(frames, &mut pcm);
        if volume < 1.0 {
            pcm.iter_mut()
                .for_each(|s| *s = (f32::from(*s) * volume) as i16);
        }
        sink.play(&pcm, rate, channels);
    }
}
