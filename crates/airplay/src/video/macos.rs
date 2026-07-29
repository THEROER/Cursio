use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::VideoCodec;
use crate::AirPlayError;

static HELPER_ID: AtomicU64 = AtomicU64::new(0);
static SHARED_HELPER: OnceLock<Mutex<Option<Arc<Mutex<HelperProcess>>>>> = OnceLock::new();
static VIDEO_SINK_HELPER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/cursio-airplay-video-sink"));

pub fn prepare_window() -> Result<(), AirPlayError> {
    shared_helper().map(|_| ())
}

/// Native AVSampleBufferDisplayLayer presentation hosted in a small AppKit
/// helper. Keeping AppKit's event loop in that helper lets the Tokio CLI retain
/// its own main thread while VideoToolbox decodes on the Apple GPU.
pub struct HardwareVideoPipeline {
    helper: Arc<Mutex<HelperProcess>>,
}

struct HelperProcess {
    child: Child,
    input: ChildStdin,
}

impl HardwareVideoPipeline {
    pub fn new(codec: VideoCodec) -> Result<Self, AirPlayError> {
        if codec != VideoCodec::H264 {
            return Err(AirPlayError::Video(
                "the macOS AirPlay backend currently accepts H.264 only".into(),
            ));
        }
        Ok(Self {
            helper: shared_helper()?,
        })
    }

    pub fn push_access_unit(
        &mut self,
        bytes: &[u8],
        _pts: Option<std::time::Duration>,
    ) -> Result<(), AirPlayError> {
        let length = u32::try_from(bytes.len())
            .map_err(|_| AirPlayError::Video("H.264 access unit is too large".into()))?;
        let mut helper = self
            .helper
            .lock()
            .map_err(|_| AirPlayError::Video("macOS video helper lock is poisoned".into()))?;
        helper
            .input
            .write_all(&length.to_be_bytes())
            .and_then(|_| helper.input.write_all(bytes))
            .map_err(|error| {
                AirPlayError::Video(format!(
                    "macOS video window stopped receiving frames: {error}"
                ))
            })
    }
}

impl HelperProcess {
    fn spawn() -> Result<Self, AirPlayError> {
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
        Ok(Self { child, input })
    }

    fn is_running(&mut self) -> Result<bool, AirPlayError> {
        self.child
            .try_wait()
            .map(|status| status.is_none())
            .map_err(|error| {
                AirPlayError::Video(format!("could not inspect the macOS video window: {error}"))
            })
    }
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn shared_helper() -> Result<Arc<Mutex<HelperProcess>>, AirPlayError> {
    let registry = SHARED_HELPER.get_or_init(|| Mutex::new(None));
    let mut slot = registry
        .lock()
        .map_err(|_| AirPlayError::Video("macOS video registry lock is poisoned".into()))?;
    if let Some(helper) = slot.as_ref() {
        let is_running = helper
            .lock()
            .map_err(|_| AirPlayError::Video("macOS video helper lock is poisoned".into()))?
            .is_running()?;
        if is_running {
            return Ok(helper.clone());
        }
    }

    let helper = Arc::new(Mutex::new(HelperProcess::spawn()?));
    *slot = Some(helper.clone());
    Ok(helper)
}
