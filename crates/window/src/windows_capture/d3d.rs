//! D3D11 device + the `Windows.Graphics.Capture` session driving one window.
//!
//! This is the part `tools/wgc-probe` already proved works: capture keeps
//! delivering frames at full rate even when the source window is invisible or
//! offscreen (only minimising stops it). The probe counted frames; this returns
//! the textures.
//!
//! Frames arrive on a **free-threaded** pool, so they are pushed into a channel
//! and pulled by the capture thread on its own schedule. That keeps the
//! callback short — it must never do encoding work, since it runs on a thread
//! the system owns.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

/// How many textures the capture pool holds.
///
/// Two is the documented minimum and what we want: a deeper pool would let the
/// system queue *old* frames while we fall behind, which is exactly the "buffer
/// stale frames" failure the design forbids. Shallow means a slow consumer
/// drops frames instead of accumulating latency.
const POOL_SIZE: i32 = 2;

/// Create a hardware D3D11 device for capture.
pub fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            // Capture surfaces are BGRA; without this flag the device may
            // refuse to bind them.
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| format!("create D3D11 device: {e}"))?;

        Ok((
            device.ok_or("no D3D11 device returned")?,
            context.ok_or("no D3D11 context returned")?,
        ))
    }
}

/// One captured frame, already copied out of the GPU pool.
pub struct CapturedFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A running capture of one window.
pub struct CaptureStream {
    /// Kept alive: dropping either stops the capture.
    _session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    /// Needed to resize the pool; not `Send`, so it lives here on the capture
    /// thread rather than in the FrameArrived handler.
    d3d: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
    /// Geometry the pool currently produces.
    pool_size: windows::Graphics::SizeInt32,
    /// Geometry the frames say the window actually has.
    wanted: Arc<Mutex<windows::Graphics::SizeInt32>>,
    frames: Receiver<CapturedFrame>,
}

impl CaptureStream {
    /// Begin capturing `hwnd`.
    pub fn start(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        hwnd: HWND,
    ) -> Result<Self, String> {
        unsafe {
            let dxgi: IDXGIDevice = device.cast().map_err(|e| format!("IDXGIDevice: {e}"))?;
            let d3d = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
                .map_err(|e| format!("WinRT D3D device: {e}"))?;
            let d3d: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice =
                d3d.cast().map_err(|e| format!("IDirect3DDevice: {e}"))?;

            // A capture item for a specific HWND only exists through interop —
            // the WinRT type has no by-handle constructor.
            let interop: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                    .map_err(|e| format!("capture interop: {e}"))?;
            let item: GraphicsCaptureItem = interop
                .CreateForWindow(hwnd)
                .map_err(|e| format!("CreateForWindow: {e}"))?;
            let size = item.Size().map_err(|e| format!("item size: {e}"))?;

            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &d3d,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                POOL_SIZE,
                size,
            )
            .map_err(|e| format!("frame pool: {e}"))?;

            let (tx, frames) = std::sync::mpsc::channel();

            // Copy the pixels out INSIDE the handler, and let the frame drop
            // before returning — the same shape OBS uses.
            //
            // Previously the GPU texture itself was sent down the channel, so a
            // frame stayed checked out of the pool until the capture loop got
            // round to it. The pool only has POOL_SIZE (2) slots and stops
            // raising FrameArrived once they are all held, so the capture could
            // stall with no error anywhere — which is what we saw: `next_frame`
            // returning None forever while the process looked healthy.
            //
            // Copying here also keeps the D3D texture (a COM object with
            // apartment affinity) on the thread that produced it; only plain
            // bytes cross to the capture loop.
            let cb_device = device.clone();
            let cb_context = context.clone();
            let mut staging: Option<ID3D11Texture2D> = None;
            // The size the pool was built for, shared with the capture loop.
            //
            // The window's size at capture time is only a starting guess: an app
            // still laying out its UI reports something tiny (Voicemeeter gave
            // 160x28), and the pool would keep that size forever — the peer saw
            // a small strip of the window stretched to fill the viewer.
            //
            // The handler cannot call `Recreate` itself: that needs the
            // `IDirect3DDevice`, which is not `Send` and so cannot be captured
            // here. It therefore only *reports* the size it sees; `next_frame`,
            // running on the capture thread that owns the device, does the
            // resize.
            let wanted = Arc::new(Mutex::new(size));
            let cb_wanted = wanted.clone();
            pool.FrameArrived(&TypedEventHandler::new(
                move |pool: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
                    let Some(pool) = pool.as_ref() else {
                        return Ok(());
                    };
                    // The frame must be taken every time or the pool stalls
                    // once its slots are full.
                    let Ok(frame) = pool.TryGetNextFrame() else {
                        return Ok(());
                    };
                    let copied = (|| {
                        let surface = frame.Surface().ok()?;
                        let access = surface
                            .cast::<windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess>()
                            .ok()?;
                        let texture = access.GetInterface::<ID3D11Texture2D>().ok()?;
                        crate::windows_capture::texture_to_bgra(
                            &cb_device,
                            &cb_context,
                            &texture,
                            &mut staging,
                        )
                        .ok()
                    })();
                    if let Some((pixels, width, height)) = copied {
                        // A closed receiver just means we're shutting down.
                        let _ = tx.send(CapturedFrame {
                            pixels,
                            width,
                            height,
                        });
                    }
                    // Follow the window's real size, taken from the frame we
                    // just received rather than from the capture item — the item
                    // is not `Send` and cannot live in this handler.
                    //
                    // `Recreate` is the documented way to resize a live capture;
                    // without it the pool keeps producing frames of the original
                    // geometry forever.
                    // Report the window's real size; the capture loop resizes.
                    if let Ok(content) = frame.ContentSize() {
                        if content.Width > 0 && content.Height > 0 {
                            if let Ok(mut w) = cb_wanted.lock() {
                                *w = content;
                            }
                        }
                    }
                    // `frame` drops here, returning its slot to the pool.
                    Ok(())
                },
            ))
            .map_err(|e| format!("FrameArrived: {e}"))?;

