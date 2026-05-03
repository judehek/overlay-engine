//! Helpers for locating overlay DLLs on disk and building an
//! [`OverlayDll`] handoff for `asdf-overlay-client`.
//!
//! The engine takes a *directory* in `OverlayConfig::dll_dir` rather than
//! a per-arch tuple of paths because:
//!
//! 1. The signed DLLs for x64 / x86 / arm64 are typically shipped together
//!    in one `dlls/` folder.
//! 2. The architecture you actually need depends on the target process,
//!    not your own. `asdf-overlay-client` looks at the target's bitness
//!    and picks the right slot at injection time.
//!
//! This module just centralizes the file-name conventions we accept
//! (`asdf_overlay-x64.dll`, `asdf-overlay-x64.dll`, etc.) and turns the
//! resolved paths into the `OverlayDll<'static>` type the inject API
//! wants.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::OverlayDll;

/// File-name candidates for each architecture. First match wins.
const X64_CANDIDATES: &[&str] = &[
    "asdf_overlay-x64.dll",
    "asdf-overlay-x64.dll",
    "asdf_overlay_dll.dll",
    "asdf-overlay-dll.dll",
];
const X86_CANDIDATES: &[&str] = &[
    "asdf_overlay-x86.dll",
    "asdf-overlay-x86.dll",
];
const ARM64_CANDIDATES: &[&str] = &[
    "asdf_overlay-aarch64.dll",
    "asdf-overlay-aarch64.dll",
    "asdf_overlay-arm64.dll",
    "asdf-overlay-arm64.dll",
];

/// Resolve every architecture slot from a single directory.
///
/// At least one architecture must resolve, otherwise the function returns
/// [`Error::DllNotFound`]. Slots without a matching file are returned as
/// `None`; that's how `asdf-overlay-client` represents "I don't have a
/// build for this arch", and it will refuse to inject into a process of
/// that arch later.
pub fn resolve_dir(dll_dir: &Path) -> Result<OverlayDll<'static>> {
    if !dll_dir.is_dir() {
        return Err(Error::InvalidDllDir(dll_dir.to_path_buf()));
    }

    let x64 = first_match(dll_dir, X64_CANDIDATES);
    let x86 = first_match(dll_dir, X86_CANDIDATES);
    let arm64 = first_match(dll_dir, ARM64_CANDIDATES);

    if x64.is_none() && x86.is_none() && arm64.is_none() {
        return Err(Error::DllNotFound {
            dir: dll_dir.to_path_buf(),
            arch: "any",
        });
    }

    Ok(OverlayDll {
        x64: x64.map(leak_pathbuf),
        x86: x86.map(leak_pathbuf),
        arm64: arm64.map(leak_pathbuf),
    })
}

/// Take a single explicit DLL path and use it for every architecture
/// slot. `safe_inject` picks by the *current* process's architecture, so
/// this works for the common case of a same-arch test binary against a
/// same-arch target.
pub fn resolve_explicit(path: &Path) -> Result<OverlayDll<'static>> {
    if !path.exists() {
        return Err(Error::DllNotFound {
            dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            arch: "explicit",
        });
    }
    let leaked = leak_pathbuf(path.to_path_buf());
    Ok(OverlayDll {
        x64: Some(leaked),
        x86: Some(leaked),
        arm64: Some(leaked),
    })
}

fn first_match(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.exists())
}

/// `OverlayDll<'a>` borrows `&'a Path` slots; `asdf-overlay-client`'s
/// `inject_*` API requires `'static`, so the engine leaks the resolved
/// paths for the lifetime of the process. This is fine in practice
/// because (a) DLL paths are tiny and (b) we only resolve them once per
/// engine attach.
fn leak_pathbuf(path: PathBuf) -> &'static Path {
    Box::leak(path.into_boxed_path())
}
