//! What a panel says about itself: who made it, its serial, its model name and
//! how big its picture is.
//!
//! The operating systems name a monitor by the port it is plugged into, and
//! do not agree even on that — GNOME's `HDMI-1` is the kernel's `HDMI-A-1` —
//! so a screen is recognised by its EDID instead, which is the same bytes
//! wherever it is read. The size comes from the first detailed timing, in
//! millimetres; the coarse centimetre field is the fallback, and a field that
//! only encodes an aspect ratio (a projector's) counts as no size at all.

const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// A detailed timing smaller than this in both directions is an aspect ratio
/// written into the size fields (16 by 9), not a picture anyone could sit at.
const SMALLEST_REAL_MM: u32 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edid {
    /// The three-letter PNP maker code (`GSM` is LG).
    pub maker: String,
    /// The serial string descriptor, else the numeric serial, else none.
    pub serial: Option<String>,
    /// The model name descriptor.
    pub model: Option<String>,
    pub size_mm: Option<(u32, u32)>,
}

impl Edid {
    pub(crate) fn parse(bytes: &[u8]) -> Option<Edid> {
        if bytes.len() < 128 || bytes[..8] != HEADER {
            return None;
        }
        let code = u16::from_be_bytes([bytes[8], bytes[9]]);
        let letter = |shift: u16| char::from(b'@' + ((code >> shift) & 0x1f) as u8);
        let maker: String = [letter(10), letter(5), letter(0)].into_iter().collect();
        let numeric = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);

        let mut serial = None;
        let mut model = None;
        let mut size_mm = None;
        for block in bytes[54..126].as_chunks::<18>().0 {
            if block[0] == 0 && block[1] == 0 {
                match block[3] {
                    0xff => serial = descriptor_text(block),
                    0xfc => model = descriptor_text(block),
                    _ => {}
                }
            } else if size_mm.is_none() {
                let width = u32::from(block[12]) | (u32::from(block[14] & 0xf0) << 4);
                let height = u32::from(block[13]) | (u32::from(block[14] & 0x0f) << 8);
                size_mm = real_size(width, height);
            }
        }
        let size_mm =
            size_mm.or_else(|| real_size(u32::from(bytes[21]) * 10, u32::from(bytes[22]) * 10));
        let serial = serial.or_else(|| (numeric != 0).then(|| numeric.to_string()));
        Some(Edid {
            maker,
            serial,
            model,
            size_mm,
        })
    }

    /// The screen's stable id: maker and serial, or maker and model for a
    /// panel with no serial (two of those are then told apart by nothing).
    pub(crate) fn id(&self) -> String {
        match (&self.serial, &self.model) {
            (Some(serial), _) => format!("{}:{serial}", self.maker),
            (None, Some(model)) => format!("{}:{model}", self.maker),
            (None, None) => self.maker.clone(),
        }
    }
}

fn real_size(width: u32, height: u32) -> Option<(u32, u32)> {
    (width >= SMALLEST_REAL_MM || height >= SMALLEST_REAL_MM)
        .then_some((width, height))
        .filter(|&(w, h)| w > 0 && h > 0)
}

/// A display descriptor's text: up to 13 bytes, ended by a line feed and
/// padded with spaces.
fn descriptor_text(block: &[u8]) -> Option<String> {
    let text = &block[5..18];
    let end = text.iter().position(|&b| b == b'\n').unwrap_or(text.len());
    let text = String::from_utf8_lossy(&text[..end]).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LG: &[u8] = include_bytes!("testdata/lg-22mp.edid");
    const PHILIPS: &[u8] = include_bytes!("testdata/philips-243v7.edid");

    #[test]
    fn reads_maker_serial_model_and_size_from_a_real_panel() {
        let lg = Edid::parse(LG).unwrap();
        assert_eq!(lg.maker, "GSM");
        assert_eq!(lg.serial.as_deref(), Some("502NDWE0C929"));
        assert_eq!(lg.model.as_deref(), Some("LG IPS FULLHD"));
        assert_eq!(lg.size_mm, Some((480, 270)));
        assert_eq!(lg.id(), "GSM:502NDWE0C929");

        let philips = Edid::parse(PHILIPS).unwrap();
        assert_eq!(philips.maker, "PHL");
        assert_eq!(philips.model.as_deref(), Some("PHL 243V7"));
        assert_eq!(philips.size_mm, Some((527, 296)));
    }

    #[test]
    fn an_aspect_ratio_in_the_size_fields_is_no_size() {
        let mut bytes = LG.to_vec();
        // A projector: the timing's size says 16 by 9, the coarse field 0.
        bytes[54 + 12] = 16;
        bytes[54 + 13] = 9;
        bytes[54 + 14] = 0;
        bytes[21] = 0;
        bytes[22] = 0;
        assert_eq!(Edid::parse(&bytes).unwrap().size_mm, None);
    }

    #[test]
    fn falls_back_to_centimetres_without_a_timing_size() {
        let mut bytes = LG.to_vec();
        bytes[54 + 12] = 0;
        bytes[54 + 13] = 0;
        bytes[54 + 14] = 0;
        assert_eq!(Edid::parse(&bytes).unwrap().size_mm, Some((480, 270)));
    }

    #[test]
    fn rejects_bytes_that_are_not_an_edid() {
        assert_eq!(Edid::parse(&[0u8; 128]), None);
        assert_eq!(Edid::parse(&LG[..100]), None);
    }
}
