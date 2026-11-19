//! One still picture of a phone's screen: taken by the phone's own shell
//! when it has one ([`from_png`]), else from the stream already arriving.
//!
//! The stream is decoded from its newest keyframe with the decoder a viewer
//! window uses, and only the last picture is kept. That needed the screen
//! shared — a consent on the phone and a window on the computer — and showed
//! the screen as the stream had it, seconds behind; `screencap` needs
//! neither and shows now. It is written as a PNG
//! file in this user's data directory rather than handed back over the
//! control socket: a phone's screen as PNG is a few megabytes, past what one
//! control line carries, and the reader (the CLI, the MCP server) is on the
//! same machine anyway.

use std::path::{Path, PathBuf};

use super::remote::Stream;

#[cfg(target_os = "linux")]
use cursio_window::video_decoder::VideoDecoder;
#[cfg(windows)]
use cursio_window::video_decoder_win::VideoDecoder;

/// Screenshots kept in the directory; older ones are removed.
const KEEP: usize = 10;

/// A picture written to a file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Shot {
    pub path: PathBuf,
    /// The picture's size as written.
    pub width: u32,
    pub height: u32,
}

/// Where screenshots go.
pub fn dir() -> Option<PathBuf> {
    crate::data_dir::data_dir().map(|d| d.join("screenshots"))
}

/// Decode `stream` and write its newest picture as a PNG in `dir`, no longer
/// than `max_side` on its long side when given.
pub fn take(stream: &Stream, max_side: Option<u32>, dir: &Path) -> Result<Shot, String> {
    let mut decoder =
        VideoDecoder::open().map_err(|e| format!("no H.264 decoder for the picture: {e}"))?;
    let mut newest = None;
    for unit in &stream.units {
        match decoder.decode(unit) {
            Ok(frames) => {
                if let Some(frame) = frames.into_iter().last() {
                    newest = Some(frame);
                }
            }
            Err(error) => tracing::debug!(%error, "a unit of the phone's stream did not decode"),
        }
    }
    let frame = newest.ok_or_else(|| {
        format!(
            "{}'s stream did not decode to a picture yet; try again in a moment",
            stream.phone
        )
    })?;
    let mut rgba = frame.pixels;
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
    let image = image::RgbaImage::from_raw(frame.width, frame.height, rgba)
        .ok_or("the decoded picture has the wrong size for its pixels")?;
    save(image, &stream.phone, max_side, dir)
}

/// Write the PNG `png` the phone took itself (`screencap`, through its own
/// shell) in `dir`, scaled as [`take`] scales.
pub fn from_png(
    png: &[u8],
    phone: &str,
    max_side: Option<u32>,
    dir: &Path,
) -> Result<Shot, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| format!("{phone}'s screenshot is not a picture: {e}"))?
        .into_rgba8();
    save(image, phone, max_side, dir)
}

/// Scale `image` to `max_side` on its long side when given, and write it as
/// a PNG in `dir`.
fn save(
    mut image: image::RgbaImage,
    phone: &str,
    max_side: Option<u32>,
    dir: &Path,
) -> Result<Shot, String> {
    if let Some(max) = max_side.filter(|m| *m > 0) {
        let long = image.width().max(image.height());
        if long > max {
            let scale = max as f32 / long as f32;
            let (w, h) = (
                ((image.width() as f32 * scale).round() as u32).max(1),
                ((image.height() as f32 * scale).round() as u32).max(1),
            );
            image = image::imageops::resize(&image, w, h, image::imageops::FilterType::Triangle);
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe: String = phone
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = dir.join(format!("{safe}-{stamp}.png"));
    image
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    prune(dir);
    Ok(Shot {
        path,
        width: image.width(),
        height: image.height(),
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use cursio_window::video_encoder::{Fidelity, VideoEncoder};

    #[test]
    fn a_screenshot_the_phone_took_is_kept_scaled_to_fit() {
        let image = image::RgbaImage::from_pixel(600, 1200, image::Rgba([0, 255, 0, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let dir = std::env::temp_dir().join(format!("cursio-own-shot-{}", std::process::id()));
        let shot = from_png(&png, "Test Phone", Some(600), &dir).unwrap();
        assert_eq!((shot.width, shot.height), (300, 600));
        assert!(shot.path.exists());
        assert!(from_png(b"no picture", "Test Phone", None, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_phones_stream_becomes_a_png_of_its_newest_picture_scaled_to_fit() {
        let (w, h) = (320, 640);
        let mut encoder = VideoEncoder::open(w, h, Fidelity::Lossy { bitrate: 2_000_000 })
            .expect("an H.264 encoder on this machine");
        // Red then blue; the shot must show the second.
        let mut units = Vec::new();
        for colour in [[0u8, 0, 255, 255], [255, 0, 0, 255]] {
            let bgra: Vec<u8> = colour.repeat((w * h) as usize);
            for _ in 0..3 {
                units.extend(encoder.encode(&bgra).unwrap());
            }
        }
        units.extend(encoder.finish().unwrap());
        let stream = Stream {
            phone: "Test Phone".into(),
            width: w,
            height: h,
            units,
        };
        let dir = std::env::temp_dir().join(format!("cursio-shot-{}", std::process::id()));
        let shot = take(&stream, Some(320), &dir).unwrap();
        assert_eq!((shot.width, shot.height), (160, 320), "the long side fits");
        let image = image::open(&shot.path).unwrap().to_rgba8();
        let middle = image.get_pixel(80, 160).0;
        assert!(
            middle[2] > 200 && middle[0] < 60,
            "blue, not red: {middle:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Keep the newest [`KEEP`] screenshots in `dir`.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut shots: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    shots.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    let excess = shots.len().saturating_sub(KEEP);
    for old in shots.into_iter().take(excess) {
        let _ = std::fs::remove_file(old);
    }
}
