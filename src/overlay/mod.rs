//! High-level overlay API.
//!
//! Wraps [`OverlayEngine`] with:
//!
//! * A local HTTP asset server that serves the embedded shell page
//!   plus optional user-supplied panel assets and custom routes.
//! * A panel registry that exposes `create_panel` / `close_panel` /
//!   `panel.post_message` etc. on top of the engine's
//!   `post_web_message` channel.
//! * An [`OverlayEvent`] stream that translates raw shell-side
//!   `chrome.webview.postMessage` traffic into typed
//!   `PanelMessage` / `PanelLoaded` / `PanelRequestClose` events,
//!   and forwards engine-level events (`Detached`, `Warning`, ...)
//!   unchanged.
//!
//! Most consumers never need [`OverlayEngine`] directly; reach for
//! [`Overlay`] instead. Drop down to the engine if you want to bring
//! your own UI host page (e.g. for a custom shell that doesn't use
//! iframes).
//!
//! ```no_run
//! use overlay_engine::overlay::{Overlay, PanelOptions, Rect};
//! # async fn run() -> overlay_engine::Result<()> {
//! let overlay = Overlay::builder()
//!     .dll_dir("./dlls")
//!     .static_dir("./panels-dist")
//!     .build()
//!     .await?;
//!
//! let pid = overlay_engine::process::find_by_name("game.exe").unwrap();
//! let mut events = overlay.attach(pid).await?;
//!
//! let panel = overlay
//!     .create_panel(
//!         PanelOptions::new(
//!             "notifications",
//!             "/notifications.html",
//!             Rect { x: 0, y: 110, w: 300, h: 100 },
//!         )
//!     )
//!     .await?;
//! panel
//!     .post_message(&serde_json::json!({ "type": "show", "text": "hi" }))
//!     .await?;
//!
//! while let Some(event) = events.recv().await {
//!     dbg!(event);
//! }
//! # Ok(())
//! # }
//! ```

mod asset_server;
mod panel;
pub mod protocol;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde::Serialize;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use crate::config::DllSource;
use crate::engine::OverlayEngine;
use crate::error::{Error, Result};
use crate::event::EngineEvent;
use crate::hit_region::HitRegion;

use asset_server::{AssetServerConfig, OverlayAssetServer};
use protocol::{HostToShell, ShellToHost, PROTOCOL_VERSION};

pub use panel::{Panel, PanelOptions};
pub use protocol::Rect;

/// One event delivered on the [`Overlay::attach`] receiver.
///
/// `WebMessage` events from the underlying engine are intercepted
/// and translated into the panel-specific variants below; everything
/// else is forwarded as [`OverlayEvent::Engine`].
#[derive(Debug)]
pub enum OverlayEvent {
    /// Shell page finished loading and is ready to host panels. Any
    /// `create_panel` / `post_message` calls made before this point
    /// were queued and have just been drained to the shell.
    ShellReady,
    /// A panel iframe finished loading and announced itself via
    /// `@overlay-engine/client`.
    PanelLoaded { panel_id: String },
    /// The panel reported an error to the host (e.g. failed to load).
    PanelError { panel_id: String, error: String },
    /// The panel sent `host.postMessage(payload)` from inside its iframe.
    PanelMessage {
        panel_id: String,
        payload: serde_json::Value,
    },
    /// The panel called `host.requestClose()` (e.g. from a close
    /// button). The host SHOULD follow up with `panel.close()` --
    /// the panel does not close itself.
    PanelRequestClose { panel_id: String },
    /// Forwarded engine event (`Detached`, `Warning`, ...). The
    /// engine's `WebMessage` events are NOT re-emitted here.
    Engine(EngineEvent),
}

/// The high-level overlay handle. Cheap to clone (it's an `Arc`
/// internally); panel methods are safe to call from any task.
#[derive(Clone)]
pub struct Overlay {
    inner: Arc<OverlayInner>,
}

/// Builder for [`Overlay`]. Configure DLL source + asset server,
/// then call [`OverlayBuilder::build`] to start the asset server.
pub struct OverlayBuilder {
    dll_source: Option<DllSource>,
    static_dir: Option<PathBuf>,
    extra_router: Option<axum::Router>,
    surface_size: Option<(u32, u32)>,
    surface_layout: Option<crate::config::SurfaceLayout>,
    document_created_scripts: Vec<String>,
    user_data_folder: Option<PathBuf>,
}

