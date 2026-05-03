//! D3D11 device + shared-KMT staging texture used to hand WebView2 frames
//! to asdf-overlay.
//!
//! Pipeline:
//! 1. We create a single D3D11 device on the WebView2 thread and use it
//!    for both the Graphics.Capture framepool and the staging texture.
//! 2. The staging texture is a `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`
//!    `B8G8R8A8_UNORM` 2D texture; its `GetSharedHandle()` is what the
//!    engine forwards to asdf-overlay.
//! 3. Each captured frame, [`copy_into_staging`] holds the staging keyed
//!    mutex on slot 0 and `CopyResource`s the captured texture into it.

use std::ptr;

use anyhow::{Context, Result};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGIKeyedMutex, IDXGIResource};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Graphics::Capture::Direct3D11CaptureFrame;
use windows::core::Interface;

/// Create a hardware D3D11 device with BGRA support enabled (required by
/// WebView2 composition). Returns the device and its immediate context.
pub(crate) fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE(ptr::null_mut()),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
    }
    .context("D3D11CreateDevice")?;
    let device = device.context("no device")?;
    let ctx = ctx.context("no context")?;
    Ok((device, ctx))
}

/// Wrap our `ID3D11Device` in a WinRT `IDirect3DDevice` so the
/// Graphics.Capture framepool can accept it.
pub(crate) fn wrap_d3d11_as_winrt(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast().context("cast to IDXGIDevice")?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .context("CreateDirect3D11DeviceFromDXGIDevice")?;
    inspectable.cast().context("cast to IDirect3DDevice")
}

/// A shared-KMT `ID3D11Texture2D` plus the keyed mutex and OS shared
/// handle we hand out to asdf-overlay's [`OverlaySurface`].
///
/// Recreated on size change (not yet implemented) — the framepool can
/// resize itself with `Recreate`, but the staging texture's shared
/// handle changes when the texture is recreated, so the engine has to
/// re-issue an `UpdateSharedHandle` to the game window in that case.
pub(crate) struct StagingTexture {
    pub texture: ID3D11Texture2D,
    pub keyed_mutex: IDXGIKeyedMutex,
    /// Shared handle as a `u32` — that's the form asdf-overlay's API
    /// (`UpdateSharedHandle`) expects.
    pub shared_handle: u32,
    pub width: u32,
    pub height: u32,
}

impl StagingTexture {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe {
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
                },
                None,
                Some(&mut texture),
            )
        }
        .context("CreateTexture2D (staging)")?;
        let texture = texture.context("staging texture is None")?;
        let keyed_mutex: IDXGIKeyedMutex = texture.cast().context("cast to IDXGIKeyedMutex")?;
        let shared_handle = unsafe {
            texture
                .cast::<IDXGIResource>()
                .context("cast to IDXGIResource")?
                .GetSharedHandle()
                .context("GetSharedHandle")?
        };
        Ok(Self {
            texture,
            keyed_mutex,
            shared_handle: shared_handle.0 as u32,
            width,
            height,
        })
    }
}

/// Copy the captured frame texture into the staging shared texture.
///
/// Holds the staging texture's keyed mutex on slot 0 for the duration of
/// the copy so readers on the asdf-overlay side don't race. Always
/// releases the mutex, even on error.
pub(crate) fn copy_into_staging(
    frame: &Direct3D11CaptureFrame,
    ctx: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    mutex: &IDXGIKeyedMutex,
) -> Result<()> {
    let surface = frame.Surface().context("frame.Surface")?;
    let access: IDirect3DDxgiInterfaceAccess =
        surface.cast().context("cast to IDirect3DDxgiInterfaceAccess")?;
    let captured: ID3D11Texture2D = unsafe { access.GetInterface() }
        .context("IDirect3DDxgiInterfaceAccess::GetInterface")?;

    unsafe {
        mutex.AcquireSync(0, u32::MAX)?;
    }
    let copy_res = (|| -> Result<()> {
        unsafe { ctx.CopyResource(staging, &captured) };
        Ok(())
    })();
    unsafe {
        let _ = mutex.ReleaseSync(0);
    }
    copy_res
}
