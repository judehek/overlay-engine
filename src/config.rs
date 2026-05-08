//! `OverlayConfig` and its builder.
//!
//! `OverlayConfig` is the immutable bundle of "starting state" the engine
//! reads at attach time. Anything that can change while attached (URL,
//! hit regions) is exposed as a runtime method on
//! [`crate::OverlayEngine`] instead of going on this struct.

use std::path::{Path, PathBuf};

use asdf_overlay_client::common::size::PercentLength;

use crate::error::{Error, Result};
use crate::{InjectStrategy, OverlayDll};

/// Default render-surface size used when the caller doesn't override it. The
/// engine recreates the WebView2 surface at this size on attach.
pub const DEFAULT_SURFACE_SIZE: (u32, u32) = (800, 600);

/// Where the overlay surface gets composited inside the target game
/// window. Mirrors `asdf-overlay`'s layout primitives directly:
///
/// * `position`  — point inside the game window, relative to its
///   client area, that the surface anchor maps onto.
/// * `anchor`    — point inside the surface texture (relative to the
///   surface's own size) that lands on `position`.
/// * `margin`    — `(top, right, bottom, left)` padding folded into
///   the layout calculation.
///
/// All four are [`PercentLength`]s so callers can mix percent-of-
/// container with absolute pixels in the same expression.
///
/// The default layout (everything zero) anchors the top-left of the
/// surface at the top-left of the game window, which is what every
/// existing call site assumed before this knob was exposed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceLayout {
    pub position: (PercentLength, PercentLength),
    pub anchor: (PercentLength, PercentLength),
    pub margin: (PercentLength, PercentLength, PercentLength, PercentLength),
}

impl SurfaceLayout {
    /// All-zero layout: surface drawn at the top-left of the game
    /// window at native size. Matches the engine's pre-`SurfaceLayout`
    /// behaviour.
    pub const fn top_left() -> Self {
        Self {
            position: (PercentLength::ZERO, PercentLength::ZERO),
            anchor: (PercentLength::ZERO, PercentLength::ZERO),
            margin: (
                PercentLength::ZERO,
                PercentLength::ZERO,
                PercentLength::ZERO,
                PercentLength::ZERO,
            ),
        }
    }
}

impl Default for SurfaceLayout {
    fn default() -> Self {
        Self::top_left()
    }
}

/// How the engine should locate the `asdf-overlay` DLL to inject.
///
/// Most callers want [`DllSource::Dir`] -- point the engine at a folder of
/// signed DLLs and let it pick the right architecture for the target.
/// [`DllSource::Files`] is for advanced cases (custom DLL names, in-memory
/// test rigs, etc.) where the caller has already resolved the per-arch
/// paths themselves.
#[derive(Debug, Clone)]
pub enum DllSource {
    /// Resolve from a directory of conventionally-named DLLs at attach
    /// time. See [`crate::dll::resolve_dir`] for the file-name candidates.
    Dir(PathBuf),
    /// Use these per-architecture paths directly.
    Files(OverlayDll<'static>),
}

/// Immutable engine configuration. Build with [`OverlayConfig::builder`].
#[derive(Debug, Clone)]
pub struct OverlayConfig {
    /// Where to find the overlay DLL. Required.
    pub(crate) dll_source: DllSource,

    /// Initial URL to navigate the WebView2 to once the composition stack
    /// is up. Required for the "interactive UI" use case; if `None`, the
    /// surface stays blank until the consumer calls
    /// [`crate::OverlayEngine::navigate`].
    pub(crate) initial_url: Option<String>,

    /// Overlay surface size in physical pixels.
    pub(crate) surface_size: (u32, u32),

    /// Where in the game window the overlay surface gets drawn. The
    /// engine sends this as `SetPosition` / `SetAnchor` / `SetMargin`
    /// to the asdf-overlay DLL during attach.
    pub(crate) surface_layout: SurfaceLayout,

    /// Which DLL injection strategy to use. Defaults to
    /// [`InjectStrategy::WindowsHook`] -- the only strategy known to work
    /// with kernel anti-cheats (Vanguard) when the DLL is properly signed.
    pub(crate) inject_strategy: InjectStrategy,

    /// Hard upper bound on how long we wait for the safe-injection
    /// `SetWindowsHookEx` path to confirm the DLL has loaded into the
    /// target.
    pub(crate) attach_timeout: std::time::Duration,

    /// JavaScript snippets injected via
    /// `AddScriptToExecuteOnDocumentCreated`. Each script runs at the
    /// start of every document the WebView2 loads, before the page's
    /// own scripts. Useful for a host-controlled UI layer (e.g. a
    /// draggable frame) that needs to survive top-frame navigations
    /// away from the embedded shell page.
    pub(crate) document_created_scripts: Vec<String>,

    /// Filesystem path WebView2 uses for its user-data folder
    /// (cookies, local storage, cache). `None` lets WebView2 pick
    /// `<exe>.WebView2/EBWebView/` next to the host process.
    pub(crate) user_data_folder: Option<PathBuf>,
}

impl OverlayConfig {
    /// Start a fluent builder.
    pub fn builder() -> OverlayConfigBuilder {
        OverlayConfigBuilder::default()
    }

    pub fn dll_source(&self) -> &DllSource {
        &self.dll_source
    }

    pub fn initial_url(&self) -> Option<&str> {
        self.initial_url.as_deref()
    }