impl Default for OverlayBuilder {
    fn default() -> Self {
        Self {
            dll_source: None,
            static_dir: None,
            extra_router: None,
            surface_size: None,
            surface_layout: None,
            document_created_scripts: Vec::new(),
            user_data_folder: None,
        }
    }
}

impl OverlayBuilder {
    /// Filesystem directory containing the asdf-overlay DLL(s).
    /// Equivalent to `OverlayConfig::builder().dll_dir(...)`.
    pub fn dll_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dll_source = Some(DllSource::Dir(dir.into()));
        self
    }

    /// Pre-resolved per-architecture DLL paths. Equivalent to
    /// `OverlayConfig::builder().dll_files(...)`.
    pub fn dll_files(mut self, files: asdf_overlay_client::OverlayDll<'static>) -> Self {
        self.dll_source = Some(DllSource::Files(files));
        self
    }

    /// Filesystem directory whose contents should be served at
    /// `/<path>` by the asset server. This is where panel HTML
    /// bundles live (e.g. produced by `vite build`). Optional --
    /// panels can also be loaded from absolute URLs.
    pub fn static_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.static_dir = Some(dir.into());
        self
    }

    /// Mount additional axum routes alongside the embedded shell
    /// and `static_dir`. Useful for app-specific endpoints (e.g.
    /// Ascent's `/video?token=...`). The router is merged into the
    /// engine's router; routes you define here override anything
    /// the engine reserves under `/__overlay/`.
    pub fn extra_router(mut self, router: axum::Router) -> Self {
        self.extra_router = Some(router);
        self
    }

    /// Override the WebView2 composition surface size in physical
    /// pixels. The surface is stretched to cover the game window, so
    /// pixel-coords on panels are scaled by `game_window / surface`.
    /// Pick a size at or above the game's likely render resolution to
    /// avoid blurring panels (default `800×600`, which is fine for
    /// fixed-pixel HUD-style panels but undersized for full-screen
    /// games at 1080p+).
    pub fn surface_size(mut self, width: u32, height: u32) -> Self {
        self.surface_size = Some((width, height));
        self
    }

    /// Override where the overlay surface is composited inside the
    /// game window. The default ([`SurfaceLayout::top_left`]) draws
    /// the surface at the top-left corner at native size, which is
    /// the right answer when `surface_size` covers the full game
    /// window. For sub-window panels (e.g. a phone-shaped HUD on
    /// the right side) tweak this to anchor / position the surface
    /// where you want it.
    ///
    /// [`SurfaceLayout`]: crate::SurfaceLayout
    /// [`SurfaceLayout::top_left`]: crate::SurfaceLayout::top_left
    pub fn surface_layout(mut self, layout: crate::config::SurfaceLayout) -> Self {
        self.surface_layout = Some(layout);
        self
    }

    /// Append a JavaScript snippet that runs at the start of every
    /// document the WebView2 loads (via
    /// `AddScriptToExecuteOnDocumentCreated`). Call multiple times
    /// to register multiple scripts; they're executed in
    /// registration order. Useful for a host-controlled UI layer
    /// that needs to survive top-frame navigations away from the
    /// embedded shell.
    pub fn document_created_script(mut self, script: impl Into<String>) -> Self {
        self.document_created_scripts.push(script.into());
        self
    }

    /// Override the WebView2 user-data folder for the env this overlay
    /// will create.
    pub fn user_data_folder(mut self, path: impl Into<PathBuf>) -> Self {
        self.user_data_folder = Some(path.into());
        self
    }

    /// Start the asset server and return the configured `Overlay`.
    /// The server keeps running until the returned `Overlay` (and
    /// all its clones) is dropped.
    pub async fn build(self) -> Result<Overlay> {
        let dll_source = self
            .dll_source
            .ok_or_else(|| Error::Other(anyhow::anyhow!("OverlayBuilder: dll_dir or dll_files is required")))?;

        let asset_server = asset_server::start(AssetServerConfig {
            static_dir: self.static_dir,
            extra_router: self.extra_router,
        })
        .await?;

        Ok(Overlay {
            inner: Arc::new(OverlayInner {
                dll_source,
                asset_server,
                state: AsyncMutex::new(OverlayState::Detached),
                shell_ready: AtomicBool::new(false),
                surface_size: StdMutex::new(self.surface_size),
                surface_layout: StdMutex::new(self.surface_layout),
                document_created_scripts: self.document_created_scripts,
                user_data_folder: self.user_data_folder,
            }),
        })
    }
}

