/**
 * overlay-engine default shell.
 *
 * Runs inside the WebView2 composition surface as the only HTML page
 * loaded by the engine. Hosts panel iframes, aggregates their hit
 * regions back to the host, and routes messages between the host
 * (Rust, via `window.chrome.webview`) and individual panels (each
 * iframe, via `iframe.contentWindow.postMessage`).
 *
 * Protocol is documented in `protocol.ts`. Rust mirror lives in
 * `src/overlay/protocol.rs`.
 */

import {
  PROTOCOL_VERSION,
  type HostToShell,
  type PanelToShell,
  type Rect,
  type ShellToHost,
  type ShellToPanel,
} from "./protocol";

interface PanelState {
  id: string;
  iframe: HTMLIFrameElement;
  bounds: Rect;
  interactive: boolean;
  zIndex: number;
  /**
   * Per-panel hit-region override sent up by the panel via
   * `host.setHitRegions(...)`. When `null` we use the iframe's full
   * bounds as a single region (assuming `interactive`); when set
   * (even to `[]`) we use exactly these.
   *
   * Coordinates are panel-local (relative to the iframe origin); we
   * translate to surface coords when aggregating.
   */
  customRegions: Rect[] | null;
  /** Latched once the panel has sent its `panel-client:ready` message. */
  loaded: boolean;
}

type ChromeWebView = {
  postMessage: (data: unknown) => void;
  addEventListener: (type: "message", listener: (ev: { data: unknown }) => void) => void;
};

declare global {
  interface Window {
    chrome?: { webview?: ChromeWebView };
  }
}

const panels = new Map<string, PanelState>();

const webview: ChromeWebView | undefined = window.chrome?.webview;
if (!webview) {
  // Outside a WebView2 host. The shell is still useful for local
  // browser dev (running shell.html in a regular browser, e.g. via
  // `pnpm dev` when iterating panels), but host messages obviously
  // won't flow.
  console.warn("[overlay-shell] window.chrome.webview not present; running in fallback browser mode");
}

// ----- Host (Rust) -> Shell -----

function postToHost(msg: ShellToHost): void {
  if (!webview) {
    console.debug("[overlay-shell] (no webview) host msg:", msg);
    return;
  }
  webview.postMessage(JSON.stringify(msg));
}

function handleHostMessage(raw: unknown): void {
  let msg: HostToShell;
  try {
    msg = typeof raw === "string" ? (JSON.parse(raw) as HostToShell) : (raw as HostToShell);
  } catch (err) {
    console.error("[overlay-shell] failed to parse host message", raw, err);
    return;
  }
  if (!msg || typeof msg !== "object" || msg.v !== PROTOCOL_VERSION) {
    console.warn("[overlay-shell] dropping message with unsupported protocol version", msg);
    return;
  }
  switch (msg.type) {
    case "panel:create":
      createPanel(msg.id, msg.url, msg.bounds, msg.interactive, msg.zIndex);
      break;
    case "panel:close":
      closePanel(msg.id);
      break;
    case "panel:bounds":
      setBounds(msg.id, msg.bounds);
      break;
    case "panel:set-interactive":
      setInteractive(msg.id, msg.interactive);
      break;
    case "panel:set-z-index":
      setZIndex(msg.id, msg.zIndex);
      break;
    case "panel:message":
      sendToPanel(msg.id, msg.payload);
      break;
    case "shell:ping":
      postToHost({ v: PROTOCOL_VERSION, type: "shell:pong" });
      break;
    default: {
      // Exhaustiveness check: a new HostToShell variant must be
      // handled above or this assignment will fail to compile.
      const _exhaustive: never = msg;
      void _exhaustive;
    }
  }
}

if (webview) {
  webview.addEventListener("message", (ev) => handleHostMessage(ev.data));
}

// ----- Panel lifecycle -----

function createPanel(id: string, url: string, bounds: Rect, interactive: boolean, zIndex: number): void {
  if (panels.has(id)) {
    postToHost({
      v: PROTOCOL_VERSION,
      type: "panel:error",
      id,
      error: `panel id "${id}" is already mounted`,
    });
    return;
  }
  const iframe = document.createElement("iframe");
  iframe.className = `panel${interactive ? " panel--interactive" : ""}`;
  iframe.dataset.panelId = id;
  iframe.allow = "autoplay; fullscreen; clipboard-read; clipboard-write";
  applyBounds(iframe, bounds, zIndex);
  iframe.src = url;
  document.body.appendChild(iframe);

  panels.set(id, {
    id,
    iframe,
    bounds,
    interactive,
    zIndex,
    customRegions: null,
    loaded: false,
  });
  recomputeHitRegions();
}

