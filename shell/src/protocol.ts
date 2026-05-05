// Wire protocol between the host (Rust) and the embedded shell, and
// between the shell and panel iframes.
//
// Every JSON message carries a `v` (protocol version) and a `type`
// discriminator. Keep the version bumped + the Rust mirror in
// `src/overlay/protocol.rs` updated in lockstep.

export const PROTOCOL_VERSION = 1;

/** Pixel-space rectangle in the WebView2 surface coordinate system. */
export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

// ----- Host -> Shell -----

export type HostToShell =
  | { v: number; type: "panel:create"; id: string; url: string; bounds: Rect; interactive: boolean; zIndex: number }
  | { v: number; type: "panel:close"; id: string }
  | { v: number; type: "panel:bounds"; id: string; bounds: Rect }
  | { v: number; type: "panel:set-interactive"; id: string; interactive: boolean }
  | { v: number; type: "panel:set-z-index"; id: string; zIndex: number }
  | { v: number; type: "panel:message"; id: string; payload: unknown }
  | { v: number; type: "shell:ping" };

// ----- Shell -> Host -----

export type ShellToHost =
  | { v: number; type: "shell:ready" }
  | { v: number; type: "shell:hit-regions"; regions: Rect[] }
  | { v: number; type: "panel:loaded"; id: string }
  | { v: number; type: "panel:error"; id: string; error: string }
  | { v: number; type: "panel:request-close"; id: string }
  | { v: number; type: "panel:message-up"; id: string; payload: unknown }
  | { v: number; type: "shell:pong" };

// ----- Panel iframe -> Shell (via window.postMessage to parent) -----

export type PanelToShell =
  | { v: number; type: "panel-client:ready" }
  | { v: number; type: "panel-client:message"; payload: unknown }
  | { v: number; type: "panel-client:request-close" }
  | { v: number; type: "panel-client:set-hit-regions"; regions: Rect[] | null };

// ----- Shell -> Panel iframe (via iframe.contentWindow.postMessage) -----

export type ShellToPanel =
  | { v: number; type: "shell-client:message"; payload: unknown };
