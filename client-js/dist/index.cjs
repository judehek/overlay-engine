"use strict";
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};
var __copyProps = (to, from, except, desc) => {
  if (from && typeof from === "object" || typeof from === "function") {
    for (let key of __getOwnPropNames(from))
      if (!__hasOwnProp.call(to, key) && key !== except)
        __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
  }
  return to;
};
var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);

// src/index.ts
var src_exports = {};
__export(src_exports, {
  host: () => host
});
module.exports = __toCommonJS(src_exports);
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
//# sourceMappingURL=index.cjs.map
