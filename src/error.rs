//! Engine-level error type.
//!
//! Most errors bubble up from `asdf-overlay-client`, `windows`, or
//! `webview2-com` via the catch-all `Other` variant; the named variants are
//! reserved for the cases callers reasonably want to branch on (target not
//! found, DLL missing, etc).

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// The configured target process could not be found at attach time.
    #[error("target process not found: {0}")]
    TargetNotFound(String),

    /// No overlay DLL exists for the architecture we want to inject (and no
    /// explicit per-arch path was provided either).
    #[error("no overlay DLL found in '{dir}' (looked for arch={arch})")]
    DllNotFound { dir: PathBuf, arch: &'static str },

    /// The configured DLL directory itself isn't usable (doesn't exist,
    /// not a directory, no permission, etc.).
    #[error("invalid DLL directory: {0}")]
    InvalidDllDir(PathBuf),

    /// The user supplied a `url` that we couldn't accept (empty, not a
    /// well-formed URL, ...).
    #[error("invalid initial URL: {0}")]
    InvalidUrl(String),

    /// Caller forgot to provide a required builder field.
    #[error("missing required config field: {0}")]
    MissingConfig(&'static str),

    /// asdf-overlay-side injection failed (signing, anti-cheat blocked,
    /// arch mismatch, ...).
    #[error("overlay injection failed")]
    Injection(#[source] anyhow::Error),

    /// asdf-overlay IPC closed before we saw a usable game window.
    #[error("overlay IPC connection closed before a usable window was found")]
    IpcClosedEarly,

    /// Generic IPC-side failure from `asdf-overlay-client` after attach.
    #[error("overlay IPC error")]
    Ipc(#[source] anyhow::Error),

    /// COM / WinRT / WebView2 init failure when bringing up the
    /// composition stack.
    #[error("composition pipeline failed to initialize")]
    Composition(#[source] anyhow::Error),

    /// `OverlayEngine::detach` was called more than once or after the
    /// engine had already shut itself down.
    #[error("overlay engine is already detached")]
    AlreadyDetached,

    /// Catch-all for anyhow-shaped errors we don't have a specific variant
    /// for yet.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