impl Overlay {
    /// Start configuring a new overlay. Build the result with
    /// [`OverlayBuilder::build`].
    pub fn builder() -> OverlayBuilder {
        OverlayBuilder::default()
    }

    /// Inject into `pid` and bring up the shell. Returns the
    /// [`OverlayEvent`] receiver for this attach session; the engine
    /// produces a final `Engine(EngineEvent::Detached { .. })` and
    /// closes the channel on detach.
    pub async fn attach(&self, pid: u32) -> Result<mpsc::Receiver<OverlayEvent>> {
        let mut state = self.inner.state.lock().await;
        if matches!(*state, OverlayState::Attached(_)) {
            return Err(Error::Other(anyhow::anyhow!(
                "Overlay::attach called while already attached"
            )));
        }

        let mut config_builder = crate::config::OverlayConfig::builder()
            .dll_source(self.inner.dll_source.clone())
            .url(self.inner.asset_server.shell_url().to_string());
        let surface_size = *self
            .inner
            .surface_size
            .lock()
            .expect("Overlay::surface_size mutex poisoned");
        let surface_layout = *self
            .inner
            .surface_layout
            .lock()
            .expect("Overlay::surface_layout mutex poisoned");
        if let Some((w, h)) = surface_size {
            config_builder = config_builder.surface_size(w, h);
        }
        if let Some(layout) = surface_layout {
            config_builder = config_builder.surface_layout(layout);
        }
        for script in &self.inner.document_created_scripts {
            config_builder = config_builder.document_created_script(script.clone());
        }
        if let Some(path) = &self.inner.user_data_folder {
            config_builder = config_builder.user_data_folder(path.clone());
        }
        let config = config_builder.build()?;

        let (engine, engine_events) = OverlayEngine::attach(pid, config).await?;

        let (event_tx, event_rx) = mpsc::channel::<OverlayEvent>(64);
        // Reset the ready flag so a re-attach against a fresh shell
        // page starts queueing again until shell:ready arrives.
        self.inner.shell_ready.store(false, Ordering::Release);

        *state = OverlayState::Attached(AttachedState {
            engine: engine.clone(),
            panels: HashMap::new(),
            pending_outbound: Vec::new(),
        });
        drop(state);

        let inner = Arc::clone(&self.inner);
        tokio::spawn(driver_task(inner, engine, engine_events, event_tx));

        Ok(event_rx)
    }

    /// Mount a new panel inside the shell. The panel is registered
    /// immediately on the host side; the actual iframe is created
    /// once the shell is ready (calls before `ShellReady` are
    /// queued and replayed in order).
    pub async fn create_panel(&self, options: PanelOptions) -> Result<Panel> {
        let envelope = HostToShell::PanelCreate {
            v: PROTOCOL_VERSION,
            id: options.id.clone(),
            url: options.url.clone(),
            bounds: options.bounds,
            interactive: options.interactive,
            z_index: options.z_index,
        };

        let mut state = self.inner.state.lock().await;
        let attached = match &mut *state {
            OverlayState::Detached => return Err(Error::AlreadyDetached),
            OverlayState::Attached(a) => a,
        };
        if attached.panels.contains_key(&options.id) {
            return Err(Error::Other(anyhow::anyhow!(
                "panel id already exists: {}",
                options.id
            )));
        }
        attached.panels.insert(
            options.id.clone(),
            PanelInfo {
                options: options.clone(),
                loaded: false,
            },
        );

        let engine = attached.engine.clone();
        if self.inner.shell_ready.load(Ordering::Acquire) {
            drop(state);
            send_envelope(&engine, &envelope).await?;
        } else {
            attached.pending_outbound.push(envelope);
        }

        Ok(Panel {
            inner: Arc::clone(&self.inner),
            id: options.id,
        })
    }

