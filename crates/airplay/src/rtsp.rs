//! Bounded HTTP/RTSP framing and the initial AirPlay control routes.

use std::fmt;

use plist::{Dictionary, Value};

use crate::crypto::{CryptoEndpoint, CryptoSession};
use crate::discovery::AIRPLAY_FEATURES;
use crate::{AirPlayError, ReceiverConfig};

pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_BUFFERED_BYTES: usize = MAX_HEADER_BYTES + MAX_BODY_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Http11,
    Rtsp10,
}

impl Version {
    fn parse(value: &str) -> Result<Self, ProtocolError> {
        match value {
            "HTTP/1.1" => Ok(Self::Http11),
            "RTSP/1.0" => Ok(Self::Rtsp10),
            other => Err(ProtocolError::UnsupportedVersion(other.to_string())),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http11 => f.write_str("HTTP/1.1"),
            Self::Rtsp10 => f.write_str("RTSP/1.0"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub version: Version,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Incremental decoder for a persistent control connection.
#[derive(Debug, Default)]
pub struct RequestDecoder {
    buffered: Vec<u8>,
}

impl RequestDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        if self.buffered.len().saturating_add(bytes.len()) > MAX_BUFFERED_BYTES {
            return Err(ProtocolError::BufferTooLarge);
        }
        self.buffered.extend_from_slice(bytes);
        Ok(())
    }

    pub fn next_request(&mut self) -> Result<Option<Request>, ProtocolError> {
        let Some(header_offset) = find_header_end(&self.buffered) else {
            if self.buffered.len() > MAX_HEADER_BYTES {
                return Err(ProtocolError::HeaderTooLarge);
            }
            return Ok(None);
        };
        let header_len = header_offset + 4;
        if header_len > MAX_HEADER_BYTES {
            return Err(ProtocolError::HeaderTooLarge);
        }

        let header = std::str::from_utf8(&self.buffered[..header_offset])
            .map_err(|_| ProtocolError::Malformed("headers are not UTF-8"))?;
        let mut lines = header.split("\r\n");
        let request_line = lines
            .next()
            .ok_or(ProtocolError::Malformed("missing request line"))?;
        let mut parts = request_line.split_ascii_whitespace();
        let method = parts
            .next()
            .ok_or(ProtocolError::Malformed("missing method"))?
            .to_string();
        let target = parts
            .next()
            .ok_or(ProtocolError::Malformed("missing target"))?
            .to_string();
        let version = parts
            .next()
            .ok_or(ProtocolError::Malformed("missing version"))?;
        if parts.next().is_some() {
            return Err(ProtocolError::Malformed("invalid request line"));
        }
        if !method.bytes().all(is_token_byte) || target.is_empty() {
            return Err(ProtocolError::Malformed("invalid method or target"));
        }
        let version = Version::parse(version)?;

        let mut headers = Vec::new();
        let mut content_length = None;
        for line in lines {
            let (name, value) = line
                .split_once(':')
                .ok_or(ProtocolError::Malformed("invalid header line"))?;
            let name = name.trim();
            let value = value.trim();
            if name.is_empty() || !name.bytes().all(is_token_byte) {
                return Err(ProtocolError::Malformed("invalid header name"));
            }
            if value
                .bytes()
                .any(|byte| byte != b'\t' && (byte < b' ' || byte == 0x7f))
            {
                return Err(ProtocolError::Malformed("invalid header value"));
            }
            if name.eq_ignore_ascii_case("content-length") {
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| ProtocolError::InvalidContentLength)?;
                if content_length.replace(parsed).is_some() {
                    return Err(ProtocolError::DuplicateContentLength);
                }
            }
            headers.push((name.to_string(), value.to_string()));
        }

        let body_len = content_length.unwrap_or(0);
        if body_len > MAX_BODY_BYTES {
            return Err(ProtocolError::BodyTooLarge(body_len));
        }
        let request_len = header_len
            .checked_add(body_len)
            .ok_or(ProtocolError::BodyTooLarge(body_len))?;
        if self.buffered.len() < request_len {
            return Ok(None);
        }

        let body = self.buffered[header_len..request_len].to_vec();
        self.buffered.drain(..request_len);
        Ok(Some(Request {
            method,
            target,
            version,
            headers,
            body,
        }))
    }

    pub fn is_empty(&self) -> bool {
        self.buffered.is_empty()
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("control header exceeds {MAX_HEADER_BYTES} bytes")]
    HeaderTooLarge,
    #[error("control body is {0} bytes, limit is {MAX_BODY_BYTES}")]
    BodyTooLarge(usize),
    #[error("control receive buffer limit exceeded")]
    BufferTooLarge,
    #[error("invalid Content-Length")]
    InvalidContentLength,
    #[error("duplicate Content-Length")]
    DuplicateContentLength,
    #[error("unsupported control protocol version {0}")]
    UnsupportedVersion(String),
    #[error("malformed control request: {0}")]
    Malformed(&'static str),
    #[error("connection closed in the middle of a control request")]
    UnexpectedEof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub(crate) fn new(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub(crate) fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub(crate) fn with_body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.body = body;
        self
    }

    pub fn encode(&self, version: Version, cseq: Option<&str>) -> Vec<u8> {
        let mut encoded = format!("{version} {} {}\r\n", self.status, self.reason);
        encoded.push_str("Server: AirTunes/220.68\r\n");
        if let Some(cseq) = cseq {
            encoded.push_str("CSeq: ");
            encoded.push_str(cseq);
            encoded.push_str("\r\n");
        }
        for (name, value) in &self.headers {
            encoded.push_str(name);
            encoded.push_str(": ");
            encoded.push_str(value);
            encoded.push_str("\r\n");
        }
        encoded.push_str("Connection: keep-alive\r\n");
        encoded.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        let mut bytes = encoded.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// Route the discovery and first-probe endpoints.
///
/// Pairing, FairPlay, and media setup explicitly return 501. This is important:
/// acknowledging those stages before their cryptographic state machines exist
/// would leave the sender and receiver in inconsistent states.
pub fn route(request: &Request, receiver: &ReceiverConfig) -> Result<Response, AirPlayError> {
    route_with_crypto(request, receiver, None)
}

pub(crate) fn route_with_crypto(
    request: &Request,
    receiver: &ReceiverConfig,
    crypto: Option<&mut dyn CryptoSession>,
) -> Result<Response, AirPlayError> {
    let target = request.target.split('?').next().unwrap_or(&request.target);
    if request.method.eq_ignore_ascii_case("OPTIONS") {
        return Ok(Response::new(200, "OK").with_header(
            "Public",
            "OPTIONS, GET, POST, ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, \
             GET_PARAMETER, SET_PARAMETER",
        ));
    }

    if request.method.eq_ignore_ascii_case("GET") && matches!(target, "/info" | "/server-info") {
        return Ok(Response::new(200, "OK")
            .with_body("application/x-apple-binary-plist", info_plist(receiver)?));
    }

    let crypto_endpoint = match target {
        "/pair-setup" => Some(CryptoEndpoint::PairSetup),
        "/pair-verify" => Some(CryptoEndpoint::PairVerify),
        "/fp-setup" => Some(CryptoEndpoint::FairPlaySetup),
        _ => None,
    };
    if let (Some(endpoint), Some(crypto)) = (crypto_endpoint, crypto) {
        let body = crypto.handle(endpoint, &request.body)?;
        return Ok(Response::new(200, "OK").with_body("application/octet-stream", body));
    }
    let media_endpoint = matches!(target, "/stream" | "/feedback" | "/command" | "/audioMode");
    let media_method = matches!(
        request.method.to_ascii_uppercase().as_str(),
        "ANNOUNCE"
            | "SETUP"
            | "RECORD"
            | "PAUSE"
            | "FLUSH"
            | "TEARDOWN"
            | "GET_PARAMETER"
            | "SET_PARAMETER"
    );
    if crypto_endpoint.is_some() || media_endpoint || media_method {
        let stage = if crypto_endpoint.is_some() {
            "airplay-crypto-pending"
        } else {
            "airplay-media-pending"
        };
        return Ok(Response::new(501, "Not Implemented").with_header("X-Cursio-Stage", stage));
    }

    Ok(Response::new(404, "Not Found"))
}

fn info_plist(receiver: &ReceiverConfig) -> Result<Vec<u8>, plist::Error> {
    let mut info = Dictionary::new();
    info.insert("name".to_string(), receiver.name.clone().into());
    info.insert(
        "deviceID".to_string(),
        receiver.device_id.to_string().into(),
    );
    info.insert(
        "macAddress".to_string(),
        receiver.device_id.to_string().into(),
    );
    info.insert("model".to_string(), receiver.model.clone().into());
    info.insert(
        "sourceVersion".to_string(),
        receiver.source_version.clone().into(),
    );
    info.insert("pi".to_string(), receiver.identity.to_string().into());
    info.insert("features".to_string(), AIRPLAY_FEATURES.into());
    info.insert("statusFlags".to_string(), 4_u64.into());
    info.insert("vv".to_string(), 2_u64.into());
    info.insert(
        "pk".to_string(),
        Value::Data(receiver.pairing_public_key.to_vec()),
    );
    info.insert("keepAliveLowPower".to_string(), true.into());
    info.insert("keepAliveSendStatsAsBody".to_string(), true.into());

    let (width, height) = receiver.profile.resolution.dimensions();
    let mut display = Dictionary::new();
    display.insert("width".to_string(), u64::from(width).into());
    display.insert("height".to_string(), u64::from(height).into());
    display.insert("widthPixels".to_string(), u64::from(width).into());
    display.insert("heightPixels".to_string(), u64::from(height).into());
    display.insert("features".to_string(), 2_u64.into());
    display.insert("uuid".to_string(), "cursio_display".into());
    display.insert(
        "refreshRate".to_string(),
        Value::Real(1.0 / f64::from(receiver.profile.fps)),
    );
    display.insert("maxFPS".to_string(), u64::from(receiver.profile.fps).into());
    info.insert(
        "displays".to_string(),
        Value::Array(vec![Value::Dictionary(display)]),
    );

    let mut encoded = Vec::new();
    Value::Dictionary(info).to_writer_binary(&mut encoded)?;
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::CryptoSession;
    use crate::{Resolution, VideoProfile};
    use std::net::SocketAddr;
    use uuid::Uuid;

    fn config() -> ReceiverConfig {
        ReceiverConfig::new(
            "Test TV",
            Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 7000)),
            VideoProfile::new(Resolution::Qhd, 60).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn decodes_fragmented_request_and_body() {
        let mut decoder = RequestDecoder::new();
        decoder
            .push(b"POST /pair-setup RTSP/1.0\r\nCSeq: 3\r\nContent-L")
            .unwrap();
        assert_eq!(decoder.next_request().unwrap(), None);
        decoder.push(b"ength: 4\r\n\r\nab").unwrap();
        assert_eq!(decoder.next_request().unwrap(), None);
        decoder.push(b"cd").unwrap();
        let request = decoder.next_request().unwrap().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/pair-setup");
        assert_eq!(request.version, Version::Rtsp10);
        assert_eq!(request.header("cseq"), Some("3"));
        assert_eq!(request.body, b"abcd");
        assert!(decoder.is_empty());
    }

    #[test]
    fn decodes_pipelined_http_and_rtsp_requests() {
        let mut decoder = RequestDecoder::new();
        decoder
            .push(
                b"GET /info HTTP/1.1\r\nContent-Length: 0\r\n\r\n\
                  OPTIONS * RTSP/1.0\r\nCSeq: 2\r\n\r\n",
            )
            .unwrap();
        assert_eq!(
            decoder.next_request().unwrap().unwrap().version,
            Version::Http11
        );
        assert_eq!(
            decoder.next_request().unwrap().unwrap().version,
            Version::Rtsp10
        );
        assert!(decoder.next_request().unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_or_duplicate_content_length() {
        let mut invalid = RequestDecoder::new();
        invalid
            .push(b"POST /x RTSP/1.0\r\nContent-Length: nope\r\n\r\n")
            .unwrap();
        assert_eq!(
            invalid.next_request().unwrap_err(),
            ProtocolError::InvalidContentLength
        );

        let mut duplicate = RequestDecoder::new();
        duplicate
            .push(b"POST /x RTSP/1.0\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        assert_eq!(
            duplicate.next_request().unwrap_err(),
            ProtocolError::DuplicateContentLength
        );
    }

    #[test]
    fn rejects_control_characters_in_header_values() {
        let mut decoder = RequestDecoder::new();
        decoder
            .push(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\nInjected: yes\r\n\r\n")
            .unwrap();
        assert_eq!(
            decoder.next_request().unwrap_err(),
            ProtocolError::Malformed("invalid header value")
        );
    }

    #[test]
    fn enforces_header_and_body_limits() {
        let mut header = RequestDecoder::new();
        header.push(&vec![b'a'; MAX_HEADER_BYTES + 1]).unwrap();
        assert_eq!(
            header.next_request().unwrap_err(),
            ProtocolError::HeaderTooLarge
        );

        let mut body = RequestDecoder::new();
        body.push(
            format!(
                "POST /x RTSP/1.0\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY_BYTES + 1
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            body.next_request().unwrap_err(),
            ProtocolError::BodyTooLarge(MAX_BODY_BYTES + 1)
        );
    }

    #[test]
    fn info_is_a_binary_plist_with_receiver_metadata() {
        let response = route(
            &Request {
                method: "GET".to_string(),
                target: "/info".to_string(),
                version: Version::Rtsp10,
                headers: vec![("CSeq".to_string(), "7".to_string())],
                body: Vec::new(),
            },
            &config(),
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert!(response.body.starts_with(b"bplist00"));
        let value = Value::from_reader(std::io::Cursor::new(&response.body)).unwrap();
        let dictionary = value.as_dictionary().unwrap();
        assert_eq!(dictionary["name"].as_string(), Some("Test TV"));
        assert_eq!(
            dictionary["deviceID"].as_string(),
            Some("02:11:22:33:44:55")
        );
        assert_eq!(
            dictionary["features"].as_unsigned_integer(),
            Some(AIRPLAY_FEATURES)
        );
        assert_eq!(
            dictionary["pk"].as_data(),
            Some(config().pairing_public_key.as_slice())
        );
        let displays = dictionary["displays"].as_array().unwrap();
        assert_eq!(
            displays[0].as_dictionary().unwrap()["width"].as_unsigned_integer(),
            Some(2560)
        );
    }

    #[test]
    fn options_echoes_cseq_and_has_exact_content_length() {
        let request = Request {
            method: "OPTIONS".to_string(),
            target: "*".to_string(),
            version: Version::Rtsp10,
            headers: vec![("cseq".to_string(), "42".to_string())],
            body: Vec::new(),
        };
        let response = route(&request, &config()).unwrap();
        let encoded = response.encode(request.version, request.header("CSeq"));
        let text = std::str::from_utf8(&encoded).unwrap();
        assert!(text.contains("\r\nCSeq: 42\r\n"));
        assert!(text.ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[test]
    fn unsupported_crypto_and_media_fail_closed() {
        for (method, target, stage) in [
            ("POST", "/pair-setup", "airplay-crypto-pending"),
            ("POST", "/fp-setup", "airplay-crypto-pending"),
            ("SETUP", "/stream", "airplay-media-pending"),
        ] {
            let response = route(
                &Request {
                    method: method.to_string(),
                    target: target.to_string(),
                    version: Version::Rtsp10,
                    headers: Vec::new(),
                    body: Vec::new(),
                },
                &config(),
            )
            .unwrap();
            assert_eq!(response.status, 501);
            assert_eq!(
                response
                    .headers
                    .iter()
                    .find(|(key, _)| key == "X-Cursio-Stage")
                    .map(|(_, value)| value.as_str()),
                Some(stage)
            );
        }
    }

    struct TestCrypto {
        called: Option<CryptoEndpoint>,
    }

    impl CryptoSession for TestCrypto {
        fn handle(
            &mut self,
            endpoint: CryptoEndpoint,
            request: &[u8],
        ) -> Result<Vec<u8>, AirPlayError> {
            self.called = Some(endpoint);
            let mut response = b"reply:".to_vec();
            response.extend_from_slice(request);
            Ok(response)
        }

        fn unwrap_media_key(&mut self, _: &[u8]) -> Result<[u8; 16], AirPlayError> {
            Ok([7; 16])
        }
    }

    #[test]
    fn configured_crypto_session_handles_pairing_endpoint() {
        let request = Request {
            method: "POST".to_string(),
            target: "/fp-setup".to_string(),
            version: Version::Rtsp10,
            headers: Vec::new(),
            body: b"challenge".to_vec(),
        };
        let mut crypto = TestCrypto { called: None };
        let response = route_with_crypto(&request, &config(), Some(&mut crypto)).unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"reply:challenge");
        assert_eq!(crypto.called, Some(CryptoEndpoint::FairPlaySetup));
    }

    #[test]
    fn unknown_endpoint_is_not_found() {
        let response = route(
            &Request {
                method: "GET".to_string(),
                target: "/unknown".to_string(),
                version: Version::Http11,
                headers: Vec::new(),
                body: Vec::new(),
            },
            &config(),
        )
        .unwrap();
        assert_eq!(response.status, 404);
    }
}
