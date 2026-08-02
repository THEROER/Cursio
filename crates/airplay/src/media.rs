//! Per-control-connection AirPlay media setup and mirroring transport.

use std::io::Cursor;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plist::{Dictionary, Value};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::audio::{self, AudioDecryptor, AudioStreamRequest};
use crate::crypto::{CryptoSession, MirrorDecryptor};
use crate::mirror::{AvcConfig, MirrorPacket, MirrorPacketDecoder, PayloadType};
use crate::rtsp::{Request, Response};
use crate::AirPlayError;

const MIRROR_STREAM_TYPE: u64 = 110;
const TIMING_PACKET_BYTES: usize = 32;
const NTP_UNIX_EPOCH_OFFSET: u64 = 2_208_988_800;
/// How long the advertised `dataPort` waits for the sender before giving the
/// listener and the video window back.
const MIRROR_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectionContext {
    pub local_addr: SocketAddr,
    pub peer_addr: SocketAddr,
}

pub(crate) struct MediaSession {
    encrypted_key: Option<Vec<u8>>,
    timing_task: Option<JoinHandle<()>>,
    mirror_task: Option<JoinHandle<()>>,
    audio_task: Option<JoinHandle<()>>,
    /// Kept from the key exchange: audio needs it for every packet.
    iv: Option<[u8; 16]>,
    screen_enabled: bool,
    /// Present when the mesh should be able to watch along.
    tap: Option<crate::MirrorTap>,
    /// The sender's volume, in AirPlay's decibel scale.
    ///
    /// Owned by the receiver rather than by this session: an iPhone drops and
    /// remakes its control connection constantly — locking the screen, opening
    /// a video — and a per-session volume would silently return to full every
    /// time, undoing whatever the user had set.
    volume: tokio::sync::watch::Sender<f32>,
    /// Fired when the user closes the window the screen was appearing in.
    ///
    /// Closing it means "stop sending me this", and the only way to tell an
    /// iPhone that is to end the control connection — there is no AirPlay
    /// message for "the viewer went away". So the mirror task raises this and
    /// the connection loop drops the socket, which takes the phone out of
    /// Screen Mirroring instead of leaving it broadcasting to nothing.
    screen_closed: Arc<Notify>,
    /// How large to open the window the screen appears in.
    screen_size: crate::video::ScreenSize,
}

impl MediaSession {
    pub(crate) fn new(
        screen_enabled: bool,
        tap: Option<crate::MirrorTap>,
        volume: tokio::sync::watch::Sender<f32>,
        screen_size: crate::video::ScreenSize,
    ) -> Self {
        Self {
            encrypted_key: None,
            timing_task: None,
            mirror_task: None,
            audio_task: None,
            iv: None,
            screen_enabled,
            tap,
            screen_closed: Arc::new(Notify::new()),
            volume,
            screen_size,
        }
    }

    /// Resolves when the screen window is closed and the session should end.
    pub(crate) async fn screen_closed(&self) {
        self.screen_closed.notified().await;
    }

    pub(crate) async fn handle_request(
        &mut self,
        request: &Request,
        context: Option<ConnectionContext>,
        crypto: &mut dyn CryptoSession,
    ) -> Result<Option<Response>, AirPlayError> {
        let target = request.target.split('?').next().unwrap_or(&request.target);
        if request.method.eq_ignore_ascii_case("SETUP") {
            let Some(context) = context else {
                return Ok(Some(Response::new(500, "Internal Server Error")));
            };
            return self
                .handle_setup(&request.body, context, crypto)
                .await
                .map(Some);
        }

        if request.method.eq_ignore_ascii_case("TEARDOWN") {
            self.teardown(&request.body);
            return Ok(Some(Response::new(200, "OK")));
        }

        if request.method.eq_ignore_ascii_case("GET_PARAMETER") && self.encrypted_key.is_some() {
            let asks_for_volume = request
                .body
                .split(|byte| *byte == b'\n')
                .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                .any(|line| line == b"volume");
            let response = if asks_for_volume {
                // Reporting what the sender last set, not a constant: an iPhone
                // reads this back to draw its own slider, and a receiver that
                // always answers 0.0 makes the slider jump back every time.
                Response::new(200, "OK").with_body(
                    "text/parameters",
                    format!("volume: {:.6}\r\n", *self.volume.borrow()).into_bytes(),
                )
            } else {
                Response::new(200, "OK")
            };
            return Ok(Some(response));
        }

        if request.method.eq_ignore_ascii_case("SET_PARAMETER") && self.encrypted_key.is_some() {
            if let Some(volume) = parse_volume(&request.body) {
                tracing::debug!(volume, "AirPlay volume changed");
                // Replaced rather than sent: `send` fails when nothing is
                // listening yet, and a sender sets the volume before the audio
                // stream exists — losing it there is what made a quiet phone
                // come back loud.
                self.volume.send_replace(volume);
            }
            return Ok(Some(Response::new(200, "OK")));
        }

        let session_method = matches!(
            request.method.to_ascii_uppercase().as_str(),
            "RECORD" | "PAUSE" | "FLUSH" | "SET_PARAMETER"
        );
        let session_endpoint = matches!(target, "/feedback" | "/command" | "/audioMode");
        if (session_method || session_endpoint) && self.encrypted_key.is_some() {
            return Ok(Some(Response::new(200, "OK")));
        }

        Ok(None)
    }

