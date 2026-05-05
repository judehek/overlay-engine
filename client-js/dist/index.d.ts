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
interface Rect {
    x: number;
    y: number;
    w: number;
    h: number;
}
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
export declare const host: HostBridge;
export type { Rect, HostBridge };
