//! Per-control-connection AirPlay media setup and mirroring transport.

use std::io::Cursor;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plist::{Dictionary, Value};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;

use crate::crypto::{CryptoSession, MirrorDecryptor};
use crate::mirror::{AvcConfig, MirrorPacketDecoder, PayloadType};
use crate::rtsp::{Request, Response};
use crate::AirPlayError;

const MIRROR_STREAM_TYPE: u64 = 110;
const TIMING_PACKET_BYTES: usize = 32;
const NTP_UNIX_EPOCH_OFFSET: u64 = 2_208_988_800;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectionContext {
    pub local_addr: SocketAddr,
    pub peer_addr: SocketAddr,
}

#[derive(Default)]
pub(crate) struct MediaSession {
    encrypted_key: Option<Vec<u8>>,
    timing_task: Option<JoinHandle<()>>,
    mirror_task: Option<JoinHandle<()>>,
}

impl MediaSession {
    pub(crate) async fn handle_request(
        &mut self,
        request: &Request,
        context: Option<ConnectionContext>,
        crypto: Option<&mut dyn CryptoSession>,
    ) -> Result<Option<Response>, AirPlayError> {
        let target = request.target.split('?').next().unwrap_or(&request.target);
        if request.method.eq_ignore_ascii_case("SETUP") {
            let Some(context) = context else {
                return Ok(Some(Response::new(500, "Internal Server Error")));
            };
            let Some(crypto) = crypto else {
                return Ok(Some(Response::new(501, "Not Implemented").with_header(
                    "X-Cursio-Stage",
                    "airplay-crypto-provider-required",
                )));
            };
            return self
                .handle_setup(&request.body, context, crypto)
                .await
                .map(Some);
        }

        if request.method.eq_ignore_ascii_case("TEARDOWN") {
            self.stop();
            return Ok(Some(Response::new(200, "OK")));
        }

        let session_method = matches!(
            request.method.to_ascii_uppercase().as_str(),
            "RECORD" | "PAUSE" | "FLUSH" | "GET_PARAMETER" | "SET_PARAMETER"
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
                return Ok(Response::new(400, "Bad Request").with_header("X-Cursio-Error", message));
            }
        };
        let mut response = Dictionary::new();

        if let Some(keys) = setup.keys {
            self.encrypted_key = Some(keys.encrypted_key);
            let timing_port = self
                .start_timing(
                    context.local_addr.ip(),
                    context.peer_addr.ip(),
                    keys.remote_timing_port,
                )
                .await?;
            response.insert("timingPort".into(), u64::from(timing_port).into());
            response.insert(
                "eventPort".into(),
                u64::from(context.local_addr.port()).into(),
            );
        }

        if let Some(stream_connection_id) = setup.mirror_stream_id {
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
        let sink = hardware_video_sink()?;
        self.mirror_task = Some(tokio::spawn(async move {
            if let Err(error) = serve_mirror(listener, peer_ip, decryptor, sink).await {
                tracing::warn!(%error, "AirPlay mirroring stream ended");
            }
        }));
        Ok(local_port)
    }

    fn stop(&mut self) {
        if let Some(task) = self.timing_task.take() {
            task.abort();
        }
        if let Some(task) = self.mirror_task.take() {
            task.abort();
        }
        self.encrypted_key = None;
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SetupKeys {
    encrypted_key: Vec<u8>,
    remote_timing_port: u16,
}

struct SetupRequest {
    keys: Option<SetupKeys>,
    mirror_stream_id: Option<u64>,
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
                    remote_timing_port: timing_port,
                })
            }
            _ => return Err("AirPlay SETUP has invalid ekey/eiv fields"),
        };

        let mut mirror_stream_id = None;
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
                            .and_then(Value::as_unsigned_integer)
                            .ok_or("mirror stream has no streamConnectionID")?,
                    );
                }
            }
        }

        Ok(Self {
            keys,
            mirror_stream_id,
        })
    }
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

#[cfg(any(target_os = "linux", target_os = "windows"))]
struct HardwareSink(crate::video::HardwareVideoPipeline);

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl VideoSink for HardwareSink {
    fn push(&mut self, access_unit: &[u8]) -> Result<(), AirPlayError> {
        self.0.push_access_unit(access_unit, None)
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn hardware_video_sink() -> Result<Box<dyn VideoSink>, AirPlayError> {
    Ok(Box::new(HardwareSink(
        crate::video::HardwareVideoPipeline::new(crate::video::VideoCodec::H264)?,
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn hardware_video_sink() -> Result<Box<dyn VideoSink>, AirPlayError> {
    Err(AirPlayError::Video(
        "AirPlay hardware video output is supported on Linux and Windows".into(),
    ))
}

async fn serve_mirror(
    listener: TcpListener,
    peer_ip: IpAddr,
    decryptor: Box<dyn MirrorDecryptor>,
    sink: Box<dyn VideoSink>,
) -> Result<(), AirPlayError> {
    let mut stream = loop {
        let (stream, peer) = listener.accept().await?;
        if peer.ip() == peer_ip {
            break stream;
        }
        tracing::warn!(%peer, expected = %peer_ip, "rejected foreign AirPlay media connection");
    };
    stream.set_nodelay(true)?;
    let mut processor = MirrorProcessor::new(decryptor, sink);
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
    avc: Option<AvcConfig>,
}

impl MirrorProcessor {
    fn new(decryptor: Box<dyn MirrorDecryptor>, sink: Box<dyn VideoSink>) -> Self {
        Self {
            decoder: MirrorPacketDecoder::new(),
            decryptor,
            sink,
            avc: None,
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
            match packet.payload_type {
                PayloadType::CodecConfig => {
                    let config = AvcConfig::parse(&packet.payload)
                        .map_err(|error| AirPlayError::Video(error.to_string()))?;
                    self.sink.push(config.parameter_sets())?;
                    self.avc = Some(config);
                }
                PayloadType::Video => {
                    let config = self.avc.as_ref().ok_or_else(|| {
                        AirPlayError::Video("video arrived before AVC configuration".into())
                    })?;
                    let mut plaintext = Vec::with_capacity(packet.payload.len());
                    self.decryptor.decrypt(&packet.payload, &mut plaintext)?;
                    if plaintext.len() != packet.payload.len() {
                        return Err(AirPlayError::Crypto(
                            "mirror decryptor returned a different payload length".into(),
                        ));
                    }
                    let access_unit = config
                        .convert_access_unit(&plaintext)
                        .map_err(|error| AirPlayError::Video(error.to_string()))?;
                    self.sink.push(&access_unit)?;
                }
                PayloadType::Heartbeat | PayloadType::Unknown(_) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::crypto::CryptoEndpoint;

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
    async fn key_setup_returns_live_timing_and_event_ports() {
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
        let mut media = MediaSession::default();
        let response = media
            .handle_request(&request, Some(context), Some(&mut TestCrypto))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status, 200);
        let body = Value::from_reader(Cursor::new(response.body)).unwrap();
        let body = body.as_dictionary().unwrap();
        assert_eq!(
            body.get("eventPort").and_then(Value::as_unsigned_integer),
            Some(7000)
        );
        assert_ne!(
            body.get("timingPort")
                .and_then(Value::as_unsigned_integer)
                .unwrap(),
            0
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
        let mut processor = MirrorProcessor::new(Box::new(Passthrough), Box::new(sink));
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
}
