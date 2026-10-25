//! Fresh SDP records for peers whose stack allocates a dynamic RFCOMM channel.
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use quick_xml::{
    events::{BytesStart, Event},
    Reader,
};

const OUTPUT_LIMIT: usize = 64 * 1024;
const QUERY_TIMEOUT: Duration = Duration::from_secs(8);

/// BlueZ's helper performs a fresh service query rather than using Device1's
/// pairing-time cache. Bound both its lifetime and the amount of output read.
pub(super) fn channels(address: &str) -> io::Result<Vec<u8>> {
    let mut child = Command::new("sdptool")
        .args(["browse", "--xml", address])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(OUTPUT_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error),
            Ok(None) if Instant::now() >= deadline => {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SDP query timed out",
                ))
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let bytes = reader
        .join()
        .map_err(|_| io::Error::other("SDP output reader failed"))??;
    if bytes.len() > OUTPUT_LIMIT {
        return Err(invalid("SDP output exceeds its limit"));
    }
    let status = result?;
    if !status.success() {
        return Err(io::Error::other(format!("SDP lookup exited with {status}")));
    }
    parse(std::str::from_utf8(&bytes).map_err(invalid)?)
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn attribute(
    event: &BytesStart<'_>,
    key: &[u8],
    reader: &Reader<&[u8]>,
) -> io::Result<Option<String>> {
    event
        .try_get_attribute(key)
        .map_err(invalid)?
        .map(|attribute| {
            attribute
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
                .map(|v| v.into_owned())
                .map_err(invalid)
        })
        .transpose()
}

fn number(value: &str) -> Option<u16> {
    match value.strip_prefix("0x") {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => value.parse().ok(),
    }
}

/// sdptool emits one XML document per record, preceded by a status line.
fn parse(mut xml: &str) -> io::Result<Vec<u8>> {
    let mut channels = Vec::new();
    while let Some(start) = xml.find("<record>") {
        let record = &xml[start..];
        let end = record
            .find("</record>")
            .ok_or_else(|| invalid("truncated SDP record"))?
            + "</record>".len();
        for channel in parse_record(&record[..end])? {
            if !channels.contains(&channel) {
                channels.push(channel);
            }
        }
        xml = &record[end..];
    }
    Ok(channels)
}

fn parse_record(xml: &str) -> io::Result<Vec<u8>> {
    let mut reader = Reader::from_str(xml);
    let (mut attr, mut depth, mut rfcomm) = (None, 0usize, None);
    let mut mesh = false;
    let mut channels = Vec::new();
    loop {
        match reader.read_event().map_err(invalid)? {
            Event::Start(event) => match event.name().as_ref() {
                b"attribute" => {
                    attr = attribute(&event, b"id", &reader)?.and_then(|v| number(&v));
                }
                b"sequence" => {
                    depth += 1;
                    if depth > 16 {
                        return Err(invalid("SDP nesting exceeds its limit"));
                    }
                }
                _ => {}
            },
            Event::Empty(event) => {
                let value = attribute(&event, b"value", &reader)?.unwrap_or_default();
                match (attr, event.name().as_ref()) {
                    (Some(1), b"uuid") if value.eq_ignore_ascii_case(crate::MESH_UUID) => {
                        mesh = true
                    }
                    (Some(4), b"uuid") if number(&value) == Some(3) => rfcomm = Some(depth),
                    (Some(4), b"uint8") if rfcomm == Some(depth) => {
                        if let Some(channel @ 1..=30) = number(&value) {
                            channels.push(channel as u8);
                        }
                    }
                    _ => {}
                }
            }
            Event::End(event) => match event.name().as_ref() {
                b"sequence" => {
                    if rfcomm == Some(depth) {
                        rfcomm = None;
                    }
                    depth = depth
                        .checked_sub(1)
                        .ok_or_else(|| invalid("unbalanced SDP sequence"))?;
                }
                b"attribute" => {
                    attr = None;
                    rfcomm = None;
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(if mesh { channels } else { Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(uuid: &str, channel: u8) -> String {
        format!(
            r#"<record><attribute id="0x0001"><sequence><uuid value="{uuid}"/></sequence></attribute><attribute id="0x0004"><sequence><sequence><uuid value="0x0100"/><uint8 value="23"/></sequence><sequence><uuid value="0x0003"/><uint8 value="{channel}"/></sequence></sequence></attribute></record>"#
        )
    }

    #[test]
    fn dynamic_channels_are_taken_only_from_the_mesh_service_and_rfcomm_descriptor() {
        let xml = format!(
            "Browsing paired device ...\n<?xml version=\"1.0\"?>{}<?xml version=\"1.0\"?>{}",
            record("other-service", 23),
            record(crate::MESH_UUID, 4)
        );
        assert_eq!(parse(&xml).unwrap(), [4]);
    }

    #[test]
    fn stale_duplicate_or_invalid_channels_do_not_create_extra_candidates() {
        let xml = [
            record(crate::MESH_UUID, 4),
            record(crate::MESH_UUID, 4),
            record(crate::MESH_UUID, 0),
            record(crate::MESH_UUID, 31),
        ]
        .concat();
        assert_eq!(parse(&xml).unwrap(), [4]);
        assert!(parse(&record("not-cursio", 4)).unwrap().is_empty());
    }

    #[test]
    fn malformed_and_deep_records_are_rejected() {
        assert!(parse("<record><attribute>").is_err());
        assert!(parse("<record><sequence></attribute></record>").is_err());
        let xml = format!(
            "<record>{}{}</record>",
            "<sequence>".repeat(17),
            "</sequence>".repeat(17)
        );
        assert!(parse(&xml).is_err());
    }
}
