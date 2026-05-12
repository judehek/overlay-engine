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
  /**
   * Host-bound payloads received before the panel's iframe document
   * had a `message` listener attached. We buffer them until the
   * panel posts `panel-client:ready` (which is what `@overlay-engine/
   * client` sends after registering its window-message listener),
   * then flush in order. Without this, any host-driven message that
   * races iframe load is silently lost — `window.postMessage` to a
   * window with no listener simply discards the event per HTML spec.
   */
  pendingMessages: unknown[];
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
  // Top-frame escape hatch. URLs prefixed with `topframe:` are
  // navigated as the WebView's top-level document instead of being
  // mounted as an iframe — this is the only way to render sites
  // that ship `X-Frame-Options: DENY` or `Content-Security-Policy:
  // frame-ancestors` (Instagram, Twitter, etc.). After this point
  // the shell is gone, so subsequent host messages (panel:create,
  // panel:close, hit-region updates) won't reach anything; the host
  // is expected to detach the engine entirely to tear the panel
  // down and re-attach to show a different one. Before navigating
  // we tell the host the full surface is interactive so clicks /
  // scrolls reach the new top-frame document instead of falling
  // through to the game.
  if (url.startsWith("topframe:")) {
    const realUrl = url.slice("topframe:".length);
    postToHost({
      v: PROTOCOL_VERSION,
      type: "shell:hit-regions",
      regions: [{ x: 0, y: 0, w: window.innerWidth, h: window.innerHeight }],
    });
    postToHost({ v: PROTOCOL_VERSION, type: "panel:loaded", id });
    window.location.href = realUrl;
    return;
  }
  const iframe = document.createElement("iframe");
  iframe.className = `panel${interactive ? " panel--interactive" : ""}`;
  iframe.dataset.panelId = id;
  iframe.allow = "autoplay; fullscreen; clipboard-read; clipboard-write";
  applyBounds(iframe, bounds, zIndex);
  // Diagnostic: surface iframe element-level load lifecycle through the
  // existing panel:error channel. The "[iframe-diag]" prefix lets the
  // host grep these out from real panel errors. A `load` event fires
  // for every navigation including error pages, so an absence of this
  // log alongside a missing panel:loaded indicates the iframe never
  // even fired a navigation completion (most likely an AV/firewall
  // blocking the loopback fetch from inside the game process).
  iframe.addEventListener("load", () => {
    // Probe the iframe content to distinguish "our HTML loaded" from
    // "Chromium rendered an error page." Error pages live on
    // chrome-error://chromewebdata/ — a different origin from the
    // shell — so any contentDocument access throws SecurityError.
    // If accessible, we surface title/URL/#app presence to confirm
    // it's actually game-notification.html.
    let info = `load fired for ${url}`;
    try {
      const doc = iframe.contentDocument;
      if (!doc) {
        info += " | contentDocument=null (likely cross-origin error page)";
      } else {
        const title = doc.title ?? "";
        const docUrl = doc.URL ?? "";
        const appPresent = !!doc.getElementById("app");
        const scripts = doc.querySelectorAll("script").length;
        const bodyText = (doc.body?.textContent ?? "").trim().slice(0, 120);
        info += ` | title="${title}" url=${docUrl} appDiv=${appPresent} scripts=${scripts} bodyTextSample="${bodyText}"`;
      }
    } catch (e) {
      info += ` | contentDocument access threw: ${(e as Error).message ?? e}`;
    }
    postToHost({
      v: PROTOCOL_VERSION,
      type: "panel:error",
      id,
      error: `[iframe-diag] ${info}`,
    });
  });
  iframe.addEventListener("error", (ev) => {
    postToHost({
      v: PROTOCOL_VERSION,
      type: "panel:error",
      id,
      error: `[iframe-diag] error event: ${(ev as ErrorEvent).message ?? "unknown"} for ${url}`,
    });
  });
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
    pendingMessages: [],
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
      // Flush anything we buffered while the panel was loading. Order
      // is preserved (FIFO) so the panel sees messages in the same
      // sequence the host sent them.
      if (panel.pendingMessages.length > 0) {
        const drained = panel.pendingMessages;
        panel.pendingMessages = [];
        for (const payload of drained) {
          deliverToPanel(panel, payload);
        }
      }
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
  if (!panel) return;
  // Iframe load (and its panel-client subscribing to `message`
  // events) is asynchronous, but the host has no way to know when
  // an arbitrary panel becomes ready — `panel:create` resolves as
  // soon as the engine accepts the request, well before the iframe
  // document boots. Buffer any host-bound messages here and flush
  // when `panel-client:ready` comes back, otherwise messages that
  // race iframe load are silently dropped (the iframe's window has
  // no `message` listener yet so the dispatched event has nowhere
  // to go).
  if (!panel.loaded) {
    panel.pendingMessages.push(payload);
    return;
  }
  deliverToPanel(panel, payload);
}

function deliverToPanel(panel: PanelState, payload: unknown): void {
  if (!panel.iframe.contentWindow) return;
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
