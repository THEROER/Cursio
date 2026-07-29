use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use super::VideoCodec;
use crate::AirPlayError;

static HELPER_ID: AtomicU64 = AtomicU64::new(0);
static VIDEO_SINK_HELPER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/cursio-airplay-video-sink"));

/// Native AVSampleBufferDisplayLayer presentation hosted in a small AppKit
/// helper. Keeping AppKit's event loop in that helper lets the Tokio CLI retain
/// its own main thread while VideoToolbox decodes on the Apple GPU.
pub struct HardwareVideoPipeline {
    child: Child,
    input: Option<ChildStdin>,
}

impl HardwareVideoPipeline {
    pub fn new(codec: VideoCodec) -> Result<Self, AirPlayError> {
        if codec != VideoCodec::H264 {
            return Err(AirPlayError::Video(
                "the macOS AirPlay backend currently accepts H.264 only".into(),
            ));
        }

        let helper_path = std::env::temp_dir().join(format!(
            "cursio-airplay-video-sink-{}-{}",
            std::process::id(),
            HELPER_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut helper = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&helper_path)
            .map_err(|error| {
                AirPlayError::Video(format!("could not prepare the macOS video helper: {error}"))
            })?;
        if let Err(error) = helper
            .write_all(VIDEO_SINK_HELPER)
            .and_then(|_| helper.flush())
        {
            let _ = std::fs::remove_file(&helper_path);
            return Err(AirPlayError::Video(format!(
                "could not install the macOS video helper: {error}"
            )));
        }
        drop(helper);

        let mut child = match Command::new(&helper_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_file(&helper_path);
                return Err(AirPlayError::Video(format!(
                    "could not start the macOS VideoToolbox window: {error}"
                )));
            }
        };
        let _ = std::fs::remove_file(&helper_path);
        let input = child
            .stdin
            .take()
            .ok_or_else(|| AirPlayError::Video("macOS video helper has no input pipe".into()))?;
        Ok(Self {
            child,
            input: Some(input),
        })
    }

    pub fn push_access_unit(
        &mut self,
        bytes: &[u8],
        _pts: Option<std::time::Duration>,
    ) -> Result<(), AirPlayError> {
        let length = u32::try_from(bytes.len())
            .map_err(|_| AirPlayError::Video("H.264 access unit is too large".into()))?;
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| AirPlayError::Video("macOS video window is closed".into()))?;
        input
            .write_all(&length.to_be_bytes())
            .and_then(|_| input.write_all(bytes))
            .map_err(|error| {
                AirPlayError::Video(format!(
                    "macOS video window stopped receiving frames: {error}"
                ))
            })
    }
}

impl Drop for HardwareVideoPipeline {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
