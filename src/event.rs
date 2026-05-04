//! Engine event stream.
//!
//! `OverlayEngine` exposes a `tokio::sync::mpsc::Receiver<EngineEvent>` so
//! callers can react to overlay state changes without needing to poll. The
//! events are intentionally lightweight; payloads with a lot of detail go
//! through dedicated APIs rather than this enum.

/// Outcome of a navigation attempt in the WebView.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationStatus {
    /// `NavigationCompleted` fired with `IsSuccess = true`.
    Success,
    /// `NavigationCompleted` fired with `IsSuccess = false`. WebView2
    /// surfaces a status code we don't currently forward.
    Failed,
    /// The navigation was cancelled before completion (e.g. a new
    /// `navigate` call replaced it).
    Cancelled,
}

/// All async signals an attached overlay engine can emit.
///
/// Variants are non-exhaustive so we can grow the enum without breaking
/// downstream `match` blocks. Always include a `_ => {}` arm.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EngineEvent {
    /// The DLL is loaded into the target process and the IPC channel is
    /// up. The engine has chosen the game window we'll render onto;
    /// `surface_size` is the WebView2 render-target size we will produce
    /// frames at.
    Attached {
        target_window_id: u32,
        game_window_size: (u32, u32),
        surface_size: (u32, u32),
    },

    /// A previously-attached engine is shutting down (either explicitly
    /// via `detach`, or because the IPC closed).
    Detached { reason: DetachReason },

    /// The game's window resized. This is informational; the engine
    /// doesn't currently rebuild the WebView2 surface in response (the
    /// staging texture stays at its initial `surface_size`).
    GameWindowResized { width: u32, height: u32 },

    /// A navigation kicked off via [`crate::OverlayEngine::navigate`] (or
    /// the initial `OverlayConfig::initial_url`) reached a final state.
    Navigation { url: String, status: NavigationStatus },

    /// A non-fatal error happened on the engine's background side
    /// (composition, IPC, input). The engine keeps running. Fatal
    /// problems show up as `Detached { reason: Error(_) }` instead.
    Warning(String),

    /// A `window.chrome.webview.postMessage(text)` call from the page
    /// inside the WebView2 reached the engine. `text` is whatever the
    /// page sent — JSON-encoded by convention but the engine doesn't
    /// parse it.
    ///
    /// Pair with [`crate::OverlayEngine::post_web_message`] for the
    /// host -> page direction.
    WebMessage(String),
}

/// Why an attached engine stopped.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DetachReason {
    /// `OverlayEngine::detach` was called.
    Requested,
    /// asdf-overlay's IPC connection closed cleanly (e.g. the game
    /// process exited).
    IpcClosed,
    /// A fatal background error.
    Error(String),
}
