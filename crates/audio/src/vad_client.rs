//! User-mode client for the Cursio virtual audio driver (`native/cursio-vad`).
//!
//! This is the Windows half of device parity. On Linux every peer device/app
//! becomes a real PipeWire node; Windows has no user-space way to publish an
//! audio endpoint, so a kernel driver does it and this module drives that
//! driver over its IOCTL bridge.
//!
//! Per peer endpoint the driver creates a **pair** of native Windows devices:
//!
//! * a **render** device (a Windows *output*) — apps play into it, and
//!   [`VadSource`] pulls that PCM back out and hands it to the network, so the
//!   peer's speakers are a selectable output here;
//! * a **capture** device (a Windows *input*) — [`VadSink`] pushes PCM arriving
//!   from the peer into it, so what plays on the peer is available locally as a
//!   recording source.
//!
//! Both directions are polled on a dedicated thread at the wire packet rate;
//! the driver's ring buffers absorb the jitter between that loop and the audio
//! clock, so neither side has to keep the other's timing.
//!
//! The struct shapes here deliberately mirror [`WasapiSource`]/[`WasapiSink`]
//! (an unbounded channel of [`AudioChunk`], a stop signal, a join handle) so
//! callers in `lib.rs` treat a driver-backed stream exactly like a WASAPI one.
//!
//! [`WasapiSource`]: crate::wasapi_backend::WasapiSource
//! [`WasapiSink`]: crate::wasapi_backend::WasapiSink

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use cursio_protocol::{AudioChunk, AudioCodec, AudioStreamId, AudioStreamMeta, StreamRole};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::AudioError;

// --- The IOCTL contract, mirrored from native/cursio-vad/Shared/CursioIoctl.h ---
//
// These MUST stay in lock-step with that header; the driver rejects a mismatched
// PROTOCOL_VERSION at HELLO rather than misparsing audio, so bump both sides
// together whenever a struct or code below changes.

const CONTROL_DEVICE_PATH: &str = r"\\.\CursioAudio";
const PROTOCOL_VERSION: u32 = 1;

const DEVICE_TYPE: u32 = 0x8039;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;

/// Rebuild of the `CTL_CODE` macro from the Windows DDK.
const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

const fn cursio_ioctl(function: u32) -> u32 {
    ctl_code(DEVICE_TYPE, function, METHOD_BUFFERED, FILE_ANY_ACCESS)
}

const IOCTL_HELLO: u32 = cursio_ioctl(0x800);
const IOCTL_ADD_ENDPOINT: u32 = cursio_ioctl(0x801);
const IOCTL_REMOVE_ENDPOINT: u32 = cursio_ioctl(0x802);
const IOCTL_PUSH_CAPTURE: u32 = cursio_ioctl(0x803);
const IOCTL_PULL_RENDER: u32 = cursio_ioctl(0x804);

const MAX_NAME_CHARS: usize = 128;

#[repr(C)]
#[derive(Clone, Copy)]
struct PcmFormat {
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
}

#[repr(C)]
struct EndpointDesc {
    id: u32,
    format: PcmFormat,
    name: [u16; MAX_NAME_CHARS],
}

#[repr(C)]
struct EndpointIdArg {
    id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PcmChunkHeader {
    id: u32,
    seq: u32,
    bytes: u32,
    reserved: u32,
}

#[repr(C)]
struct HelloReply {
    driver_protocol_version: u32,
}

/// How much PCM to move per poll. At the wire format (48 kHz stereo 16-bit)
/// this is 10 ms — the same cadence the WASAPI path uses, small enough to keep
/// latency low and large enough that the syscall rate stays negligible.
const CHUNK_BYTES: usize = 1920;

/// Poll period for both directions. Slightly under the chunk duration so the
/// loop stays ahead of the ring rather than letting it accumulate.
const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// An open handle to the driver's control device.
///
/// Wrapping the raw `HANDLE` gives it a `Drop` that closes it, and lets the
/// polling threads own their own handle rather than sharing one across threads.
struct VadDevice {
    handle: HANDLE,
}

// The control device handle is used from the polling threads. Windows file
// handles are safe to use from any thread, and each VadDevice is owned by
// exactly one thread at a time.
unsafe impl Send for VadDevice {}

impl VadDevice {
    /// Open the control device and complete the version handshake.
    ///
    /// Returns [`AudioError::Unsupported`] when the device isn't there at all,
    /// which is the normal case on a machine where the driver isn't installed —
    /// callers treat that as "no parity available", not as a failure.
    fn open() -> Result<Self, AudioError> {
        let path: Vec<u16> = CONTROL_DEVICE_PATH
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .map_err(|_| AudioError::Unsupported)?;

        let device = VadDevice { handle };
        device.handshake()?;
        Ok(device)
    }

