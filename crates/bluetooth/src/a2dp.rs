//! This computer as a paired phone's Bluetooth speaker, the media half.
//!
//! A phone plays to a Bluetooth speaker over A2DP: it picks the speaker in
//! its output list, and from then on every app's sound goes there, with no
//! screen-capture prompt and no app of ours involved. When the speaker is
//! Cursio, that sound enters the same audio path as everything else the
//! phone sends — one route, one volume, the same way onto another machine —
//! instead of becoming a PipeWire Bluetooth node that the desktop's own
//! policy plays wherever it likes.
//!
//! This module holds what needs no socket: choosing an SBC configuration
//! from what the phone offers, and taking SBC frames out of the RTP packets
//! they arrive in. The codec itself is `libsbc`, loaded when a stream starts
//! (see the platform half).
//!
//! SBC only. AAC sounds better on an iPhone, but A2DP carries it as LATM
//! inside RTP and decoding it means a second codec library; a phone that
//! offers SBC — every A2DP source must — plays here, and adding AAC is a
//! second endpoint beside this one.

/// A2DP codec id for SBC.
pub const SBC: u8 = 0x00;

/// What this sink accepts, in the 4-byte SBC capability layout: 44.1 and
/// 48 kHz; mono, dual, stereo, joint stereo; 4, 8, 12, 16 blocks; 4 and 8
/// subbands; SNR and loudness allocation; bitpool 2..=53.
pub const SBC_CAPABILITIES: [u8; 4] = [0x3F, 0xFF, 2, 53];

/// The highest bitpool the A2DP specification recommends for high quality
/// joint stereo at 48 kHz; asking for more only costs air time.
const MAX_BITPOOL: u8 = 53;

/// Choose one configuration out of what the phone offers: the highest rate,
/// joint stereo, 16 blocks, 8 subbands, loudness, and the widest bitpool
/// both allow. `None` when the offer shares nothing with
/// [`SBC_CAPABILITIES`].
pub fn select_sbc(offered: &[u8]) -> Option<[u8; 4]> {
    let [freq_mode, blocks_alloc, min_pool, max_pool] = *offered else {
        return None;
    };
    let (ours_fm, ours_ba) = (SBC_CAPABILITIES[0], SBC_CAPABILITIES[1]);
    let fm = freq_mode & ours_fm;
    let ba = blocks_alloc & ours_ba;
    // Frequency is the high nibble (16, 32, 44.1, 48 kHz from bit 7 down);
    // channel mode the low (mono, dual, stereo, joint from bit 3 down).
    let freq = [0x10, 0x20].into_iter().find(|f| fm & f != 0)?;
    let mode = [0x01, 0x02, 0x04, 0x08].into_iter().find(|m| fm & m != 0)?;
    // Block length is the high nibble (4, 8, 12, 16 from bit 7 down),
    // subbands bits 3..2 (4, 8), allocation bits 1..0 (SNR, loudness).
    let blocks = [0x10, 0x20, 0x40, 0x80].into_iter().find(|b| ba & b != 0)?;
    let subbands = [0x04, 0x08].into_iter().find(|s| ba & s != 0)?;
    let alloc = [0x01, 0x02].into_iter().find(|a| ba & a != 0)?;
    let min = min_pool.max(SBC_CAPABILITIES[2]);
    let max = max_pool.min(MAX_BITPOOL);
    if min > max {
        return None;
    }
    Some([freq | mode, blocks | subbands | alloc, min, max])
}

/// The sample rate a chosen configuration plays at.
pub fn sbc_rate(config: &[u8]) -> Option<u32> {
    match config.first()? & 0xF0 {
        0x80 => Some(16_000),
        0x40 => Some(32_000),
        0x20 => Some(44_100),
        0x10 => Some(48_000),
        _ => None,
    }
}

/// The SBC frames out of one A2DP media packet: a 12-byte RTP header (more
/// with CSRCs or an extension), then one byte whose low nibble counts the
/// frames, then the frames back to back.
pub fn sbc_payload(packet: &[u8]) -> Option<&[u8]> {
    let first = *packet.first()?;
    if first >> 6 != 2 {
        return None;
    }
    let csrcs = usize::from(first & 0x0F);
    let mut at = 12 + 4 * csrcs;
    if first & 0x10 != 0 {
        let ext = packet.get(at + 2..at + 4)?;
        at += 4 + 4 * usize::from(u16::from_be_bytes([ext[0], ext[1]]));
    }
    // Skip the SBC media payload header; the frames carry their own sync.
    packet.get(at + 1..).filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phone_offering_everything_gets_48_khz_joint_stereo_loudness() {
        // Android's usual offer: every option, bitpool 2..=53.
        let chosen = select_sbc(&[0xFF, 0xFF, 2, 53]).unwrap();
        assert_eq!(chosen, [0x10 | 0x01, 0x10 | 0x04 | 0x01, 2, 53]);
        assert_eq!(sbc_rate(&chosen), Some(48_000));
    }

    #[test]
    fn a_44_1_khz_only_phone_is_taken_at_its_rate_and_bitpool() {
        let chosen = select_sbc(&[0x21, 0x15, 20, 35]).unwrap();
        assert_eq!(sbc_rate(&chosen), Some(44_100));
        assert_eq!(&chosen[2..], &[20, 35]);
    }

    #[test]
    fn an_offer_with_no_rate_in_common_is_refused() {
        assert_eq!(select_sbc(&[0xC1, 0xFF, 2, 53]), None);
        assert_eq!(select_sbc(&[0xFF, 0xFF, 60, 70]), None);
        assert_eq!(select_sbc(&[0xFF]), None);
    }

    #[test]
    fn frames_start_after_the_rtp_header_and_the_frame_count() {
        let mut packet = vec![0x80, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0x03];
        packet.extend_from_slice(&[0x9C, 1, 2]);
        assert_eq!(sbc_payload(&packet), Some(&[0x9C, 1, 2][..]));
        assert_eq!(sbc_payload(&[0x40; 20]), None);
    }
}
