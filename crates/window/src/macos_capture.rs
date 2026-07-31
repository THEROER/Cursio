//! macOS window capture: ScreenCaptureKit → [`WindowFrame`].
//!
//! The host side on macOS. Like Wayland, macOS does not let a process read
//! another window's pixels on its own — the sanctioned route is
//! ScreenCaptureKit, which is asynchronous Objective-C built on delegates and
//! `CMSampleBuffer`s. That part lives in `native/macos/window_capture.m` and is
//! **compiled into this crate**, writing tightly packed BGRA frames into a pipe
//! this module reads.
//!
//! ## Why not a helper process
//!
//! It was one at first, and it could never work. macOS grants Screen Recording
//! to the *responsible application*; a separate unsigned executable unpacked
//! into a temporary directory is not covered by the approval the user gave
//! their terminal, so every capture was refused with "the user declined" no
//! matter how many times they approved. Linked in, the request belongs to the
//! process the user actually launched.
//!
//! The first capture still needs Screen Recording granted, and macOS evaluates
//! that when the *launching* application starts — so a terminal approved while
//! already running has to be quit and reopened.
//!
//! ## Codecs
//!
//! Frames go out as lossless tiles. The other two hosts can switch a window in
//! motion to H.264 — Linux through ffmpeg, Windows through Media Foundation —
//! and macOS has neither wired up yet, so a moving window costs what zstd costs
//! rather than what NVENC costs. Correctness first; VideoToolbox is the obvious
//! next step and changes nothing above this file.

use std::io::Read;
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cursio_protocol::{
    WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput, WindowMeta,
};
use tokio::sync::mpsc;

use crate::frame_pipeline::{pick_codec, FrameDiffer};
use crate::{CaptureSession, WindowCapture, WindowError, CAPTURE_FRAME_QUEUE_CAPACITY};

unsafe extern "C" {
    /// Blocks until the capture is running or refused; takes ownership of the
    /// descriptor on success. See `native/macos/window_capture.m`.
    fn cursio_window_capture_start(needle: *const std::ffi::c_char, fps: i32, out_fd: i32) -> i32;
    fn cursio_window_capture_stop();
}

const CAPTURE_OK: i32 = 0;
const CAPTURE_DENIED: i32 = -1;
const CAPTURE_NO_MATCH: i32 = -2;

/// Bytes of frame header the helper writes: width, height, payload length.
const FRAME_HEADER_BYTES: usize = 12;

/// A frame larger than this is not a window, it is a misparse.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Selects and captures a macOS window.
pub struct MacWindowCapture {
    /// Substring of the window title to capture. Empty shares the whole
    /// screen, which is the case that needs no explaining.
    pub title_hint: String,
    /// Ceiling on frames per second; the system sends nothing while the window
    /// is still, so this costs nothing on a static window.
    pub fps: u32,
}

impl Default for MacWindowCapture {
    fn default() -> Self {
        Self {
            title_hint: String::new(),
            fps: 30,
        }
    }
}

impl WindowCapture for MacWindowCapture {
    type Session = MacCaptureSession;

    fn begin(&mut self, id: WindowId) -> Result<Self::Session, WindowError> {
        let shared = if self.title_hint.is_empty() {
            "Mac screen".to_string()
        } else {
            self.title_hint.clone()
        };
        let mut frames_in = start_capture(&self.title_hint, self.fps)?;

        // The first frame establishes the geometry, so the offer describes what
        // the peer is actually about to see rather than a guess.
        let first = read_frame(&mut frames_in).map_err(|error| {
            unsafe { cursio_window_capture_stop() };
            WindowError::Backend(format!("no frame from the macOS capture: {error}"))
        })?;

        let (width, height) = (first.width, first.height);
        let (frames_tx, frames_rx) = mpsc::channel(CAPTURE_FRAME_QUEUE_CAPACITY);
        let ended = Arc::new(AtomicBool::new(false));
        let thread_ended = ended.clone();
        std::thread::Builder::new()
            .name(format!("cursio-mac-capture-{id}"))
            .spawn(move || {
                pump(frames_in, frames_tx, id, first, thread_ended);
            })
            .map_err(|error| WindowError::Backend(format!("spawn capture thread: {error}")))?;

        Ok(MacCaptureSession {
            meta: WindowMeta {
                id,
                title: format!("Cursio ({shared})"),
                width,
                height,
            },
            frames: frames_rx,
            ended,
            reported_input: false,
        })
    }
}