    /// Exchange protocol versions; a mismatch means the driver and this binary
    /// disagree about the wire structs, so refuse rather than send audio.
    fn handshake(&self) -> Result<(), AudioError> {
        let request = PROTOCOL_VERSION;
        let mut reply = HelloReply {
            driver_protocol_version: 0,
        };

        self.ioctl(
            IOCTL_HELLO,
            Some(as_bytes(&request)),
            Some(as_bytes_mut(&mut reply)),
        )?;

        if reply.driver_protocol_version != PROTOCOL_VERSION {
            return Err(AudioError::Backend(format!(
                "Cursio audio driver speaks protocol v{}, this build speaks v{PROTOCOL_VERSION}; \
                 reinstall the driver to match",
                reply.driver_protocol_version
            )));
        }
        Ok(())
    }

    /// Issue one `DeviceIoControl`, returning the number of bytes written into
    /// `output`.
    fn ioctl(
        &self,
        code: u32,
        input: Option<&[u8]>,
        output: Option<&mut [u8]>,
    ) -> Result<u32, AudioError> {
        let (in_ptr, in_len) = match input {
            Some(buf) => (buf.as_ptr() as *const core::ffi::c_void, buf.len() as u32),
            None => (std::ptr::null(), 0),
        };
        let (out_ptr, out_len) = match output {
            Some(buf) => (buf.as_mut_ptr() as *mut core::ffi::c_void, buf.len() as u32),
            None => (std::ptr::null_mut(), 0),
        };

        let mut returned: u32 = 0;
        unsafe {
            DeviceIoControl(
                self.handle,
                code,
                if in_len > 0 { Some(in_ptr) } else { None },
                in_len,
                if out_len > 0 { Some(out_ptr) } else { None },
                out_len,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| AudioError::Backend(format!("driver ioctl {code:#x} failed: {e}")))?;

        Ok(returned)
    }

    /// Ask the driver to publish a render+capture pair for this stream.
    fn add_endpoint(&self, id: AudioStreamId, display_name: &str) -> Result<(), AudioError> {
        let mut name = [0u16; MAX_NAME_CHARS];
        // Truncate rather than fail: the name is cosmetic (it's what shows in the
        // Windows sound panel), and the driver requires NUL termination.
        for (slot, ch) in name
            .iter_mut()
            .take(MAX_NAME_CHARS - 1)
            .zip(display_name.encode_utf16())
        {
            *slot = ch;
        }

        let desc = EndpointDesc {
            id: endpoint_id(id),
            format: wire_pcm_format(),
            name,
        };

        self.ioctl(IOCTL_ADD_ENDPOINT, Some(as_bytes(&desc)), None)?;
        Ok(())
    }

    /// Tear the endpoint pair back down.
    fn remove_endpoint(&self, id: AudioStreamId) -> Result<(), AudioError> {
        let arg = EndpointIdArg {
            id: endpoint_id(id),
        };
        self.ioctl(IOCTL_REMOVE_ENDPOINT, Some(as_bytes(&arg)), None)?;
        Ok(())
    }
}

impl Drop for VadDevice {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// The driver keys endpoints by a 32-bit id; derive it from the stream id.
fn endpoint_id(id: AudioStreamId) -> u32 {
    id as u32
}

fn wire_pcm_format() -> PcmFormat {
    let f = crate::wasapi_backend::wire_format();
    PcmFormat {
        sample_rate: f.sample_rate,
        channels: f.channels as u16,
        // The driver bridge is fixed at s16 (see CursioIoctl.h); wire_format
        // uses the same, so there's nothing to negotiate yet.
        bits_per_sample: 16,
    }
}

/// Bytes per frame (one sample for every channel) at the wire format.
fn frame_bytes() -> usize {
    let f = crate::wasapi_backend::wire_format();
    f.channels as usize * 2 // s16 = 2 bytes per sample
}

/// View a `#[repr(C)]` value as the byte buffer the IOCTL expects.
fn as_bytes<T>(value: &T) -> &[u8] {
    // Safe: T is repr(C) and POD (plain integers / fixed arrays), and the slice
    // borrows `value`, so it can't outlive it.
    unsafe { std::slice::from_raw_parts(value as *const T as *const u8, std::mem::size_of::<T>()) }
}

fn as_bytes_mut<T>(value: &mut T) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(value as *mut T as *mut u8, std::mem::size_of::<T>()) }
}

/// Is the Cursio virtual audio driver installed and speaking our protocol?
///
/// Used to decide whether real per-device parity is available before falling
/// back to a degraded path.
pub fn driver_available() -> bool {
    VadDevice::open().is_ok()
}

// --- Render: a Windows OUTPUT device whose audio goes to the peer ---

/// A native Windows **output** device representing a peer speaker.
///
/// Apps select it in the sound panel and play into it; this pulls the rendered
/// PCM out of the driver and emits it as [`AudioChunk`]s for the network layer.
pub struct VadSource {
    id: AudioStreamId,
    adverts_rx: tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta>,
    chunks_rx: tokio::sync::mpsc::UnboundedReceiver<AudioChunk>,
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VadSource {
    /// Publish a Windows output device named `description` and start pulling
    /// whatever is played into it.
    pub fn virtual_sink(
        id: AudioStreamId,
        name: String,
        description: String,
    ) -> Result<Self, AudioError> {
        let device = VadDevice::open()?;
        device.add_endpoint(id, &description)?;

        let (adverts_tx, adverts_rx) = tokio::sync::mpsc::unbounded_channel();
        let (chunks_tx, chunks_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();

        let _ = adverts_tx.send(AudioStreamMeta {
            id,
            role: StreamRole::Playback,
            app_name: None,
            // (platform handle, display name): the endpoint the driver just
            // published is addressed by the name we gave it.
            device: Some((name, description)),
            format: crate::wasapi_backend::wire_format(),
            codec: AudioCodec::RawPcm,
        });

        let thread = std::thread::Builder::new()
            .name(format!("cursio-vad-render-{id}"))
            .spawn(move || run_render_loop(id, device, chunks_tx, stop_rx))
            .map_err(|e| AudioError::Backend(format!("spawn render thread: {e}")))?;

        Ok(VadSource {
            id,
            adverts_rx,
            chunks_rx,
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }

    pub fn advertisements(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioStreamMeta> {
        &mut self.adverts_rx
    }

    pub fn chunks(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<AudioChunk> {
        &mut self.chunks_rx
    }

    pub fn id(&self) -> AudioStreamId {
        self.id
    }
}

impl Drop for VadSource {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // The thread owns the device handle and removes the endpoint on its way
        // out, so the Windows device disappears when the stream does.
    }
}

/// Poll the driver for rendered PCM and forward it as chunks.
fn run_render_loop(
    id: AudioStreamId,
    device: VadDevice,
    chunks_tx: tokio::sync::mpsc::UnboundedSender<AudioChunk>,
    stop_rx: std_mpsc::Receiver<()>,
) {
    let request = PcmChunkHeader {
        id: endpoint_id(id),
        ..Default::default()
    };
    let mut reply = vec![0u8; std::mem::size_of::<PcmChunkHeader>() + CHUNK_BYTES];
    let mut seq: u32 = 0;
    let mut timestamp: u64 = 0;

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let returned = match device.ioctl(
            IOCTL_PULL_RENDER,
            Some(as_bytes(&request)),
            Some(&mut reply),
        ) {
            Ok(n) => n as usize,
            Err(e) => {
                tracing::warn!(stream = %id, error = %e, "vad: render pull failed, stopping");
                break;
            }
        };

        let header_len = std::mem::size_of::<PcmChunkHeader>();
        if returned < header_len {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }

        // How many PCM bytes actually came back. The driver never pads, so an
        // idle output simply yields zero and we just wait.
        let bytes = u32::from_ne_bytes([reply[8], reply[9], reply[10], reply[11]]) as usize;
        let available = bytes.min(returned - header_len);

        if available == 0 {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }

        let data = reply[header_len..header_len + available].to_vec();
        let frames = (available / frame_bytes()) as u64;

        if chunks_tx
            .send(AudioChunk {
                id,
                seq,
                timestamp,
                data,
            })
            .is_err()
        {
            // Receiver dropped: nobody is listening any more.
            break;
        }

        seq = seq.wrapping_add(1);
        timestamp = timestamp.wrapping_add(frames);
    }

    let _ = device.remove_endpoint(id);
}

// --- Capture: a Windows INPUT device fed with the peer's audio ---

/// A native Windows **input** device carrying audio from the peer.
///
/// Whatever the peer plays is pushed in here, so local apps (a mixer, a
/// recorder, a call app) can select it as a recording source.
pub struct VadSink {
    id: AudioStreamId,
    pcm_tx: Option<std_mpsc::Sender<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VadSink {
    /// Publish a Windows input device named `display_name`, ready to be fed.
    pub fn open(id: AudioStreamId, display_name: String) -> Result<Self, AudioError> {
        let device = VadDevice::open()?;
        device.add_endpoint(id, &display_name)?;

        let (pcm_tx, pcm_rx) = std_mpsc::channel::<Vec<u8>>();

        let thread = std::thread::Builder::new()
            .name(format!("cursio-vad-capture-{id}"))
            .spawn(move || run_capture_loop(id, device, pcm_rx))
            .map_err(|e| AudioError::Backend(format!("spawn capture thread: {e}")))?;

        Ok(VadSink {
            id,
            pcm_tx: Some(pcm_tx),
            thread: Some(thread),
        })
    }

    /// Hand one decoded chunk to the driver.
    ///
    /// Non-blocking: the chunk is queued for the pushing thread, and the
    /// driver's ring drops the oldest audio if the consumer ever falls behind,
    /// so a stalled reader can't back-pressure the network path.
    pub fn play(&self, chunk: &AudioChunk) -> Result<(), AudioError> {
        match &self.pcm_tx {
            Some(tx) => tx
                .send(chunk.data.clone())
                .map_err(|_| AudioError::Backend("vad capture thread has stopped".into())),
            None => Err(AudioError::Backend("vad sink is closed".into())),
        }
    }

    pub fn id(&self) -> AudioStreamId {
        self.id
    }
}

impl Drop for VadSink {
    fn drop(&mut self) {
        // Dropping the sender ends the loop, which removes the endpoint.
        self.pcm_tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Push queued PCM into the driver's capture ring.
fn run_capture_loop(id: AudioStreamId, device: VadDevice, pcm_rx: std_mpsc::Receiver<Vec<u8>>) {
    let header_len = std::mem::size_of::<PcmChunkHeader>();
    let mut buffer = Vec::with_capacity(header_len + CHUNK_BYTES);
    let mut seq: u32 = 0;

    // Ends when the sender is dropped (VadSink::drop), which is our stop signal.
    while let Ok(pcm) = pcm_rx.recv() {
        if pcm.is_empty() {
            continue;
        }

        let header = PcmChunkHeader {
            id: endpoint_id(id),
            seq,
            bytes: pcm.len() as u32,
            reserved: 0,
        };

        buffer.clear();
        buffer.extend_from_slice(as_bytes(&header));
        buffer.extend_from_slice(&pcm);

        if let Err(e) = device.ioctl(IOCTL_PUSH_CAPTURE, Some(&buffer), None) {
            tracing::warn!(stream = %id, error = %e, "vad: capture push failed, stopping");
            break;
        }

        seq = seq.wrapping_add(1);
    }

    let _ = device.remove_endpoint(id);
}
