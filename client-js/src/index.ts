/**
 * `@overlay-engine/client` — SDK for code running inside an
 * overlay-engine panel iframe.
 *
 * The shell hosts panel iframes inside a WebView2 composition surface
 * rendered over a game window. This package wraps the
 * `window.postMessage` protocol used between the shell and each
 * panel so panel code can call typed methods instead of hand-rolling
 * envelopes.
 *
 * ```ts
 * import { host } from '@overlay-engine/client';
 *
 * host.onMessage(msg => console.log('host says:', msg));
 * host.postMessage({ type: 'ready', when: Date.now() });
 * host.requestClose();
 * ```
 */

const PROTOCOL_VERSION = 1;

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

type ShellToPanelInbound =
  | { v: number; type: "shell-client:message"; payload: unknown };

interface HostBridge {
  /**
   * Send a message up to the host application (the Rust process).
   *
   * The host receives it as an `OverlayEvent::PanelMessage { id,
   * payload }` (or as a Tauri event when used through
   * `tauri-plugin-overlay`). Anything JSON-serialisable is fine.
   */
  postMessage(payload: unknown): void;

  /**
   * Subscribe to messages the host sends down to this panel. Returns
   * an unsubscribe function.
   *
   * Multiple subscribers are supported — every listener gets every
   * message in the order they were registered.
   */
  onMessage(listener: (payload: unknown) => void): () => void;

  /**
   * Ask the host to close this panel. The host typically responds by
   * destroying the iframe; the panel can use this from a "close"
   * button so the host can run cleanup (e.g. revoking video tokens).
   */
  requestClose(): void;

  /**
   * Override the panel's interactive hit regions. Coordinates are
   * panel-local pixels (relative to the iframe origin). Pass `null`
   * to revert to the default ("the whole panel iframe is
   * interactive when the panel was created with `interactive: true`").
   *
   * Useful for partially-interactive panels: e.g. a death-replay
   * panel with a clickable timeline at the bottom but a non-
   * interactive header could send `[{ x:0, y:200, w:600, h:40 }]`
   * to let the game handle clicks above the timeline.
   */
  setHitRegions(regions: Rect[] | null): void;
}

const listeners = new Set<(payload: unknown) => void>();

function isInbound(data: unknown): data is ShellToPanelInbound {
  if (!data || typeof data !== "object") return false;
  const m = data as { v?: unknown; type?: unknown };
  return m.v === PROTOCOL_VERSION && m.type === "shell-client:message";
}

window.addEventListener("message", (ev) => {
  if (!isInbound(ev.data)) return;
  for (const listener of listeners) {
    try {
      listener(ev.data.payload);
    } catch (err) {
      console.error("[overlay-engine/client] listener threw", err);
    }
  }
});

function postUp(msg: unknown): void {
  // We always post to `window.parent` because panels are always
  // mounted as children of the shell. `targetOrigin: '*'` is
  // intentional: panels can be served from a different origin than
  // the shell during dev (Vite at :1420 vs the asset server at
  // :NNNNN), and message authenticity is enforced at the shell-host
  // boundary by `chrome.webview` rather than at the iframe boundary.
  window.parent.postMessage(msg, "*");
}

export const host: HostBridge = {
  postMessage(payload) {
    postUp({ v: PROTOCOL_VERSION, type: "panel-client:message", payload });
  },
  onMessage(listener) {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  },
  requestClose() {
    postUp({ v: PROTOCOL_VERSION, type: "panel-client:request-close" });
  },
  setHitRegions(regions) {
    postUp({ v: PROTOCOL_VERSION, type: "panel-client:set-hit-regions", regions });
  },
};

// Announce ourselves to the shell as soon as the script runs. The
// shell uses this to emit `panel:loaded` to the host. Run after a
// microtask so client code has a chance to register listeners
// synchronously after import without missing any host -> panel
// messages that follow `panel:loaded`.
queueMicrotask(() => {
  postUp({ v: PROTOCOL_VERSION, type: "panel-client:ready" });
});

export type { Rect, HostBridge };
