// src/index.ts
var PROTOCOL_VERSION = 1;
var listeners = /* @__PURE__ */ new Set();
function isInbound(data) {
  if (!data || typeof data !== "object") return false;
  const m = data;
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
function postUp(msg) {
  window.parent.postMessage(msg, "*");
}
var host = {
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
  }
};
queueMicrotask(() => {
  postUp({ v: PROTOCOL_VERSION, type: "panel-client:ready" });
});
export {
  host
};
//# sourceMappingURL=index.js.map
