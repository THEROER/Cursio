//! Bounded framing for the AirPlay screen-mirroring TCP stream.

pub const MIRROR_HEADER_BYTES: usize = 128;
pub const MAX_MIRROR_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_BUFFERED_BYTES: usize = MIRROR_HEADER_BYTES + MAX_MIRROR_PAYLOAD_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadType {
    Video,
    CodecConfig,
    Heartbeat,
    Unknown(u16),
}

impl From<u16> for PayloadType {
    fn from(value: u16) -> Self {
        match value {
            0 => Self::Video,
            1 => Self::CodecConfig,
            2 => Self::Heartbeat,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorPacket {
    pub payload_type: PayloadType,
    pub flags: u16,
    pub timestamp: u64,
    pub payload: Vec<u8>,
}

/// Reads through a [`CursorBuffer`](crate::cursor_buffer::CursorBuffer): at
/// 60 fps the stream delivers thousands of packets a second.
#[derive(Debug, Default)]
pub struct MirrorPacketDecoder {
    buf: crate::cursor_buffer::CursorBuffer,
}

impl MirrorPacketDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), MirrorError> {
        if !self.buf.push(bytes, MAX_BUFFERED_BYTES) {
            return Err(MirrorError::BufferTooLarge);
        }
        Ok(())
    }

    pub fn next_packet(&mut self) -> Result<Option<MirrorPacket>, MirrorError> {
        let pending = self.pending();
        if pending.len() < MIRROR_HEADER_BYTES {
            return Ok(None);
        }
        let payload_len =
            u32::from_le_bytes(pending[..4].try_into().expect("four-byte payload length")) as usize;
        if payload_len > MAX_MIRROR_PAYLOAD_BYTES {
            return Err(MirrorError::PayloadTooLarge(payload_len));
        }
        let packet_len = MIRROR_HEADER_BYTES
            .checked_add(payload_len)
            .ok_or(MirrorError::PayloadTooLarge(payload_len))?;
        if pending.len() < packet_len {
            return Ok(None);
        }

        // Byte 4 selects the payload kind. Byte 5 is a video subtype flag
        // (notably 0x10 for an IDR frame), not the high byte of a u16 type.
        // Treating 00 10 as type 0x1000 skips the first encrypted IDR payload
        // and permanently desynchronizes the continuous AES-CTR stream.
        let payload_type = u16::from(pending[4]).into();
        let flags = u16::from_le_bytes([pending[6], pending[7]]);
        let timestamp =
            u64::from_le_bytes(pending[8..16].try_into().expect("eight-byte timestamp"));
        let payload = pending[MIRROR_HEADER_BYTES..packet_len].to_vec();
        self.buf.consume(packet_len);
        Ok(Some(MirrorPacket {
            payload_type,
            flags,
            timestamp,
            payload,
        }))
    }

    fn pending(&self) -> &[u8] {
        self.buf.pending()
    }
}

/// H.264 decoder configuration from an AVCDecoderConfigurationRecord (`avcC`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
    nal_length_bytes: usize,
    annex_b_parameter_sets: Vec<u8>,
}

impl AvcConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self, MirrorError> {
        if bytes.len() < 7 || bytes[0] != 1 {
            return Err(MirrorError::InvalidAvcConfig);
        }
        let nal_length_bytes = usize::from((bytes[4] & 0x03) + 1);
        let mut offset = 6;
        let mut parameter_sets = Vec::new();
        let sps_count = bytes[5] & 0x1f;
        for _ in 0..sps_count {
            append_parameter_set(bytes, &mut offset, &mut parameter_sets)?;
        }
        let pps_count = *bytes.get(offset).ok_or(MirrorError::InvalidAvcConfig)?;
        offset += 1;
        for _ in 0..pps_count {
            append_parameter_set(bytes, &mut offset, &mut parameter_sets)?;
        }
        if parameter_sets.is_empty() {
            return Err(MirrorError::InvalidAvcConfig);
        }
        Ok(Self {
            nal_length_bytes,
            annex_b_parameter_sets: parameter_sets,
        })
    }

    pub fn parameter_sets(&self) -> &[u8] {
        &self.annex_b_parameter_sets
    }

    /// Convert one AVC access unit to Annex-B start-code framing.
    pub fn convert_access_unit(&self, bytes: &[u8]) -> Result<Vec<u8>, MirrorError> {
        let mut annex_b = Vec::with_capacity(bytes.len() + 16);
        self.convert_access_unit_into(bytes, &mut annex_b)?;
        Ok(annex_b)
    }

    /// Append the Annex-B form of one access unit to `annex_b`.
    ///
    /// The caller owns the buffer so the mirroring path can reuse one
    /// allocation for every frame instead of one per frame at 60 fps.
    pub fn convert_access_unit_into(
        &self,
        bytes: &[u8],
        annex_b: &mut Vec<u8>,
    ) -> Result<(), MirrorError> {
        let mut offset = 0;
        while offset < bytes.len() {
            let end = offset
                .checked_add(self.nal_length_bytes)
                .filter(|end| *end <= bytes.len())
                .ok_or(MirrorError::InvalidAccessUnit)?;
            let mut nal_len = 0_usize;
            for byte in &bytes[offset..end] {
                nal_len = nal_len
                    .checked_mul(256)
                    .and_then(|value| value.checked_add(usize::from(*byte)))
                    .ok_or(MirrorError::InvalidAccessUnit)?;
            }
            offset = end;
            let nal_end = offset
                .checked_add(nal_len)
                .filter(|end| *end <= bytes.len())
                .ok_or(MirrorError::InvalidAccessUnit)?;
            if nal_len == 0 {
                return Err(MirrorError::InvalidAccessUnit);
            }
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(&bytes[offset..nal_end]);
            offset = nal_end;
        }
        Ok(())
    }
}