    /// Update the WebView2 composition surface size used on the
    /// **next** [`Self::attach`]. The current attach (if any) is not
    /// affected — surface size is baked into the WebView2 + DXGI
    /// stack at attach time, so resizing live would require tearing
    /// down and rebuilding the composition stack. Callers wanting an
    /// immediate effect should follow this with a `detach` /
    /// `attach` cycle.
    pub fn set_surface_size(&self, width: u32, height: u32) {
        *self
            .inner
            .surface_size
            .lock()
            .expect("Overlay::surface_size mutex poisoned") = Some((width, height));
    }

    /// Update where the overlay surface is composited inside the
    /// game window. If currently attached, the new layout is pushed
    /// to the asdf-overlay DLL via IPC immediately (the DLL just
    /// re-reads `SetPosition` / `SetAnchor` / `SetMargin`, no
    /// composition rebuild needed). Either way the new layout is
    /// also remembered for the next attach.
    pub async fn set_surface_layout(&self, layout: crate::config::SurfaceLayout) -> Result<()> {
        *self
            .inner
            .surface_layout
            .lock()
            .expect("Overlay::surface_layout mutex poisoned") = Some(layout);
        let engine = {
            let state = self.inner.state.lock().await;
            match &*state {
                OverlayState::Detached => None,
                OverlayState::Attached(a) => Some(a.engine.clone()),
            }
        };
        if let Some(engine) = engine {
            engine.set_surface_layout(layout).await?;
        }
        Ok(())
    }

    /// Margin-only fast path. Updates the cached layout's margin (so
    /// the next attach uses the new value) and pushes only `SetMargin`
    /// to the DLL when currently attached. Anchor/position must be
    /// already-set; if the cached layout is `None` this falls through
    /// to a full layout push using
    /// [`crate::config::SurfaceLayout::default`] for anchor/position.
    pub async fn set_surface_margin(
        &self,
        margin: (
            crate::PercentLength,
            crate::PercentLength,
            crate::PercentLength,
            crate::PercentLength,
        ),
    ) -> Result<()> {
        // Update the cached layout's margin so a subsequent re-attach
        // doesn't lose this update.
        {
            let mut cached = self
                .inner
                .surface_layout
                .lock()
                .expect("Overlay::surface_layout mutex poisoned");
            if let Some(ref mut layout) = *cached {
                layout.margin = margin;
            } else {
                // No cached layout - we still don't have anchor/position
                // information, so promote this to a full layout push
                // using the engine default for anchor/position.
                let mut layout = crate::config::SurfaceLayout::default();
                layout.margin = margin;
                *cached = Some(layout);
            }
        }
        let engine = {
            let state = self.inner.state.lock().await;
            match &*state {
                OverlayState::Detached => None,
                OverlayState::Attached(a) => Some(a.engine.clone()),
            }
        };
        if let Some(engine) = engine {
            engine.set_surface_margin(margin).await?;
        }
        Ok(())
    }

    /// Tear down the overlay. Detaches the engine, drops the
    /// asset-server connection list (the server itself stops when
    /// the `Overlay` and its clones are all dropped), and emits a
    /// final `Engine(EngineEvent::Detached)` on the event channel.
    pub async fn detach(&self) -> Result<()> {
        let engine = {
            let mut state = self.inner.state.lock().await;
            match std::mem::replace(&mut *state, OverlayState::Detached) {
                OverlayState::Detached => return Err(Error::AlreadyDetached),
                OverlayState::Attached(a) => a.engine,
            }
        };
        engine.detach().await
    }

    /// URL the asset server is bound to, including the path of the
    /// embedded shell page (`http://127.0.0.1:NNNNN/__overlay/shell.html`).
    /// Available immediately after [`OverlayBuilder::build`].
    ///
    /// Use the URL's `origin` (`http://127.0.0.1:NNNNN`) to construct
    /// panel URLs against the same `static_dir` ServeDir mount, e.g.
    /// `format!("{origin}/notifications.html", origin = ...)`.
    pub fn shell_url(&self) -> &str {
        self.inner.asset_server.shell_url()
    }

