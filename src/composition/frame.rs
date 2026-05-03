//! `FrameUpdate` is the value the WebView2 composition thread emits each
//! time a freshly-captured frame has been copied into the shared-KMT
//! staging texture.
//!
//! The engine's IPC side then forwards the contained handle to the game
//! through `OverlaySurface::update_from_shared`.

/// One produced frame: the OS-level shared handle for the staging texture
/// and its current dimensions.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameUpdate {
    /// OS shared handle (from `IDXGIResource::GetSharedHandle`) of the
    /// staging texture. The receiving side opens this to read the frame.
    pub handle: u32,
    pub width: u32,
    pub height: u32,
}