    async fn handle_setup(
        &mut self,
        body: &[u8],
        context: ConnectionContext,
        crypto: &mut dyn CryptoSession,
    ) -> Result<Response, AirPlayError> {
        let setup = match SetupRequest::parse(body) {
            Ok(setup) => setup,
            Err(message) => {
                tracing::warn!(
                    error = message,
                    body_bytes = body.len(),
                    "rejected AirPlay SETUP"
                );
                return Ok(Response::new(400, "Bad Request").with_header("X-Cursio-Error", message));
            }
        };
        tracing::debug!(
            has_keys = setup.keys.is_some(),
            mirror_stream_id = ?setup.mirror_stream_id,
            "parsed AirPlay SETUP"
        );
        let mut response = Dictionary::new();

        if let Some(keys) = setup.keys {
            self.encrypted_key = Some(keys.encrypted_key);
            self.iv = Some(keys.iv);
            let timing_port = self
                .start_timing(
                    context.local_addr.ip(),
                    context.peer_addr.ip(),
                    keys.remote_timing_port,
                )
                .await?;
            response.insert("timingPort".into(), u64::from(timing_port).into());
            // Screen mirroring does not use AirPlay's reverse event socket.
            // Advertising the control port here makes iOS open a second RTSP
            // connection and abandon the two-stage SETUP flow.
            response.insert("eventPort".into(), 0_u64.into());
            tracing::info!(
                local_timing_port = timing_port,
                remote_timing_port = keys.remote_timing_port,
                event_port = 0,
                "AirPlay key session configured"
            );
        }

        if let Some(stream_connection_id) = setup.mirror_stream_id {
            if !self.screen_enabled {
                tracing::info!(
                    stream_connection_id,
                    "AirPlay screen mirroring rejected because it is disabled"
                );
                return Ok(Response::new(501, "Not Implemented")
                    .with_header("X-Cursio-Stage", "airplay-screen-disabled"));
            }
            let Some(encrypted_key) = self.encrypted_key.as_deref() else {
                return Ok(Response::new(455, "Method Not Valid in This State"));
            };
            let decryptor = crypto.start_mirror_decryptor(encrypted_key, stream_connection_id)?;
            let data_port = self
                .start_mirror(context.local_addr.ip(), context.peer_addr.ip(), decryptor)
                .await?;
            let mut stream = Dictionary::new();
            stream.insert("type".into(), MIRROR_STREAM_TYPE.into());
            stream.insert("dataPort".into(), u64::from(data_port).into());
            response.insert(
                "streams".into(),
                Value::Array(vec![Value::Dictionary(stream)]),
            );
            tracing::info!(
                stream_connection_id,
                data_port,
                "AirPlay mirror stream configured"
            );
        }

        if let Some(request) = setup.audio {
            // Unwrapped here because the FairPlay session belongs to this
            // control connection; the receiving task only needs the result.
            let decryptor = match (self.encrypted_key.as_deref(), self.iv) {
                (Some(encrypted_key), Some(iv)) => match crypto.unwrap_media_key(encrypted_key) {
                    Ok(key) => Some(AudioDecryptor::new(key, iv)),
                    Err(error) => {
                        tracing::warn!(%error, "AirPlay audio will arrive encrypted");
                        None
                    }
                },
                _ => None,
            };
            let ports = self
                .start_audio(context.local_addr.ip(), request, decryptor)
                .await?;
            // A `SETUP` may carry video and audio together, so this appends to
            // whatever the mirror stream already put there.
            match response.get_mut("streams").and_then(Value::as_array_mut) {
                Some(streams) => streams.push(ports),
                None => {
                    response.insert("streams".into(), Value::Array(vec![ports]));
                }
            }
        }

        if response.is_empty() {
            return Ok(Response::new(461, "Unsupported Transport"));
        }
        let mut encoded = Vec::new();
        Value::Dictionary(response).to_writer_binary(&mut encoded)?;
        Ok(Response::new(200, "OK").with_body("application/x-apple-binary-plist", encoded))
    }