    pub fn surface_size(&self) -> (u32, u32) {
        self.surface_size
    }

    pub fn surface_layout(&self) -> SurfaceLayout {
        self.surface_layout
    }

    pub fn inject_strategy(&self) -> InjectStrategy {
        self.inject_strategy
    }

    pub fn attach_timeout(&self) -> std::time::Duration {
        self.attach_timeout
    }

    pub fn document_created_scripts(&self) -> &[String] {
        &self.document_created_scripts
    }

    pub fn user_data_folder(&self) -> Option<&Path> {
        self.user_data_folder.as_deref()
    }
}

/// Builder for [`OverlayConfig`]. All fields default to sensible values
/// except the DLL source, which has no good default.
#[derive(Debug, Clone, Default)]
pub struct OverlayConfigBuilder {
    dll_source: Option<DllSource>,
    initial_url: Option<String>,
    surface_size: Option<(u32, u32)>,
    surface_layout: Option<SurfaceLayout>,
    inject_strategy: Option<InjectStrategy>,
    attach_timeout: Option<std::time::Duration>,
    document_created_scripts: Vec<String>,
    user_data_folder: Option<PathBuf>,
}

impl OverlayConfigBuilder {
    /// Point the engine at a directory of conventionally-named DLLs.
    ///
    /// At least one of [`Self::dll_dir`] or [`Self::dll_files`] must be
    /// called. Calling either replaces any prior DLL source.
    pub fn dll_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.dll_source = Some(DllSource::Dir(dir.as_ref().to_path_buf()));
        self
    }

    /// Provide pre-resolved per-architecture DLL paths.
    ///
    /// Use this when the caller already knows the exact paths (e.g. a
    /// custom file-name layout) and wants to skip the engine's
    /// directory-search step. See [`crate::dll::resolve_explicit`] /
    /// [`crate::dll::resolve_dir`] for helpers that mint
    /// [`OverlayDll<'static>`] from runtime paths.
    pub fn dll_files(mut self, files: OverlayDll<'static>) -> Self {
        self.dll_source = Some(DllSource::Files(files));
        self
    }

    /// Set an already-built [`DllSource`] directly. Equivalent to
    /// `dll_dir` / `dll_files` but takes the enum verbatim, useful
    /// when forwarding a value from a higher-level builder
    /// (`OverlayBuilder`) that's already validated the source.
    pub fn dll_source(mut self, source: DllSource) -> Self {
        self.dll_source = Some(source);
        self
    }

    /// Optional: page to navigate the WebView2 to on attach.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.initial_url = Some(url.into());
        self
    }

    /// Optional: override the default render surface size.
    pub fn surface_size(mut self, width: u32, height: u32) -> Self {
        self.surface_size = Some((width, height));
        self
    }

    /// Optional: override the default top-left surface layout. Use
    /// this to anchor a smaller-than-full-window surface in any
    /// corner / center of the game window. See [`SurfaceLayout`] for
    /// the exact composition rules (they mirror asdf-overlay's).
    pub fn surface_layout(mut self, layout: SurfaceLayout) -> Self {
        self.surface_layout = Some(layout);
        self
    }

    /// Optional: override the injection strategy. The default is the only
    /// one known to be safe against modern kernel anti-cheats; only change
    /// this if you know what you're doing.
    pub fn inject_strategy(mut self, strategy: InjectStrategy) -> Self {
        self.inject_strategy = Some(strategy);
        self
    }

    /// Optional: how long to wait on the safe-inject path before giving up.
    pub fn attach_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.attach_timeout = Some(timeout);
        self
    }

    /// Append a JavaScript snippet that runs at the start of **every**
    /// document the WebView2 loads (via
    /// `AddScriptToExecuteOnDocumentCreated`). Call multiple times to
    /// register multiple scripts; they're executed in registration
    /// order. Useful for a host-controlled UI layer that needs to
    /// survive top-frame navigations away from the embedded shell.
    pub fn document_created_script(mut self, script: impl Into<String>) -> Self {
        self.document_created_scripts.push(script.into());
        self
    }

    /// Override the WebView2 user-data folder for the env this engine
    /// will create.
    pub fn user_data_folder(mut self, path: impl Into<PathBuf>) -> Self {
        self.user_data_folder = Some(path.into());
        self
    }

    /// Validate and freeze.
    pub fn build(self) -> Result<OverlayConfig> {
        let dll_source = self
            .dll_source
            .ok_or(Error::MissingConfig("dll_dir or dll_files"))?;

        if let DllSource::Dir(ref dir) = dll_source {
            if !dir.is_dir() {
                return Err(Error::InvalidDllDir(dir.clone()));
            }
        }

        if let Some(ref url) = self.initial_url {
            if url.trim().is_empty() {
                return Err(Error::InvalidUrl(url.clone()));
            }
        }

        Ok(OverlayConfig {
            dll_source,
            initial_url: self.initial_url,
            surface_size: self.surface_size.unwrap_or(DEFAULT_SURFACE_SIZE),
            surface_layout: self.surface_layout.unwrap_or_default(),
            inject_strategy: self.inject_strategy.unwrap_or(InjectStrategy::WindowsHook),
            attach_timeout: self
                .attach_timeout
                .unwrap_or_else(|| std::time::Duration::from_secs(15)),
            document_created_scripts: self.document_created_scripts,
            user_data_folder: self.user_data_folder,
        })
    }
}
