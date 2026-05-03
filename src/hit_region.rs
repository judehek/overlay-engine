//! `HitRegion` -- rectangular sub-regions of the overlay surface that
//! consume cursor input.
//!
//! Coordinates are in **WebView2-local pixels** (origin is the top-left of
//! the overlay's render target). They are *not* game-window-client coords;
//! the engine internally translates them to the game's coordinate space
//! based on where the overlay is anchored.
//!
//! Multiple regions are supported so a UI like "always-on toast in the
//! top-right + a temporary modal in the center" can be expressed naturally:
//! both regions consume input, and everywhere else falls through to the
//! game.

/// A rectangular sub-region of the overlay surface that consumes cursor
/// input while the overlay is in hover mode (the goverlay-style "auto
/// mouse check" behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitRegion {
    /// Top-left X in WebView2-local pixels.
    pub x: i32,
    /// Top-left Y in WebView2-local pixels.
    pub y: i32,
    /// Width in pixels. Zero-size regions are ignored.
    pub width: u32,
    /// Height in pixels. Zero-size regions are ignored.
    pub height: u32,
}

impl HitRegion {
    /// `(x, y, width, height)` shorthand.
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// A region covering the entire surface of the given size.
    pub const fn full(size: (u32, u32)) -> Self {
        Self { x: 0, y: 0, width: size.0, height: size.1 }
    }

    /// Is the WebView2-local point `(px, py)` strictly inside this region?
    /// Empty regions never contain anything.
    #[inline]
    pub fn contains(&self, px: i32, py: i32) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        let right = self.x + self.width as i32;
        let bottom = self.y + self.height as i32;
        px >= self.x && px < right && py >= self.y && py < bottom
    }
}

/// Convenience: `[HitRegion]::contains` -- does *any* region in the slice
/// contain the point?
#[inline]
pub fn any_contains(regions: &[HitRegion], px: i32, py: i32) -> bool {
    regions.iter().any(|r| r.contains(px, py))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_region_contains_nothing() {
        let r = HitRegion::new(0, 0, 0, 0);
        assert!(!r.contains(0, 0));
        assert!(!r.contains(-1, -1));
    }

    #[test]
    fn contains_is_inclusive_top_left_exclusive_bottom_right() {
        let r = HitRegion::new(10, 20, 100, 50);
        assert!(r.contains(10, 20));
        assert!(r.contains(109, 69));
        assert!(!r.contains(110, 70));
        assert!(!r.contains(9, 20));
        assert!(!r.contains(10, 19));
    }

    #[test]
    fn any_contains_picks_first_hit() {
        let regions = [
            HitRegion::new(0, 0, 10, 10),
            HitRegion::new(100, 100, 10, 10),
        ];
        assert!(any_contains(&regions, 5, 5));
        assert!(any_contains(&regions, 105, 105));
        assert!(!any_contains(&regions, 50, 50));
    }
}