fn append_parameter_set(
    bytes: &[u8],
    offset: &mut usize,
    output: &mut Vec<u8>,
) -> Result<(), MirrorError> {
    let length_end = offset.checked_add(2).ok_or(MirrorError::InvalidAvcConfig)?;
    let length_bytes = bytes
        .get(*offset..length_end)
        .ok_or(MirrorError::InvalidAvcConfig)?;
    let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
    *offset = length_end;
    let data_end = offset
        .checked_add(length)
        .ok_or(MirrorError::InvalidAvcConfig)?;
    let data = bytes
        .get(*offset..data_end)
        .ok_or(MirrorError::InvalidAvcConfig)?;
    if data.is_empty() {
        return Err(MirrorError::InvalidAvcConfig);
    }
    output.extend_from_slice(&[0, 0, 0, 1]);
    output.extend_from_slice(data);
    *offset = data_end;
    Ok(())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MirrorError {
    #[error("mirror receive buffer limit exceeded")]
    BufferTooLarge,
    #[error("mirror payload is {0} bytes, limit is {MAX_MIRROR_PAYLOAD_BYTES}")]
    PayloadTooLarge(usize),
    #[error("invalid AVC decoder configuration")]
    InvalidAvcConfig,
    #[error("invalid length-prefixed AVC access unit")]
    InvalidAccessUnit,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(payload_type: u16, timestamp: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; MIRROR_HEADER_BYTES];
        bytes[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&payload_type.to_le_bytes());
        bytes[8..16].copy_from_slice(&timestamp.to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn fragmented_and_pipelined_packets_decode() {
        let first = packet(1, 7, b"config");
        let second = packet(0, 8, b"frame");
        let mut decoder = MirrorPacketDecoder::new();
        decoder.push(&first[..50]).unwrap();
        assert!(decoder.next_packet().unwrap().is_none());
        decoder.push(&first[50..]).unwrap();
        decoder.push(&second).unwrap();

        let decoded = decoder.next_packet().unwrap().unwrap();
        assert_eq!(decoded.payload_type, PayloadType::CodecConfig);
        assert_eq!(decoded.timestamp, 7);
        assert_eq!(decoded.payload, b"config");
        assert_eq!(
            decoder.next_packet().unwrap().unwrap().payload_type,
            PayloadType::Video
        );
        assert!(decoder.next_packet().unwrap().is_none());
    }

    #[test]
    fn idr_subtype_is_still_encrypted_video() {
        let mut bytes = packet(0, 7, b"idr");
        bytes[5] = 0x10;
        let mut decoder = MirrorPacketDecoder::new();
        decoder.push(&bytes).unwrap();
        assert_eq!(
            decoder.next_packet().unwrap().unwrap().payload_type,
            PayloadType::Video
        );
    }

    /// The buffer is consumed with a cursor, so a long stream must keep
    /// decoding correctly while the backing allocation stays bounded.
    #[test]
    fn long_streams_decode_without_growing_the_buffer() {
        let mut decoder = MirrorPacketDecoder::new();
        for index in 0..512_u16 {
            decoder
                .push(&packet(0, u64::from(index), &[index as u8; 200]))
                .unwrap();
            let decoded = decoder.next_packet().unwrap().unwrap();
            assert_eq!(decoded.timestamp, u64::from(index));
            assert_eq!(decoded.payload, [index as u8; 200]);
            assert!(decoder.next_packet().unwrap().is_none());
        }
        assert!(decoder.buf.held() <= 2 * (MIRROR_HEADER_BYTES + 200));
    }

    #[test]
    fn oversized_payload_is_rejected_from_header_alone() {
        let mut bytes = vec![0; MIRROR_HEADER_BYTES];
        bytes[..4].copy_from_slice(&((MAX_MIRROR_PAYLOAD_BYTES + 1) as u32).to_le_bytes());
        let mut decoder = MirrorPacketDecoder::new();
        decoder.push(&bytes).unwrap();
        assert_eq!(
            decoder.next_packet().unwrap_err(),
            MirrorError::PayloadTooLarge(MAX_MIRROR_PAYLOAD_BYTES + 1)
        );
    }

    #[test]
    fn avcc_parameter_sets_and_access_units_become_annex_b() {
        let avcc = [
            1, 100, 0, 31, 0xff, 0xe1, 0, 3, 0x67, 0x64, 0x00, 1, 0, 2, 0x68, 0xee,
        ];
        let config = AvcConfig::parse(&avcc).unwrap();
        assert_eq!(
            config.parameter_sets(),
            &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0, 0, 0, 1, 0x68, 0xee]
        );

        let access_unit = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x06];
        assert_eq!(
            config.convert_access_unit(&access_unit).unwrap(),
            &[0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x06]
        );
    }

    #[test]
    fn malformed_avc_lengths_are_rejected() {
        let avcc = [
            1, 100, 0, 31, 0xff, 0xe1, 0, 3, 0x67, 0x64, 0x00, 1, 0, 2, 0x68, 0xee,
        ];
        let config = AvcConfig::parse(&avcc).unwrap();
        assert_eq!(
            config.convert_access_unit(&[0, 0, 0, 5, 0x65]).unwrap_err(),
            MirrorError::InvalidAccessUnit
        );
        assert_eq!(
            AvcConfig::parse(&avcc[..8]).unwrap_err(),
            MirrorError::InvalidAvcConfig
        );
    }
}