    async fn start_timing(
        &mut self,
        local_ip: IpAddr,
        peer_ip: IpAddr,
        peer_port: u16,
    ) -> Result<u16, std::io::Error> {
        if let Some(task) = self.timing_task.take() {
            task.abort();
        }
        let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
        let local_port = socket.local_addr()?.port();
        socket.connect(SocketAddr::new(peer_ip, peer_port)).await?;
        self.timing_task = Some(tokio::spawn(run_timing(socket)));
        Ok(local_port)
    }

    async fn start_mirror(
        &mut self,
        local_ip: IpAddr,
        peer_ip: IpAddr,
        decryptor: Box<dyn MirrorDecryptor>,
    ) -> Result<u16, AirPlayError> {
        if let Some(task) = self.mirror_task.take() {
            task.abort();
        }
        let listener = TcpListener::bind(SocketAddr::new(local_ip, 0)).await?;
        let local_port = listener.local_addr()?.port();
        let sink = hardware_video_sink(self.screen_size)?;
        let tap = self.tap.clone();
        let closed = self.screen_closed.clone();
        self.mirror_task = Some(tokio::spawn(async move {
            match serve_mirror(listener, peer_ip, decryptor, sink, tap).await {
                Ok(()) => {}
                Err(AirPlayError::ScreenClosed) => {
                    tracing::info!("AirPlay screen window closed; ending the session");
                    closed.notify_waiters();
                }
                Err(error) => tracing::warn!(%error, "AirPlay mirroring stream ended"),
            }
        }));
        Ok(local_port)
    }

    /// True once the sender has negotiated its media key on this connection.
    pub(crate) fn is_active(&self) -> bool {
        self.encrypted_key.is_some()
    }

    /// A `TEARDOWN` carrying a `streams` array ends **those streams only**; iOS
    /// reuses the same control session for the next `SETUP`. Dropping the
    /// negotiated key there would answer that `SETUP` with 455 and leave
    /// mirroring unable to restart without a full reconnect.
    ///
    /// Which streams matters as much as how many. Screen and sound are torn
    /// down independently — locking the phone ends one and leaves the other —
    /// and closing both when asked for one loses the picture for good, because
    /// the sender never asked for it back.
    fn teardown(&mut self, body: &[u8]) {
        match teardown_stream_types(body) {
            Some(types) => {
                if types.contains(&MIRROR_STREAM_TYPE) {
                    self.stop_mirror();
                }
                if types.contains(&audio::AUDIO_STREAM_TYPE) {
                    self.stop_audio();
                }
            }
            None => self.stop(),
        }
    }

    fn stop_mirror(&mut self) {
        if let Some(task) = self.mirror_task.take() {
            task.abort();
        }
    }

    fn stop_audio(&mut self) {
        if let Some(task) = self.audio_task.take() {
            task.abort();
        }
    }

    /// Accept the audio that accompanies mirroring.
    ///
    /// Answering at all is the point: an unanswered audio stream makes iOS
    /// mirror silently, and nothing about the sound can be learned from a
    /// sender that never sends it.
    async fn start_audio(
        &mut self,
        local_ip: IpAddr,
        request: AudioStreamRequest,
        decryptor: Option<AudioDecryptor>,
    ) -> Result<Value, AirPlayError> {
        if let Some(task) = self.audio_task.take() {
            task.abort();
        }
        let (data, control) = audio::bind_ports(local_ip).await?;
        let data_port = data.local_addr()?.port();
        let control_port = control.local_addr()?.port();
        tracing::info!(
            data_port,
            control_port,
            encrypted = decryptor.is_some(),
            setup = %request.describe(),
            "AirPlay audio stream configured"
        );
        self.audio_task = Some(tokio::spawn(audio::observe(
            data,
            request,
            decryptor,
            self.volume.subscribe(),
        )));
        Ok(audio::setup_response(data_port, control_port))
    }