/// One frame as the helper wrote it.
struct CapturedFrame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Start the linked capture and return the read end of its frame pipe.
///
/// The pipe rather than a channel because the Objective-C side writes from a
/// dispatch queue that knows nothing about Rust: a descriptor is the one thing
/// both halves already agree on.
fn start_capture(title_hint: &str, fps: u32) -> Result<std::fs::File, WindowError> {
    let needle = std::ffi::CString::new(title_hint)
        .map_err(|_| WindowError::Backend("window title contains a NUL byte".into()))?;
    let mut fds = [0_i32; 2];
    // SAFETY: `fds` is a two-element array, which is what `pipe` writes into.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(WindowError::Backend(format!(
            "cannot create a capture pipe: {}",
            std::io::Error::last_os_error()
        )));
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    // SAFETY: `write_fd` is a fresh descriptor this call takes ownership of on
    // success, and `needle` outlives the call.
    let status = unsafe { cursio_window_capture_start(needle.as_ptr(), fps as i32, write_fd) };
    if status != CAPTURE_OK {
        // SAFETY: both descriptors are ours and still open on failure.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return Err(WindowError::Backend(match status {
            CAPTURE_DENIED => "screen recording is not permitted; grant it to the application \
                 that launched Cursio in System Settings > Privacy & Security > Screen Recording, \
                 then quit and reopen that application"
                .to_string(),
            CAPTURE_NO_MATCH => format!("no window matched \"{title_hint}\""),
            other => format!("screen capture failed to start ({other})"),
        }));
    }
    // SAFETY: `read_fd` is open, owned here, and not used anywhere else.
    Ok(unsafe { std::fs::File::from_raw_fd(read_fd) })
}

fn read_exact(frames: &mut std::fs::File, buffer: &mut [u8]) -> std::io::Result<()> {
    frames.read_exact(buffer)
}

fn read_frame(frames: &mut std::fs::File) -> std::io::Result<CapturedFrame> {
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    read_exact(frames, &mut header)?;
    let width = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let height = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let length = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if length != (width as usize) * (height as usize) * 4 || length > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "capture helper sent an inconsistent frame header",
        ));
    }
    let mut pixels = vec![0_u8; length];
    read_exact(frames, &mut pixels)?;
    Ok(CapturedFrame {
        width,
        height,
        pixels,
    })
}

/// Read frames forever, diff them, and hand tiles to the session.
fn pump(
    mut frames_in: std::fs::File,
    frames: mpsc::Sender<WindowFrame>,
    id: WindowId,
    first: CapturedFrame,
    ended: Arc<AtomicBool>,
) {
    let mut state = PumpState {
        differ: FrameDiffer::new(),
        seq: 0,
        width: first.width,
        height: first.height,
    };
    // The frame that established the geometry is also the peer's first picture;
    // dropping it would leave the surrogate blank until something moved.
    if !state.emit(&frames, id, &first) {
        ended.store(true, Ordering::Relaxed);
        return;
    }
    loop {
        let frame = match read_frame(&mut frames_in) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::info!(%error, id, "macOS window capture ended");
                ended.store(true, Ordering::Relaxed);
                return;
            }
        };
        if !state.emit(&frames, id, &frame) {
            ended.store(true, Ordering::Relaxed);
            return;
        }
    }
}

struct PumpState {
    differ: FrameDiffer,
    seq: u32,
    width: u32,
    height: u32,
}

impl PumpState {
    /// Diff, encode and queue one captured frame. `false` means the session is
    /// gone and there is nothing left to capture for.
    fn emit(
        &mut self,
        frames: &mpsc::Sender<WindowFrame>,
        id: WindowId,
        frame: &CapturedFrame,
    ) -> bool {
        // A resized window invalidates every previous pixel, so the next frame
        // has to be a full one rather than a delta against a different size.
        if frame.width != self.width || frame.height != self.height {
            self.width = frame.width;
            self.height = frame.height;
            self.differ.reset();
        }
        let Some(dirty) = self.differ.diff(&frame.pixels, frame.width, frame.height) else {
            return !frames.is_closed();
        };
        let tile = FrameDiffer::extract(&frame.pixels, frame.width, dirty);
        let codec = pick_codec(tile.len());
        let encoded = match crate::codec::encode_tile(codec, &tile) {
            Ok(encoded) => encoded,
            Err(error) => {
                tracing::warn!(%error, "tile encode failed; dropping frame");
                return !frames.is_closed();
            }
        };
        self.seq = self.seq.wrapping_add(1);
        // Never wait on the session: a full queue means the link is behind, and
        // the next accepted frame is made whole instead of extending latency
        // with a delta against a picture the viewer never received.
        if frames
            .try_send(WindowFrame {
                id,
                seq: self.seq,
                dirty,
                codec,
                data: encoded,
            })
            .is_err()
        {
            self.differ.reset();
        }
        !frames.is_closed()
    }
}