            let session = pool
                .CreateCaptureSession(&item)
                .map_err(|e| format!("capture session: {e}"))?;

            // Windows draws a yellow border around captured windows by default.
            // For a window that is meant to look like it *moved* to the other
            // machine, that border would be a visible lie — and the window is
            // invisible here anyway. Best-effort: older builds lack the setting.
            let _ = session.SetIsBorderRequired(false);

            session
                .StartCapture()
                .map_err(|e| format!("StartCapture: {e}"))?;

            Ok(Self {
                _session: session,
                pool,
                d3d,
                pool_size: size,
                wanted,
                frames,
            })
        }
    }

    /// The newest frame, if one has arrived.
    ///
    /// Returns the **latest** available, discarding any that queued up behind
    /// it: an older frame is worthless once a newer one exists, and showing it
    /// would add latency for no benefit.
    pub fn next_frame(&mut self) -> Result<Option<CapturedFrame>, String> {
        // Resize the pool if the frames say the window grew or shrank. Done here
        // because it needs the D3D device, which the handler cannot hold.
        if let Ok(wanted) = self.wanted.lock() {
            let w = *wanted;
            drop(wanted);
            if w.Width != self.pool_size.Width || w.Height != self.pool_size.Height {
                match self.pool.Recreate(
                    &self.d3d,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    POOL_SIZE,
                    w,
                ) {
                    Ok(()) => {
                        tracing::info!(
                            from = format!("{}x{}", self.pool_size.Width, self.pool_size.Height),
                            to = format!("{}x{}", w.Width, w.Height),
                            "capture pool resized"
                        );
                        self.pool_size = w;
                    }
                    Err(e) => tracing::warn!(error = %e, "could not resize capture pool"),
                }
            }
        }

        let mut newest = None;
        loop {
            match self.frames.try_recv() {
                Ok(texture) => newest = Some(texture),
                Err(TryRecvError::Empty) => return Ok(newest),
                Err(TryRecvError::Disconnected) => {
                    return if newest.is_some() {
                        Ok(newest)
                    } else {
                        Err("capture session closed".into())
                    }
                }
            }
        }
    }
}