    fn stop(&mut self) {
        if let Some(task) = self.timing_task.take() {
            task.abort();
        }
        self.stop_mirror();
        self.stop_audio();
        self.encrypted_key = None;
        self.iv = None;
    }
}

/// Read `volume: <decibels>` out of a `SET_PARAMETER` body.
///
/// AirPlay states volume in decibels: 0 is full, negative is quieter, and
/// -144 means muted.
fn parse_volume(body: &[u8]) -> Option<f32> {
    std::str::from_utf8(body)
        .ok()?
        .lines()
        .find_map(|line| line.trim().strip_prefix("volume:"))
        .and_then(|value| value.trim().parse::<f32>().ok())
}

/// The stream types a `TEARDOWN` names, or `None` when it names none and
/// therefore ends the session.
fn teardown_stream_types(body: &[u8]) -> Option<Vec<u64>> {
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

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SetupKeys {
    encrypted_key: Vec<u8>,
    /// The audio stream's CBC initialisation vector, reused for every packet.
    iv: [u8; 16],
    remote_timing_port: u16,
}

struct SetupRequest {
    keys: Option<SetupKeys>,
    mirror_stream_id: Option<u64>,
    audio: Option<AudioStreamRequest>,
}

impl SetupRequest {
    fn parse(body: &[u8]) -> Result<Self, &'static str> {
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

async fn run_timing(socket: UdpSocket) {
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut response = [0_u8; 128];
    loop {
        interval.tick().await;
        let request = timing_request();
        if socket.send(&request).await.is_err() {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_millis(500), socket.recv(&mut response)).await;
    }
}

fn timing_request() -> [u8; TIMING_PACKET_BYTES] {
    let mut request = [0_u8; TIMING_PACKET_BYTES];
    request[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs().saturating_add(NTP_UNIX_EPOCH_OFFSET);
    let fraction = (u128::from(elapsed.subsec_nanos()) << 32) / 1_000_000_000_u128;
    let timestamp = (seconds << 32) | fraction as u64;
    request[24..].copy_from_slice(&timestamp.to_be_bytes());
    request
}

trait VideoSink: Send {
    fn push(&mut self, access_unit: &[u8]) -> Result<(), AirPlayError>;
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
struct HardwareSink(crate::video::HardwareVideoPipeline);

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl VideoSink for HardwareSink {
    fn push(&mut self, access_unit: &[u8]) -> Result<(), AirPlayError> {
        self.0.push_access_unit(access_unit, None)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn hardware_video_sink(size: crate::video::ScreenSize) -> Result<Box<dyn VideoSink>, AirPlayError> {
    Ok(Box::new(HardwareSink(
        crate::video::HardwareVideoPipeline::new(crate::video::VideoCodec::H264, size)?,
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn hardware_video_sink(
    _size: crate::video::ScreenSize,
) -> Result<Box<dyn VideoSink>, AirPlayError> {
    Err(AirPlayError::Video(
        "AirPlay hardware video output is supported on Linux and Windows".into(),
    ))
}

async fn serve_mirror(
    listener: TcpListener,
    peer_ip: IpAddr,
    decryptor: Box<dyn MirrorDecryptor>,
    sink: Box<dyn VideoSink>,
    tap: Option<crate::MirrorTap>,
) -> Result<(), AirPlayError> {
    let mut stream = loop {
        let accepted = tokio::time::timeout(MIRROR_CONNECT_TIMEOUT, listener.accept())
            .await
            .map_err(|_| {
                AirPlayError::Video("the AirPlay sender never opened the mirror stream".into())
            })??;
        let (stream, peer) = accepted;
        if peer.ip() == peer_ip {
            break stream;
        }
        tracing::warn!(%peer, expected = %peer_ip, "rejected foreign AirPlay media connection");
    };
    stream.set_nodelay(true)?;
    let mut processor = MirrorProcessor::new(decryptor, sink, tap);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        processor.push(&buffer[..count])?;
    }
}

struct MirrorProcessor {
    decoder: MirrorPacketDecoder,
    decryptor: Box<dyn MirrorDecryptor>,
    sink: Box<dyn VideoSink>,
    /// Where the mesh listens in, when the receiver was given a tap.
    tap: Option<crate::MirrorTap>,
    avc: Option<AvcConfig>,
    plaintext: Vec<u8>,
    access_unit: Vec<u8>,
    skipped: u64,
}

impl MirrorProcessor {
    fn new(
        decryptor: Box<dyn MirrorDecryptor>,
        sink: Box<dyn VideoSink>,
        tap: Option<crate::MirrorTap>,
    ) -> Self {
        Self {
            decoder: MirrorPacketDecoder::new(),
            decryptor,
            sink,
            tap,
            avc: None,
            plaintext: Vec::new(),
            access_unit: Vec::new(),
            skipped: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), AirPlayError> {
        self.decoder
            .push(bytes)
            .map_err(|error| AirPlayError::Video(error.to_string()))?;
        while let Some(packet) = self
            .decoder
            .next_packet()
            .map_err(|error| AirPlayError::Video(error.to_string()))?
        {
            // Framing errors above desynchronize the stream and end it. A
            // single undecodable payload does not: iOS recovers on its own at
            // the next IDR, so skipping keeps the session alive instead of
            // freezing the picture until the user reconnects.
            if let Err(error) = self.process(&packet) {
                // Not a fault to skip past: there is nothing left to show it
                // on, and the caller ends the session so the sender stops.
                if matches!(error, AirPlayError::ScreenClosed) {
                    return Err(error);
                }
                self.skipped += 1;
                // One line per skipped frame is one line per 16 ms once
                // something goes persistently wrong, which buries whatever
                // else the log was about to say.
                if self.skipped.is_power_of_two() {
                    tracing::warn!(
                        %error,
                        payload_type = ?packet.payload_type,
                        skipped = self.skipped,
                        "skipped an AirPlay mirror packet"
                    );
                }
            }
        }
        Ok(())
    }

    fn process(&mut self, packet: &MirrorPacket) -> Result<(), AirPlayError> {
        match packet.payload_type {
            PayloadType::CodecConfig => {
                let config = AvcConfig::parse(&packet.payload)
                    .map_err(|error| AirPlayError::Video(error.to_string()))?;
                self.sink.push(config.parameter_sets())?;
                if let Some(tap) = self.tap.as_ref() {
                    tap.publish_parameter_sets(config.parameter_sets());
                }
                self.avc = Some(config);
            }
            PayloadType::Video => {
                // The AES-CTR keystream spans the whole connection, so every
                // video payload has to be decrypted in wire order even when it
                // cannot be rendered. Skipping one before decryption would
                // desynchronize the cipher for the rest of the session.
                self.plaintext.clear();
                self.decryptor
                    .decrypt(&packet.payload, &mut self.plaintext)?;
                if self.plaintext.len() != packet.payload.len() {
                    return Err(AirPlayError::Crypto(
                        "mirror decryptor returned a different payload length".into(),
                    ));
                }
                let config = self.avc.as_ref().ok_or_else(|| {
                    AirPlayError::Video("video arrived before AVC configuration".into())
                })?;
                self.access_unit.clear();
                config
                    .convert_access_unit_into(&self.plaintext, &mut self.access_unit)
                    .map_err(|error| AirPlayError::Video(error.to_string()))?;
                self.sink.push(&self.access_unit)?;
                // Only pay for the copy when a peer is actually watching.
                if let Some(tap) = self.tap.as_ref().filter(|tap| tap.has_readers()) {
                    tap.publish_access_unit(&self.access_unit);
                }
            }
            PayloadType::Heartbeat | PayloadType::Unknown(_) => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::crypto::CryptoEndpoint;

    fn test_volume() -> tokio::sync::watch::Sender<f32> {
        tokio::sync::watch::Sender::new(0.0)
    }

    fn test_screen_size() -> crate::video::ScreenSize {
        crate::video::ScreenSize::new(1920, 1080)
    }

    fn binary_plist(value: Value) -> Vec<u8> {
        let mut bytes = Vec::new();
        value.to_writer_binary(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn parses_key_and_mirror_setup_requests() {
        let mut root = Dictionary::new();
        root.insert("ekey".into(), Value::Data(vec![7; 72]));
        root.insert("eiv".into(), Value::Data(vec![8; 16]));
        root.insert("timingPort".into(), 7010_u64.into());
        let keys = SetupRequest::parse(&binary_plist(Value::Dictionary(root))).unwrap();
        assert_eq!(keys.keys.unwrap().remote_timing_port, 7010);

        let mut stream = Dictionary::new();
        stream.insert("type".into(), MIRROR_STREAM_TYPE.into());
        stream.insert("streamConnectionID".into(), 42_u64.into());
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let stream = SetupRequest::parse(&binary_plist(Value::Dictionary(root))).unwrap();
        assert_eq!(stream.mirror_stream_id, Some(42));

        let mut stream = Dictionary::new();
        stream.insert("type".into(), MIRROR_STREAM_TYPE.into());
        stream.insert("streamConnectionID".into(), (-1_i64).into());
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let stream = SetupRequest::parse(&binary_plist(Value::Dictionary(root))).unwrap();
        assert_eq!(stream.mirror_stream_id, Some(u64::MAX));
    }

    #[test]
    fn rejects_partial_key_setup() {
        let mut root = Dictionary::new();
        root.insert("ekey".into(), Value::Data(vec![7; 72]));
        root.insert("timingPort".into(), 7010_u64.into());
        assert_eq!(
            SetupRequest::parse(&binary_plist(Value::Dictionary(root)))
                .err()
                .unwrap(),
            "AirPlay SETUP has invalid ekey/eiv fields"
        );
    }

    struct TestCrypto;

    impl CryptoSession for TestCrypto {
        fn handle(&mut self, _: CryptoEndpoint, _: &[u8]) -> Result<Vec<u8>, AirPlayError> {
            Ok(Vec::new())
        }

        fn unwrap_media_key(&mut self, _: &[u8]) -> Result<[u8; 16], AirPlayError> {
            Ok([0; 16])
        }
    }

    #[tokio::test]
    async fn key_setup_returns_timing_port_and_disables_event_socket() {
        let timing_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut root = Dictionary::new();
        root.insert("ekey".into(), Value::Data(vec![7; 72]));
        root.insert("eiv".into(), Value::Data(vec![8; 16]));
        root.insert(
            "timingPort".into(),
            u64::from(timing_peer.local_addr().unwrap().port()).into(),
        );
        let request = Request {
            method: "SETUP".into(),
            target: "rtsp://127.0.0.1/1".into(),
            version: crate::rtsp::Version::Rtsp10,
            headers: Vec::new(),
            body: binary_plist(Value::Dictionary(root)),
        };
        let context = ConnectionContext {
            local_addr: "127.0.0.1:7000".parse().unwrap(),
            peer_addr: "127.0.0.1:50000".parse().unwrap(),
        };
        let mut media = MediaSession::new(false, None, test_volume(), test_screen_size());
        let response = media
            .handle_request(&request, Some(context), &mut TestCrypto)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status, 200);
        let body = Value::from_reader(Cursor::new(response.body)).unwrap();
        let body = body.as_dictionary().unwrap();
        assert_eq!(
            body.get("eventPort").and_then(Value::as_unsigned_integer),
            Some(0)
        );
        assert_ne!(
            body.get("timingPort")
                .and_then(Value::as_unsigned_integer)
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn mirror_setup_is_rejected_when_screen_is_disabled() {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), MIRROR_STREAM_TYPE.into());
        stream.insert("streamConnectionID".into(), 42_u64.into());
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let request = Request {
            method: "SETUP".into(),
            target: "rtsp://127.0.0.1/1".into(),
            version: crate::rtsp::Version::Rtsp10,
            headers: Vec::new(),
            body: binary_plist(Value::Dictionary(root)),
        };
        let context = ConnectionContext {
            local_addr: "127.0.0.1:7000".parse().unwrap(),
            peer_addr: "127.0.0.1:50000".parse().unwrap(),
        };
        let mut media = MediaSession::new(false, None, test_volume(), test_screen_size());
        let response = media
            .handle_request(&request, Some(context), &mut TestCrypto)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(response.status, 501);
        assert!(response.headers.iter().any(|(name, value)| {
            name == "X-Cursio-Stage" && value == "airplay-screen-disabled"
        }));
    }

    #[tokio::test]
    async fn stream_teardown_keeps_the_negotiated_session() {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), MIRROR_STREAM_TYPE.into());
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let request = Request {
            method: "TEARDOWN".into(),
            target: "rtsp://127.0.0.1/1".into(),
            version: crate::rtsp::Version::Rtsp10,
            headers: Vec::new(),
            body: binary_plist(Value::Dictionary(root)),
        };
        let mut media = MediaSession::new(true, None, test_volume(), test_screen_size());
        media.encrypted_key = Some(vec![7; 72]);

        let response = media
            .handle_request(&request, None, &mut TestCrypto)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status, 200);
        assert!(media.is_active());
    }

    /// Locking the phone tears down one stream and leaves the other running.
    /// Closing both would lose the picture permanently, because the sender only
    /// re-creates what it asked to close.
    #[tokio::test]
    async fn tearing_down_audio_leaves_the_mirror_alone() {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), crate::audio::AUDIO_STREAM_TYPE.into());
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let body = binary_plist(Value::Dictionary(root));

        let types = teardown_stream_types(&body).unwrap();
        assert!(types.contains(&crate::audio::AUDIO_STREAM_TYPE));
        assert!(!types.contains(&MIRROR_STREAM_TYPE));
    }

    #[test]
    fn a_teardown_naming_the_mirror_names_only_it() {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), MIRROR_STREAM_TYPE.into());
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let types = teardown_stream_types(&binary_plist(Value::Dictionary(root))).unwrap();
        assert_eq!(types, vec![MIRROR_STREAM_TYPE]);
    }

    #[test]
    fn a_teardown_naming_nothing_ends_the_session() {
        assert_eq!(teardown_stream_types(&[]), None);
        let empty = binary_plist(Value::Dictionary(Dictionary::new()));
        assert_eq!(teardown_stream_types(&empty), None);
    }

    #[tokio::test]
    async fn session_teardown_without_streams_clears_the_key() {
        let request = Request {
            method: "TEARDOWN".into(),
            target: "rtsp://127.0.0.1/1".into(),
            version: crate::rtsp::Version::Rtsp10,
            headers: Vec::new(),
            body: Vec::new(),
        };
        let mut media = MediaSession::new(true, None, test_volume(), test_screen_size());
        media.encrypted_key = Some(vec![7; 72]);

        let response = media
            .handle_request(&request, None, &mut TestCrypto)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status, 200);
        assert!(!media.is_active());
    }

    /// What the sender set has to survive the reconnection it makes moments
    /// later, or the user's choice is undone by a screen lock.
    #[tokio::test]
    async fn a_volume_the_sender_set_is_reported_back() {
        let volume = test_volume();
        let mut media = MediaSession::new(false, None, volume.clone(), test_screen_size());
        media.encrypted_key = Some(vec![7; 72]);

        let set = Request {
            method: "SET_PARAMETER".into(),
            target: "rtsp://127.0.0.1/1".into(),
            version: crate::rtsp::Version::Rtsp10,
            headers: vec![("Content-Type".into(), "text/parameters".into())],
            body: b"volume: -23.500000\r\n".to_vec(),
        };
        assert_eq!(
            media
                .handle_request(&set, None, &mut TestCrypto)
                .await
                .unwrap()
                .unwrap()
                .status,
            200
        );
        assert_eq!(*volume.borrow(), -23.5);

        // A new connection reuses the receiver's volume rather than resetting.
        let mut reconnected = MediaSession::new(false, None, volume, test_screen_size());
        reconnected.encrypted_key = Some(vec![7; 72]);
        let get = Request {
            method: "GET_PARAMETER".into(),
            target: "rtsp://127.0.0.1/1".into(),
            version: crate::rtsp::Version::Rtsp10,
            headers: Vec::new(),
            body: b"volume\r\n".to_vec(),
        };
        let response = reconnected
            .handle_request(&get, None, &mut TestCrypto)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.body, b"volume: -23.500000\r\n");
    }

    #[test]
    fn a_volume_line_is_read_from_a_set_parameter_body() {
        assert_eq!(parse_volume(b"volume: -30.000000\r\n"), Some(-30.0));
        assert_eq!(parse_volume(b"volume: 0.000000\r\n"), Some(0.0));
        // Muting is its own value rather than a very small one.
        assert_eq!(parse_volume(b"volume: -144.000000\r\n"), Some(-144.0));
        assert_eq!(parse_volume(b"progress: 1/2/3\r\n"), None);
        assert_eq!(parse_volume(&[0xff, 0xfe]), None);
    }

    #[tokio::test]
    async fn get_parameter_returns_requested_volume() {
        let request = Request {
            method: "GET_PARAMETER".into(),
            target: "rtsp://127.0.0.1/1".into(),
            version: crate::rtsp::Version::Rtsp10,
            headers: vec![("Content-Type".into(), "text/parameters".into())],
            body: b"volume\r\n".to_vec(),
        };
        let mut media = MediaSession::new(false, None, test_volume(), test_screen_size());
        media.encrypted_key = Some(vec![7; 72]);
        let response = media
            .handle_request(&request, None, &mut TestCrypto)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"volume: 0.000000\r\n");
        assert_eq!(
            response
                .headers
                .iter()
                .find(|(name, _)| name == "Content-Type")
                .map(|(_, value)| value.as_str()),
            Some("text/parameters")
        );
    }

    struct Passthrough;

    impl MirrorDecryptor for Passthrough {
        fn decrypt(
            &mut self,
            ciphertext: &[u8],
            plaintext: &mut Vec<u8>,
        ) -> Result<(), AirPlayError> {
            plaintext.extend_from_slice(ciphertext);
            Ok(())
        }
    }

    struct RecordingSink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl VideoSink for RecordingSink {
        fn push(&mut self, access_unit: &[u8]) -> Result<(), AirPlayError> {
            self.0.lock().unwrap().push(access_unit.to_vec());
            Ok(())
        }
    }

    fn mirror_packet(payload_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0; crate::mirror::MIRROR_HEADER_BYTES];
        packet[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        packet[4..6].copy_from_slice(&payload_type.to_le_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn decrypts_and_converts_video_for_the_sink() {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingSink(frames.clone());
        let mut processor = MirrorProcessor::new(Box::new(Passthrough), Box::new(sink), None);
        let avcc = [
            1, 100, 0, 31, 0xff, 0xe1, 0, 3, 0x67, 0x64, 0x00, 1, 0, 2, 0x68, 0xee,
        ];
        processor.push(&mirror_packet(1, &avcc)).unwrap();
        processor
            .push(&mirror_packet(0, &[0, 0, 0, 2, 0x65, 0xaa]))
            .unwrap();

        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1], [0, 0, 0, 1, 0x65, 0xaa]);
    }

    /// A packet that cannot be rendered must not end the stream, and it must
    /// still pass through the decryptor: the AES-CTR keystream is continuous,
    /// so skipping one before decryption corrupts every later frame.
    #[test]
    fn undecodable_packets_are_skipped_without_desyncing_the_cipher() {
        struct Counting(Arc<Mutex<usize>>);

        impl MirrorDecryptor for Counting {
            fn decrypt(
                &mut self,
                ciphertext: &[u8],
                plaintext: &mut Vec<u8>,
            ) -> Result<(), AirPlayError> {
                *self.0.lock().unwrap() += 1;
                plaintext.extend_from_slice(ciphertext);
                Ok(())
            }
        }

        let decrypted = Arc::new(Mutex::new(0));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let mut processor = MirrorProcessor::new(
            Box::new(Counting(decrypted.clone())),
            Box::new(RecordingSink(frames.clone())),
            None,
        );

        // Video before any codec configuration, then a malformed access unit,
        // then a frame that is fine.
        processor
            .push(&mirror_packet(0, &[0, 0, 0, 2, 0x65, 0xaa]))
            .unwrap();
        let avcc = [
            1, 100, 0, 31, 0xff, 0xe1, 0, 3, 0x67, 0x64, 0x00, 1, 0, 2, 0x68, 0xee,
        ];
        processor.push(&mirror_packet(1, &avcc)).unwrap();
        processor
            .push(&mirror_packet(0, &[0, 0, 0, 9, 0x65]))
            .unwrap();
        processor
            .push(&mirror_packet(0, &[0, 0, 0, 2, 0x65, 0xbb]))
            .unwrap();

        assert_eq!(*decrypted.lock().unwrap(), 3);
        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1], [0, 0, 0, 1, 0x65, 0xbb]);
    }
}
