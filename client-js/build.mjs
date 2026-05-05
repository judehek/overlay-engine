// Build script for `@overlay-engine/client`.
//
// Emits ESM + CJS bundles to `dist/`. Type declarations are emitted
// separately by `tsc --emitDeclarationOnly` (see package.json
// `build` script).

import * as esbuild from "esbuild";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const watch = process.argv.includes("--watch");

const baseOpts = {
  entryPoints: [`${root}/src/index.ts`],
  bundle: true,
  target: "es2020",
  sourcemap: true,
  legalComments: "none",
};

async function buildAll() {
  await Promise.all([
    esbuild.build({ ...baseOpts, format: "esm", outfile: `${root}/dist/index.js` }),
    esbuild.build({ ...baseOpts, format: "cjs", outfile: `${root}/dist/index.cjs` }),
  ]);
  console.log("[overlay-engine/client] built dist/index.{js,cjs}");
}

if (watch) {
  const ctx1 = await esbuild.context({ ...baseOpts, format: "esm", outfile: `${root}/dist/index.js` });
  const ctx2 = await esbuild.context({ ...baseOpts, format: "cjs", outfile: `${root}/dist/index.cjs` });
  await Promise.all([ctx1.watch(), ctx2.watch()]);
  console.log("[overlay-engine/client] watching for changes...");
} else {
  await buildAll();
}
