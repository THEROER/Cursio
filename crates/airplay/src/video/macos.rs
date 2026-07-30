use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use super::VideoCodec;
use crate::AirPlayError;

static HELPER_ID: AtomicU64 = AtomicU64::new(0);
static SHARED_HELPER: OnceLock<Mutex<Option<Arc<Mutex<HelperProcess>>>>> = OnceLock::new();
static VIDEO_SINK_HELPER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/cursio-airplay-video-sink"));

/// Roughly a quarter second at 60 fps. Deep enough to absorb a presentation
/// hiccup, shallow enough that recovering never means playing back stale video.
const MAX_QUEUED_FRAMES: usize = 16;

pub fn prepare_window() -> Result<(), AirPlayError> {
    shared_helper().map(|_| ())
}

/// Native AVSampleBufferDisplayLayer presentation hosted in a small AppKit
/// helper. Keeping AppKit's event loop in that helper lets the Tokio CLI retain
/// its own main thread while VideoToolbox decodes on the Apple GPU.
pub struct HardwareVideoPipeline {
    _helper: Arc<Mutex<HelperProcess>>,
    frames: Arc<FrameQueue>,
}

struct HelperProcess {
    child: Child,
    frames: Arc<FrameQueue>,
    writer: Option<JoinHandle<()>>,
}

impl HardwareVideoPipeline {
    pub fn new(codec: VideoCodec) -> Result<Self, AirPlayError> {
        if codec != VideoCodec::H264 {
            return Err(AirPlayError::Video(
                "the macOS AirPlay backend currently accepts H.264 only".into(),
            ));
        }
        let helper = shared_helper()?;
        let frames = helper
            .lock()
            .map_err(|_| AirPlayError::Video("macOS video helper lock is poisoned".into()))?
            .frames
            .clone();
        Ok(Self {
            _helper: helper,
            frames,
        })
    }

    /// Hand one access unit to the helper's writer thread.
    ///
    /// The pipe write itself is blocking and the helper stalls whenever AppKit
    /// is busy, so it must not run on a Tokio worker: one backed-up window
    /// would otherwise freeze unrelated mesh work on the same thread.
    pub fn push_access_unit(
        &mut self,
        bytes: &[u8],
        _pts: Option<std::time::Duration>,
    ) -> Result<(), AirPlayError> {
        u32::try_from(bytes.len())
            .map_err(|_| AirPlayError::Video("H.264 access unit is too large".into()))?;
        self.frames.push(bytes)
    }
}

/// Bounded frame hand-off. When presentation cannot keep up the oldest frame is
/// dropped rather than the newest: mirroring recovers at the next IDR, whereas
/// an unbounded queue trades latency and memory for frames nobody will see.
#[derive(Default)]
struct FrameQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

#[derive(Default)]
struct QueueState {
    frames: VecDeque<Vec<u8>>,
    closed: bool,
    dropped: u64,
}

impl FrameQueue {
    fn push(&self, bytes: &[u8]) -> Result<(), AirPlayError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AirPlayError::Video("macOS video queue lock is poisoned".into()))?;
        if state.closed {
            return Err(AirPlayError::Video(
                "the macOS video window stopped receiving frames".into(),
            ));
        }
        if state.frames.len() == MAX_QUEUED_FRAMES {
            state.frames.pop_front();
            state.dropped += 1;
            let dropped = state.dropped;
            if dropped.is_power_of_two() {
                tracing::debug!(dropped, "macOS video window is behind; dropped a frame");
            }
        }
        state.frames.push_back(bytes.to_vec());
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(frame) = state.frames.pop_front() {
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).ok()?;
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.frames.clear();
        }
        self.ready.notify_all();
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
        let frames = Arc::new(FrameQueue::default());
        let writer = std::thread::Builder::new()
            .name("cursio-airplay-video".into())
            .spawn({
                let frames = frames.clone();
                move || write_frames(input, &frames)
            })
            .map_err(|error| {
                AirPlayError::Video(format!("could not start the macOS video writer: {error}"))
            })?;
        Ok(Self {
            child,
            frames,
            writer: Some(writer),
        })
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

fn write_frames(mut input: ChildStdin, frames: &FrameQueue) {
    while let Some(frame) = frames.pop() {
        let length = frame.len() as u32;
        if let Err(error) = input
            .write_all(&length.to_be_bytes())
            .and_then(|_| input.write_all(&frame))
        {
            tracing::warn!(%error, "macOS video window stopped receiving frames");
            break;
        }
    }
    frames.close();
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        self.frames.close();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
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
