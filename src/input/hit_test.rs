//! Engine-side hit-testing for cursor events against `&[HitRegion]`.
//!
//! When the engine has registered hit regions, the IPC loop runs every
//! incoming `CursorInput` through [`cursor_in_any_region`] before
//! deciding whether to forward it to the WebView2 thread. Events that
//! don't intersect any region pass through to the game; events that do
//! get forwarded so the page can react to them.
//!
//! The single-rect "the entire overlay is interactive" mode is just a
//! degenerate case of this with one region covering the surface.

use asdf_overlay_client::event::input::CursorInput;

use crate::hit_region::{any_contains, HitRegion};

/// True if `cursor.client` (overlay-local pixel coordinates set by the
/// DLL) lies inside any of `regions`.
///
/// `HitRegion::contains` already rejects negative points relative to a
/// non-negative region origin, so we don't have to special-case the
/// possibly-negative `client.x` / `client.y` values here.
#[inline]
pub(crate) fn cursor_in_any_region(regions: &[HitRegion], cursor: &CursorInput) -> bool {
    any_contains(regions, cursor.client.x, cursor.client.y)
}