function closePanel(id: string): void {
  const panel = panels.get(id);
  if (!panel) return;
  panel.iframe.remove();
  panels.delete(id);
  recomputeHitRegions();
}

function setBounds(id: string, bounds: Rect): void {
  const panel = panels.get(id);
  if (!panel) return;
  panel.bounds = bounds;
  applyBounds(panel.iframe, bounds, panel.zIndex);
  recomputeHitRegions();
}

function setInteractive(id: string, interactive: boolean): void {
  const panel = panels.get(id);
  if (!panel) return;
  panel.interactive = interactive;
  panel.iframe.classList.toggle("panel--interactive", interactive);
  recomputeHitRegions();
}

function setZIndex(id: string, zIndex: number): void {
  const panel = panels.get(id);
  if (!panel) return;
  panel.zIndex = zIndex;
  panel.iframe.style.zIndex = String(zIndex);
}

function applyBounds(iframe: HTMLIFrameElement, bounds: Rect, zIndex: number): void {
  iframe.style.left = `${bounds.x}px`;
  iframe.style.top = `${bounds.y}px`;
  iframe.style.width = `${bounds.w}px`;
  iframe.style.height = `${bounds.h}px`;
  iframe.style.zIndex = String(zIndex);
}

// ----- Panel iframe -> Shell -----

window.addEventListener("message", (ev) => {
  // Locate which panel this message came from. We can't trust origin
  // alone (multiple panels may share an origin); match by source
  // window reference.
  const panel = findPanelBySource(ev.source);
  if (!panel) return;
  const data = ev.data as PanelToShell;
  if (!data || typeof data !== "object" || data.v !== PROTOCOL_VERSION) {
    return;
  }
  switch (data.type) {
    case "panel-client:ready":
      panel.loaded = true;
      postToHost({ v: PROTOCOL_VERSION, type: "panel:loaded", id: panel.id });
      break;
    case "panel-client:message":
      postToHost({
        v: PROTOCOL_VERSION,
        type: "panel:message-up",
        id: panel.id,
        payload: data.payload,
      });
      break;
    case "panel-client:request-close":
      postToHost({ v: PROTOCOL_VERSION, type: "panel:request-close", id: panel.id });
      break;
    case "panel-client:set-hit-regions":
      panel.customRegions = data.regions;
      recomputeHitRegions();
      break;
    default: {
      const _exhaustive: never = data;
      void _exhaustive;
    }
  }
});

function findPanelBySource(source: MessageEventSource | null): PanelState | null {
  if (!source) return null;
  for (const panel of panels.values()) {
    if (panel.iframe.contentWindow === source) {
      return panel;
    }
  }
  return null;
}

function sendToPanel(id: string, payload: unknown): void {
  const panel = panels.get(id);
  if (!panel || !panel.iframe.contentWindow) return;
  const msg: ShellToPanel = {
    v: PROTOCOL_VERSION,
    type: "shell-client:message",
    payload,
  };
  // We use '*' as targetOrigin because the asset server may serve
  // panels from a different origin than the shell when the host has
  // configured a Vite dev server URL for hot-reload. The shell only
  // forwards messages from panels it created, and panels only get
  // host-relayed traffic, so message authenticity is enforced at
  // the host-shell boundary, not at the iframe boundary.
  panel.iframe.contentWindow.postMessage(msg, "*");
}

// ----- Hit-region aggregation -----

let lastSentRegions = "";

function recomputeHitRegions(): void {
  const regions: Rect[] = [];
  for (const panel of panels.values()) {
    if (!panel.interactive) continue;
    if (panel.customRegions === null) {
      regions.push({ ...panel.bounds });
      continue;
    }
    for (const r of panel.customRegions) {
      regions.push({
        x: panel.bounds.x + r.x,
        y: panel.bounds.y + r.y,
        w: r.w,
        h: r.h,
      });
    }
  }
  // Cheap dedup: only message the host when the set actually
  // changes. Stringifying ~10 rects on bounds updates is fine; this
  // saves a postMessage per pixel during drags.
  const serialized = JSON.stringify(regions);
  if (serialized === lastSentRegions) return;
  lastSentRegions = serialized;
  postToHost({ v: PROTOCOL_VERSION, type: "shell:hit-regions", regions });
}

// ----- Bootstrap -----

postToHost({ v: PROTOCOL_VERSION, type: "shell:ready" });
