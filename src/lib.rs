//! # overlay-engine
//!
//! Embed a WebView2-rendered overlay UI into a running game window. Wraps
//! [`asdf-overlay`](https://github.com/judehek/asdf-overlay) for the
//! injected-DLL side (rendering hook + IPC), drives WebView2 in composition
//! mode for the UI layer, composes the result into a shared D3D11 texture
//! handed back to the DLL, and routes input cleanly between the game and the
//! overlay.
//!
//! ## What the engine owns
//!
//! * The injection lifecycle: pick a target process, ship the signed DLL into
//!   it via the safe `SetWindowsHookEx` path, hold the hook open until detach.
//! * The IPC connection to the in-game DLL.
//! * A WebView2 composition stack (D3D11 device, DComp visual tree,
//!   `Graphics.Capture` framepool) that captures the WebView's output into a
//!   shared D3D11 texture.
//! * Routing pointer events from the game to the WebView, with hit-test
//!   gating so the game stays interactive outside the overlay's clickable
//!   regions.
//!
//! ## What it deliberately does not own
//!
//! * The overlay UI itself -- the consumer points the engine at a URL (or
//!   a local `file://` of a Tauri / Vite build) and the WebView2 takes it
//!   from there.
//! * Multi-window compositing. There is exactly one WebView2 surface per
//!   attached game window. Multiple "panels" (notifications, modals, ...)
//!   are expected to be DOM elements inside that single WebView2.
//!
//! ## Quick start
//!
//! ```no_run
//! use overlay_engine::{OverlayEngine, OverlayConfig};
//!
//! # async fn run() -> overlay_engine::Result<()> {
//! let pid = overlay_engine::process::find_by_name("League of Legends.exe")
//!     .ok_or_else(|| overlay_engine::Error::TargetNotFound("League of Legends.exe".into()))?;
//!
//! let config = OverlayConfig::builder()
//!     .dll_dir("./dlls")
//!     .url("https://my-overlay.app/")
//!     .build()?;
//!
//! let (engine, mut events) = OverlayEngine::attach(pid, config).await?;
//!
//! while let Some(event) = events.recv().await {
//!     println!("engine event: {event:?}");
//! }
//!
//! engine.detach().await?;
//! # Ok(())
//! # }
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod config;
pub mod dll;
pub mod error;
pub mod event;
pub mod hit_region;
pub mod overlay;
pub mod process;

mod engine;

#[cfg(target_os = "windows")]
mod composition;

#[cfg(target_os = "windows")]
mod input;

#[cfg(target_os = "windows")]
mod ipc;

pub use config::{DllSource, OverlayConfig, OverlayConfigBuilder, SurfaceLayout};
pub use engine::OverlayEngine;
pub use error::{Error, Result};
pub use event::{DetachReason, EngineEvent, NavigationStatus};
pub use hit_region::HitRegion;
pub use overlay::{Overlay, OverlayBuilder, OverlayEvent, Panel, PanelOptions, Rect};

// Re-export the asdf-overlay types we expose in our public surface so
// consumers don't have to add asdf-overlay-client as a direct dep.
pub use asdf_overlay_client::common::size::PercentLength;
pub use asdf_overlay_client::{InjectStrategy, OverlayDll};