    /// Convenience: just the origin portion of [`Self::shell_url`]
    /// (`http://127.0.0.1:NNNNN`, no trailing slash). Returns the same
    /// string regardless of whether anything is currently attached.
    pub fn asset_origin(&self) -> String {
        let shell = self.inner.asset_server.shell_url();
        // Shell URL is always `http(s)://host[:port]/__overlay/shell.html`.
        // Strip the path. Works for both ipv4 and ipv6 since axum binds
        // to `127.0.0.1` for us.
        match shell.find("/__overlay") {
            Some(idx) => shell[..idx].to_string(),
            None => shell.trim_end_matches('/').to_string(),
        }
    }

    /// Send a host-level "ping" to the shell. The shell replies with
    /// a `pong` — useful as a liveness check during integration tests.
    pub async fn ping_shell(&self) -> Result<()> {
        let engine = self.engine_clone().await?;
        send_envelope(&engine, &HostToShell::ShellPing { v: PROTOCOL_VERSION }).await
    }

    // ----- Panel-by-id operations -----
    //
    // These are the same operations as the methods on `Panel`, but
    // looked up by id. They exist so the plugin layer (which only
    // sees ids on the JS-side wire) doesn't have to maintain a
    // duplicate panel registry. Direct use from Rust consumers is
    // also fine when you don't already have a `Panel` handle in
    // hand.

    /// Close a panel by id. Equivalent to [`Panel::close`].
    pub async fn close_panel(&self, id: &str) -> Result<()> {
        self.inner.close_panel(id).await
    }

    /// Move and/or resize a panel by id. Equivalent to [`Panel::set_bounds`].
    pub async fn set_panel_bounds(&self, id: &str, bounds: Rect) -> Result<()> {
        self.inner.set_bounds(id, bounds).await
    }

    /// Toggle a panel's input-capture state by id. Equivalent to [`Panel::set_interactive`].
    pub async fn set_panel_interactive(&self, id: &str, interactive: bool) -> Result<()> {
        self.inner.set_interactive(id, interactive).await
    }

    /// Restack a panel by id. Equivalent to [`Panel::set_z_index`].
    pub async fn set_panel_z_index(&self, id: &str, z_index: i32) -> Result<()> {
        self.inner.set_z_index(id, z_index).await
    }

    /// Send a JSON message to a panel by id. Equivalent to [`Panel::post_message`].
    pub async fn post_panel_message(
        &self,
        id: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.inner.post_panel_message(id, payload).await
    }

