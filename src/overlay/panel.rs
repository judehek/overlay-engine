//! Panel handle + options + shared registry types.
//!
//! Panels live as `<iframe>` children of the embedded shell. Each one
//! is identified by a stable user-supplied id (e.g. `"notifications"`,
//! `"death-replay"`); ids must be unique within a single attached
//! overlay. The host orchestrates panels via [`Overlay::create_panel`]
//! and friends; see the module docs on [`crate::overlay`].

use std::sync::Arc;

use serde::Serialize;

use crate::error::{Error, Result};
use crate::overlay::protocol::Rect;
use crate::overlay::OverlayInner;

/// Construction parameters for a panel.
///
/// The shell creates an iframe with `src = url`, positions it at
/// `bounds`, applies `z_index` for layering, and includes its rect
/// in the engine's hit-region set iff `interactive` is `true`.
#[derive(Debug, Clone, Serialize)]
pub struct PanelOptions {
    /// Stable user-chosen id. Must be unique per overlay; passing a
    /// duplicate to [`Overlay::create_panel`] returns
    /// [`Error::Other`] with a "panel id already exists" message.
    pub id: String,
    /// URL the panel iframe loads. Typically a path under the asset
    /// server's `static_dir` (e.g. `"/notifications.html"`), but any
    /// fully-qualified URL works.
    pub url: String,
    /// Initial bounds in surface-pixel coordinates.
    pub bounds: Rect,
    /// Whether clicks land in the panel (true) or pass through to
    /// the game (false). Update at runtime via
    /// [`Panel::set_interactive`].
    pub interactive: bool,
    /// Stacking order. Higher values render on top. Update at
    /// runtime via [`Panel::set_z_index`].
    pub z_index: i32,
}

impl PanelOptions {
    /// Convenience constructor with sensible defaults
    /// (`interactive: false`, `z_index: 0`).
    pub fn new(id: impl Into<String>, url: impl Into<String>, bounds: Rect) -> Self {
        Self {
            id: id.into(),
            url: url.into(),
            bounds,
            interactive: false,
            z_index: 0,
        }
    }

    /// Builder-style: mark the panel as interactive (clickable).
    pub fn interactive(mut self) -> Self {
        self.interactive = true;
        self
    }

    /// Builder-style: set the z-index.
    pub fn z_index(mut self, z: i32) -> Self {
        self.z_index = z;
        self
    }
}

/// Host-side handle to a panel mounted inside the overlay shell.
///
/// A `Panel` is a thin reference: cloning is cheap, multiple clones
/// can drive the same panel, and dropping a `Panel` does *not* close
/// the underlying iframe — that requires an explicit
/// [`Panel::close`]. The panel also stays mounted across detach if
/// you reuse the same `Overlay` for a future attach (modulo the
/// shell page reloading). Most consumers will call `close()` when
/// they're done.
#[derive(Clone)]
pub struct Panel {
    pub(super) inner: Arc<OverlayInner>,
    pub(super) id: String,
}

impl Panel {
    /// Stable id this panel was created with.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Move and/or resize the panel.
    pub async fn set_bounds(&self, bounds: Rect) -> Result<()> {
        self.inner.set_bounds(&self.id, bounds).await
    }

    /// Toggle whether the panel captures input (`true`) or lets it
    /// pass through to the game (`false`).
    pub async fn set_interactive(&self, interactive: bool) -> Result<()> {
        self.inner.set_interactive(&self.id, interactive).await
    }

    /// Restack the panel.
    pub async fn set_z_index(&self, z_index: i32) -> Result<()> {
        self.inner.set_z_index(&self.id, z_index).await
    }

    /// Send `payload` down to the panel's iframe. The panel receives
    /// it via `host.onMessage(...)` from `@overlay-engine/client`.
    ///
    /// `payload` can be any `serde::Serialize` type; it's encoded as
    /// JSON before being forwarded over `chrome.webview`.
    pub async fn post_message<T: Serialize>(&self, payload: &T) -> Result<()> {
        let value =
            serde_json::to_value(payload).map_err(|e| Error::Other(anyhow::anyhow!(e)))?;
        self.inner.post_panel_message(&self.id, value).await
    }

    /// Close the panel. The shell removes the iframe; the host
    /// forgets the registration. Subsequent operations on this
    /// `Panel` (or any clone) return [`Error::Other`] with a "panel
    /// not found" message.
    pub async fn close(&self) -> Result<()> {
        self.inner.close_panel(&self.id).await
    }
}