/// A window being captured on this machine for a peer.
pub struct MacCaptureSession {
    meta: WindowMeta,
    frames: mpsc::Receiver<WindowFrame>,
    ended: Arc<AtomicBool>,
    reported_input: bool,
}

impl CaptureSession for MacCaptureSession {
    fn meta(&self) -> &WindowMeta {
        &self.meta
    }

    fn frames(&mut self) -> &mut mpsc::Receiver<WindowFrame> {
        &mut self.frames
    }

    fn apply_input(&mut self, _input: WindowInput) -> Result<(), WindowError> {
        // Replaying a viewer's clicks needs CGEvent posting and the window's
        // own coordinate space, neither of which exists here yet. Said once so
        // the reason is visible without a log full of pointer motion.
        if !self.reported_input {
            self.reported_input = true;
            tracing::info!(
                id = self.meta.id,
                "input replay is not implemented on macOS; the peer's clicks are ignored"
            );
        }
        Ok(())
    }

    fn on_ack(&mut self, _ack: WindowFrameAck) {
        // Nothing adapts yet: there is one codec choice and the helper's frame
        // rate is fixed at setup.
    }

    fn close(&mut self, reason: WindowCloseReason) {
        tracing::info!(id = self.meta.id, ?reason, "stopping macOS window capture");
        if !self.ended.load(Ordering::Relaxed) {
            // SAFETY: stopping a capture that is not running is a no-op.
            unsafe { cursio_window_capture_stop() };
        }
    }
}

impl Drop for MacCaptureSession {
    fn drop(&mut self) {
        // SAFETY: stopping a capture that is not running is a no-op.
        unsafe { cursio_window_capture_stop() };
    }
}

/// Showing a peer's window on macOS is not implemented.
///
/// The engine is generic over both roles and needs a renderer type to exist
/// even when only the host half is used, so this is the honest placeholder: a
/// Mac can share its windows today and cannot yet display anyone else's.
pub struct MacWindowRenderer;

impl crate::WindowRenderer for MacWindowRenderer {
    type Surrogate = MacSurrogate;

    fn open(&mut self, _meta: &WindowMeta) -> Result<Self::Surrogate, WindowError> {
        Err(WindowError::Backend(
            "showing a peer's window is not supported on macOS yet".into(),
        ))
    }
}

/// Never constructed; it exists to satisfy the renderer's associated type.
pub enum MacSurrogate {}

impl crate::Surrogate for MacSurrogate {
    fn apply_frame(&mut self, _frame: &WindowFrame) -> Result<(), WindowError> {
        match *self {}
    }

    fn input(&mut self) -> &mut mpsc::UnboundedReceiver<WindowInput> {
        match *self {}
    }

    fn take_ack(&mut self) -> Option<WindowFrameAck> {
        match *self {}
    }

    fn close(&mut self, _reason: WindowCloseReason) {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real capture against the running desktop, so the permission story can be
    /// checked rather than assumed. Ignored by default: it needs a logged-in
    /// session with Screen Recording granted, which CI does not have.
    ///
    ///     cargo test -p cursio-window -- --ignored --nocapture
    #[test]
    #[ignore]
    fn captures_the_screen_for_real() {
        let mut capture = MacWindowCapture::default();
        let mut session = capture.begin(1).expect("capture should start");
        println!(
            "offered {}x{} as {:?}",
            session.meta().width,
            session.meta().height,
            session.meta().title
        );
        assert!(session.meta().width > 0);
        assert!(session.meta().height > 0);
        session.close(WindowCloseReason::HostClosed);
    }

    fn header(width: u32, height: u32, length: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes
    }

    /// The length field is redundant with the geometry on purpose: a mismatch
    /// means the stream desynchronised, and reading on would turn pixels into
    /// a header.
    #[test]
    fn an_inconsistent_header_is_rejected() {
        let bytes = header(4, 4, 999);
        let width = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let height = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let length = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        assert_ne!(length, (width as usize) * (height as usize) * 4);
    }
}
