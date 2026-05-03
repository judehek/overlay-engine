//! Windows.Graphics.Capture pipeline for the WebView2 visual tree.
//!
//! We capture the root `ContainerVisual` of the composition tree
//! (which has the WebView2 `web_visual` as a child), give the framepool
//! our shared D3D11 device, and on each `FrameArrived` callback copy the
//! captured texture into the staging shared-KMT texture and notify the
//! engine over `frame_tx`.

use anyhow::{Context, Result};
use tokio::sync::mpsc as tokio_mpsc;
use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::UI::Composition::{ContainerVisual, Visual};
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

use super::d3d;
use super::frame::FrameUpdate;

/// Turn a `ContainerVisual` into a `GraphicsCaptureItem` scoped to that
/// visual's subtree. ContainerVisual upcasts to Visual via WinRT
/// inheritance so we can hand it to `CreateFromVisual`.
pub(crate) fn capture_item_from_visual(visual: &ContainerVisual) -> Result<GraphicsCaptureItem> {
    let v: Visual = visual.cast().context("ContainerVisual -> Visual")?;
    GraphicsCaptureItem::CreateFromVisual(&v).context("CreateFromVisual")
}

/// Inputs to [`start_capture`]: identifies the texture to copy into and
/// where to notify when a new frame is ready.
pub(crate) struct CaptureSetup<'a> {
    pub winrt_device: &'a IDirect3DDevice,
    pub capture_item: &'a GraphicsCaptureItem,
    pub d3d_ctx: ID3D11DeviceContext,
    pub staging_texture: ID3D11Texture2D,
    pub staging_mutex: IDXGIKeyedMutex,
    pub staging_handle: u32,
    pub width: u32,
    pub height: u32,
    pub frame_tx: tokio_mpsc::UnboundedSender<FrameUpdate>,
}

/// Live capture handles. Keep alive for the duration of the session;
/// dropping closes the framepool and capture session.
pub(crate) struct LiveCapture {
    pub frame_pool: Direct3D11CaptureFramePool,
    pub session: GraphicsCaptureSession,
}

impl LiveCapture {
    /// Tear down the capture session before dropping. Idempotent — calling
    /// `Close` twice is a no-op.
    pub(crate) fn close(&self) {
        let _ = self.session.Close();
        let _ = self.frame_pool.Close();
    }
}

/// Build the Direct3D11CaptureFramePool, register the FrameArrived
/// handler that copies into the staging texture and notifies `frame_tx`,
/// and start the capture session.
///
/// Hides the yellow "being captured" border on Windows 10 2104+ where
/// available.
pub(crate) fn start_capture(setup: CaptureSetup<'_>) -> Result<LiveCapture> {
    let CaptureSetup {
        winrt_device,
        capture_item,
        d3d_ctx,
        staging_texture,
        staging_mutex,
        staging_handle,
        width,
        height,
        frame_tx,
    } = setup;

    let frame_pool = Direct3D11CaptureFramePool::Create(
        winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        SizeInt32 {
            Width: width as i32,
            Height: height as i32,
        },
    )
    .context("Direct3D11CaptureFramePool::Create")?;

    // FrameArrived fires on the DispatcherQueue thread (us). It pulls the
    // next frame texture, copies it into the shared staging texture, and
    // pings the engine that a new handle is ready.
    let ctx_clone = d3d_ctx;
    let staging_texture_clone = staging_texture;
    let staging_mutex_clone = staging_mutex;
    let frame_tx_clone = frame_tx;

    let handler = TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
        move |pool, _args| {
            let pool = pool
                .as_ref()
                .ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(-1)))?;
            let frame = match pool.TryGetNextFrame() {
                Ok(f) => f,
                Err(_) => return Ok(()),
            };
            if let Err(err) = d3d::copy_into_staging(
                &frame,
                &ctx_clone,
                &staging_texture_clone,
                &staging_mutex_clone,
            ) {
                eprintln!("[overlay-engine] copy_into_staging failed: {err:?}");
            } else {
                let _ = frame_tx_clone.send(FrameUpdate {
                    handle: staging_handle,
                    width,
                    height,
                });
            }
            Ok(())
        },
    );
    let _token = frame_pool.FrameArrived(&handler).context("FrameArrived")?;

    let session = frame_pool
        .CreateCaptureSession(capture_item)
        .context("CreateCaptureSession")?;
    // Hide the yellow "being captured" border on Windows 10 2104+; the
    // call is a no-op on older builds (returns E_NOTIMPL, which we drop).
    let _ = session.SetIsBorderRequired(false);
    session.StartCapture().context("StartCapture")?;

    Ok(LiveCapture { frame_pool, session })
}
