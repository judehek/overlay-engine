//! WebView2 + DComp + Graphics.Capture composition pipeline.
//!
//! This module owns everything that runs on the dedicated
//! "webview2-composition" STA thread:
//!
//! * [`d3d`] — D3D11 device creation, the shared-KMT staging texture, and
//!   the per-frame copy from the captured texture into the staging.
//! * [`host`] — a hidden Win32 window that WebView2 parents itself to.
//! * [`webview`] — async creation of `ICoreWebView2Environment` and
//!   `ICoreWebView2CompositionController`.
//! * [`capture`] — `Direct3D11CaptureFramePool` plumbing and the
//!   `FrameArrived` callback that drives the staging copy.
//! * [`thread`] — the orchestrator: spawn the STA thread, wire everything
//!   together, run the Win32 message loop, and join on shutdown.
//! * [`frame`] — the `FrameUpdate` value the thread emits each time a new
//!   shared handle is ready for the engine's tokio side to push to the
//!   game.

pub(crate) mod capture;
pub(crate) mod d3d;
pub(crate) mod frame;
pub(crate) mod host;
pub(crate) mod thread;
pub(crate) mod webview;

pub(crate) use frame::FrameUpdate;
pub(crate) use thread::{spawn_web_thread, WebThreadHandle, WebThreadParams};
