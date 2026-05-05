# @overlay-engine/client

Client SDK for code running inside an [overlay-engine](https://github.com/judehek/overlay-engine) panel iframe.

```bash
pnpm add @overlay-engine/client
```

```ts
import { host } from "@overlay-engine/client";

host.onMessage((msg) => console.log("host says:", msg));
host.postMessage({ type: "ready", when: Date.now() });

closeButton.addEventListener("click", () => host.requestClose());
```

See the [overlay-engine README](https://github.com/judehek/overlay-engine) for the host-side API and the panel mounting model.
