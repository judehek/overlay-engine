//! JSON wire protocol between the host (this Rust crate) and the
//! embedded shell page. Mirrors `shell/src/protocol.ts`; bump
//! [`PROTOCOL_VERSION`] (and the TS `PROTOCOL_VERSION`) in lockstep
//! when adding or renaming variants.
//!
//! Messages are serialized as JSON strings and sent over WebView2's
//! `chrome.webview.postMessage` channel in either direction.

use serde::{Deserialize, Serialize};

/// Current protocol version. Sender must include `v` on every
/// payload; receiver MUST drop messages with a different `v` rather
/// than try to interpret them. Bumping this is a breaking change for
/// the shell <-> host pair; we don't promise backwards compatibility
/// across versions because the shell ships embedded.
pub const PROTOCOL_VERSION: u32 = 1;

/// Pixel-space rectangle in the WebView2 surface coordinate system
/// (i.e. game-window client-area pixels with the engine anchored at
/// 0,0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Messages the host sends down to the shell.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum HostToShell {
    #[serde(rename = "panel:create")]
    PanelCreate {
        v: u32,
        id: String,
        url: String,
        bounds: Rect,
        interactive: bool,
        #[serde(rename = "zIndex")]
        z_index: i32,
    },
    #[serde(rename = "panel:close")]
    PanelClose { v: u32, id: String },
    #[serde(rename = "panel:bounds")]
    PanelBounds { v: u32, id: String, bounds: Rect },
    #[serde(rename = "panel:set-interactive")]
    PanelSetInteractive {
        v: u32,
        id: String,
        interactive: bool,
    },
    #[serde(rename = "panel:set-z-index")]
    PanelSetZIndex {
        v: u32,
        id: String,
        #[serde(rename = "zIndex")]
        z_index: i32,
    },
    #[serde(rename = "panel:message")]
    PanelMessage {
        v: u32,
        id: String,
        payload: serde_json::Value,
    },
    #[serde(rename = "shell:ping")]
    ShellPing { v: u32 },
}

/// Messages the shell sends up to the host. Parsed from incoming
/// `EngineEvent::WebMessage` strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ShellToHost {
    #[serde(rename = "shell:ready")]
    ShellReady { v: u32 },
    #[serde(rename = "shell:hit-regions")]
    ShellHitRegions { v: u32, regions: Vec<Rect> },
    #[serde(rename = "panel:loaded")]
    PanelLoaded { v: u32, id: String },
    #[serde(rename = "panel:error")]
    PanelError { v: u32, id: String, error: String },
    #[serde(rename = "panel:request-close")]
    PanelRequestClose { v: u32, id: String },
    #[serde(rename = "panel:message-up")]
    PanelMessageUp {
        v: u32,
        id: String,
        payload: serde_json::Value,
    },
    #[serde(rename = "shell:pong")]
    ShellPong { v: u32 },
}

impl ShellToHost {
    /// Protocol-version field on every variant.
    pub fn v(&self) -> u32 {
        match self {
            Self::ShellReady { v }
            | Self::ShellHitRegions { v, .. }
            | Self::PanelLoaded { v, .. }
            | Self::PanelError { v, .. }
            | Self::PanelRequestClose { v, .. }
            | Self::PanelMessageUp { v, .. }
            | Self::ShellPong { v } => *v,
        }
    }
}