    async fn engine_clone(&self) -> Result<OverlayEngine> {
        let state = self.inner.state.lock().await;
        match &*state {
            OverlayState::Detached => Err(Error::AlreadyDetached),
            OverlayState::Attached(a) => Ok(a.engine.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// Inner state -- shared between Overlay clones and the driver task.
// ---------------------------------------------------------------------------

pub(super) struct OverlayInner {
    dll_source: DllSource,
    asset_server: OverlayAssetServer,
    state: AsyncMutex<OverlayState>,
    /// Latched once the shell page sends `shell:ready`. Read from
    /// outside the mutex so panel methods can decide between
    /// "send now" vs "queue" without serialising on the state lock.
    shell_ready: AtomicBool,
    /// Override for the WebView2 composition surface size, captured
    /// at builder time and mutable at runtime via
    /// [`Overlay::set_surface_size`]. `None` means the engine falls
    /// back to its default ([`crate::config::DEFAULT_SURFACE_SIZE`]).
    /// Reads on each [`Overlay::attach`] so mutations take effect on
    /// the next attach without rebuilding the [`Overlay`].
    surface_size: StdMutex<Option<(u32, u32)>>,
    /// Override for where the surface is composited inside the game
    /// window. `None` means the engine falls back to top-left at
    /// native size ([`crate::config::SurfaceLayout::top_left`]). Same
    /// mutability semantics as [`Self::surface_size`]; updates are
    /// also pushed to the engine via IPC if currently attached.
    surface_layout: StdMutex<Option<crate::config::SurfaceLayout>>,
    /// JavaScript snippets registered with WebView2 via
    /// `AddScriptToExecuteOnDocumentCreated` on every attach. Same
    /// snapshot is used across attaches; not mutable today (changes
    /// would only matter for the next attach anyway).
    document_created_scripts: Vec<String>,
    /// Optional override for WebView2's user-data folder. See
    /// [`OverlayBuilder::user_data_folder`].
    user_data_folder: Option<PathBuf>,
}

enum OverlayState {
    Detached,
    Attached(AttachedState),
}

struct AttachedState {
    engine: OverlayEngine,
    panels: HashMap<String, PanelInfo>,
    /// Messages requested before `shell:ready`. Drained by the
    /// driver task on receipt of the ready event.
    pending_outbound: Vec<HostToShell>,
}

struct PanelInfo {
    #[allow(dead_code)] // retained for future inspection (bounds, z_index)
    options: PanelOptions,
    #[allow(dead_code)]
    loaded: bool,
}

// ---------------------------------------------------------------------------
// Panel-API impls -- crate-private helpers used by `Panel`'s methods.
// ---------------------------------------------------------------------------

impl OverlayInner {
    pub(super) async fn close_panel(&self, id: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let attached = match &mut *state {
            OverlayState::Detached => return Err(Error::AlreadyDetached),
            OverlayState::Attached(a) => a,
        };
        if attached.panels.remove(id).is_none() {
            return Err(Error::Other(anyhow::anyhow!("panel not found: {id}")));
        }
        let engine = attached.engine.clone();
        let envelope = HostToShell::PanelClose {
            v: PROTOCOL_VERSION,
            id: id.to_string(),
        };
        if self.shell_ready.load(Ordering::Acquire) {
            drop(state);
            send_envelope(&engine, &envelope).await?;
        } else {
            attached.pending_outbound.push(envelope);
        }
        Ok(())
    }

    pub(super) async fn set_bounds(&self, id: &str, bounds: Rect) -> Result<()> {
        let envelope = HostToShell::PanelBounds {
            v: PROTOCOL_VERSION,
            id: id.to_string(),
            bounds,
        };
        self.dispatch_panel_envelope(id, envelope, |info| info.options.bounds = bounds)
            .await
    }

    pub(super) async fn set_interactive(&self, id: &str, interactive: bool) -> Result<()> {
        let envelope = HostToShell::PanelSetInteractive {
            v: PROTOCOL_VERSION,
            id: id.to_string(),
            interactive,
        };
        self.dispatch_panel_envelope(id, envelope, |info| info.options.interactive = interactive)
            .await
    }

    pub(super) async fn set_z_index(&self, id: &str, z_index: i32) -> Result<()> {
        let envelope = HostToShell::PanelSetZIndex {
            v: PROTOCOL_VERSION,
            id: id.to_string(),
            z_index,
        };
        self.dispatch_panel_envelope(id, envelope, |info| info.options.z_index = z_index)
            .await
    }

    pub(super) async fn post_panel_message(
        &self,
        id: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        let envelope = HostToShell::PanelMessage {
            v: PROTOCOL_VERSION,
            id: id.to_string(),
            payload,
        };
        self.dispatch_panel_envelope(id, envelope, |_| {}).await
    }

    /// Verify the panel still exists, mutate its mirrored host-side
    /// options if applicable, then send (or queue) the envelope.
    async fn dispatch_panel_envelope(
        &self,
        id: &str,
        envelope: HostToShell,
        mutate: impl FnOnce(&mut PanelInfo),
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        let attached = match &mut *state {
            OverlayState::Detached => return Err(Error::AlreadyDetached),
            OverlayState::Attached(a) => a,
        };
        let info = attached
            .panels
            .get_mut(id)
            .ok_or_else(|| Error::Other(anyhow::anyhow!("panel not found: {id}")))?;
        mutate(info);
        let engine = attached.engine.clone();
        if self.shell_ready.load(Ordering::Acquire) {
            drop(state);
            send_envelope(&engine, &envelope).await
        } else {
            attached.pending_outbound.push(envelope);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Driver task -- consumes engine events, routes shell traffic.
// ---------------------------------------------------------------------------

async fn driver_task(
    inner: Arc<OverlayInner>,
    engine: OverlayEngine,
    mut engine_events: mpsc::Receiver<EngineEvent>,
    out_tx: mpsc::Sender<OverlayEvent>,
) {
    while let Some(event) = engine_events.recv().await {
        match event {
            EngineEvent::WebMessage(text) => {
                if let Err(err) = handle_shell_message(&inner, &engine, &out_tx, &text).await {
                    eprintln!("[overlay] shell message handling failed: {err:?}");
                }
            }
            EngineEvent::Detached { .. } => {
                let _ = out_tx.send(OverlayEvent::Engine(event)).await;
                // Reset state so a future attach starts clean.
                let mut state = inner.state.lock().await;
                *state = OverlayState::Detached;
                inner.shell_ready.store(false, Ordering::Release);
                break;
            }
            other => {
                let _ = out_tx.send(OverlayEvent::Engine(other)).await;
            }
        }
    }
}

async fn handle_shell_message(
    inner: &OverlayInner,
    engine: &OverlayEngine,
    out_tx: &mpsc::Sender<OverlayEvent>,
    text: &str,
) -> Result<()> {
    let msg: ShellToHost = serde_json::from_str(text)
        .map_err(|e| Error::Other(anyhow::anyhow!("shell -> host parse: {e}")))?;
    if msg.v() != PROTOCOL_VERSION {
        return Err(Error::Other(anyhow::anyhow!(
            "shell -> host protocol version mismatch (got {}, expected {})",
            msg.v(),
            PROTOCOL_VERSION
        )));
    }

    match msg {
        ShellToHost::ShellReady { .. } => {
            inner.shell_ready.store(true, Ordering::Release);
            // Drain any envelopes queued before the shell came up.
            let pending = {
                let mut state = inner.state.lock().await;
                if let OverlayState::Attached(a) = &mut *state {
                    std::mem::take(&mut a.pending_outbound)
                } else {
                    Vec::new()
                }
            };
            for envelope in pending {
                if let Err(err) = send_envelope(engine, &envelope).await {
                    eprintln!("[overlay] failed to flush queued envelope: {err:?}");
                }
            }
            let _ = out_tx.send(OverlayEvent::ShellReady).await;
        }
        ShellToHost::ShellHitRegions { regions, .. } => {
            let regions_eng: Vec<HitRegion> = regions
                .into_iter()
                // Negative widths/heights would wrap to huge values via
                // `as u32`; treat them as empty regions instead.
                .filter(|r| r.w > 0 && r.h > 0)
                .map(|r| HitRegion {
                    x: r.x,
                    y: r.y,
                    width: r.w as u32,
                    height: r.h as u32,
                })
                .collect();
            if let Err(err) = engine.set_hit_regions(&regions_eng).await {
                eprintln!("[overlay] set_hit_regions from shell failed: {err:?}");
            }
        }
        ShellToHost::PanelLoaded { id, .. } => {
            {
                let mut state = inner.state.lock().await;
                if let OverlayState::Attached(a) = &mut *state {
                    if let Some(info) = a.panels.get_mut(&id) {
                        info.loaded = true;
                    }
                }
            }
            let _ = out_tx
                .send(OverlayEvent::PanelLoaded { panel_id: id })
                .await;
        }
        ShellToHost::PanelError { id, error, .. } => {
            let _ = out_tx
                .send(OverlayEvent::PanelError {
                    panel_id: id,
                    error,
                })
                .await;
        }
        ShellToHost::PanelRequestClose { id, .. } => {
            let _ = out_tx
                .send(OverlayEvent::PanelRequestClose { panel_id: id })
                .await;
        }
        ShellToHost::PanelMessageUp { id, payload, .. } => {
            let _ = out_tx
                .send(OverlayEvent::PanelMessage {
                    panel_id: id,
                    payload,
                })
                .await;
        }
        ShellToHost::ShellPong { .. } => {
            // Currently no host-side observable; future: wake a oneshot for ping_shell.
        }
    }
    Ok(())
}

async fn send_envelope<T: Serialize>(engine: &OverlayEngine, envelope: &T) -> Result<()> {
    let text = serde_json::to_string(envelope)
        .map_err(|e| Error::Other(anyhow::anyhow!("envelope serialize: {e}")))?;
    engine.post_web_message(text).await
}

